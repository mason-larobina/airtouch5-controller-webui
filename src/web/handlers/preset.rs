//! Preset handlers: `GET /partials/presets` and `POST /presets*`.
//!
//! Presets are named, full-state captures. Save records the current live state
//! under a user-supplied name; apply replays a saved preset through the normal
//! command path (so the mock's derived-field syncing stays correct); remove
//! deletes whichever preset currently matches the live state. Each handler
//! re-renders the `#presets` partial; the AC/zone cards an apply touches update
//! separately over SSE.

use axum::extract::{Form, State};
use axum::response::Html;

use airtouch5::types::Temperature;
use airtouch5::types::control::{AcMode, AcPower, FanSpeed, ZonePower};

use crate::manager::ManagerHandle;
use crate::manager::command::{AcControlReq, Command, ZoneControlReq};
use crate::manager::snapshot::clamp_setpoint;
use crate::scenes::{Scene, capture_scene};
use crate::templates;
use crate::web::error::AppError;
use crate::web::state::AppState;

/// `GET /partials/presets` -- the presets card.
pub async fn partial_presets(State(state): State<AppState>) -> Html<String> {
    render(&state)
}

/// `POST /presets` -- form field `name`. Captures the current live state under
/// `name` (overwriting any preset with the same name). An empty name (the user
/// cancelled the browser prompt) is a no-op re-render.
pub async fn save(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let name = sanitize_name(&field(&form, "name"));
    if !name.is_empty() {
        let snap = state.manager.snapshot_rx.borrow().clone();
        state
            .scenes
            .upsert(capture_scene(name, &snap))
            .map_err(AppError::msg)?;
    }
    Ok(render(&state))
}

/// `POST /presets/apply` -- form field `name`. Replays the named preset.
pub async fn apply(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let name = field(&form, "name");
    let scene = state
        .scenes
        .get()
        .scenes
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| AppError::msg(format!("preset {name:?} not found")))?;
    apply_scene(&state.manager, &scene).await?;
    Ok(render(&state))
}

/// `POST /presets/remove` -- removes whichever preset currently matches the
/// live state (the "selected" one). A no-op when nothing matches.
pub async fn remove(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    if let Some(name) = state.scenes.get().active_name(&snap) {
        state.scenes.remove(&name).map_err(AppError::msg)?;
    }
    Ok(render(&state))
}

/// Replay a preset through the normal command path.
///
/// Ordered in three phases:
///
/// 1. **AC mode / fan / setpoint first.** A zone row is tinted by its owning
///    AC's operating mode (`ac_mode_slug` -> `data-ac-mode`). Applying the mode
///    up front means every zone fragment emitted afterwards already carries the
///    final colour, so the live SSE stream never pushes a zone painted in the
///    *old* mode. Emitting a stale-colour fragment mid-apply can leave a zone
///    stuck on the wrong colour in the browser if the correcting `outerHTML`
///    swap races with an earlier one.
/// 2. **Zones** (absolute value, which carries no power field so it sets the
///    control mode without waking the zone, then the power state).
/// 3. **AC power last**, so an AC that should power on has at least one open
///    zone (the console rejects running a unit with no airflow path).
async fn apply_scene(manager: &ManagerHandle, scene: &Scene) -> Result<(), AppError> {
    let snap = manager.snapshot_rx.borrow().clone();

    // Phase 1: AC mode/fan/setpoint (not power) so zones tint correctly below.
    for a in &scene.acs {
        if !snap.acs.contains_key(&a.id) {
            continue;
        }
        if let Some(mode) = slug_to_mode(&a.mode) {
            send_ac(manager, a.id, AcControlReq::Mode(mode)).await?;
        }
        if let Some(fan) = slug_to_fan(&a.fan) {
            send_ac(manager, a.id, AcControlReq::FanSpeed(fan)).await?;
        }
        if let Some(t) = a.setpoint_c {
            send_ac(
                manager,
                a.id,
                AcControlReq::Setpoint(Temperature::from_float(clamp_setpoint(t))),
            )
            .await?;
        }
    }

    // Phase 2: zones. The AC mode is already set, so each zone's first emitted
    // fragment carries the final colour.
    for z in &scene.zones {
        let Some(live) = snap.zones.get(&z.id) else {
            continue;
        };
        // Temperature control needs a sensor; fall back to airflow otherwise.
        let value_req = match (z.control_mode.as_str(), z.setpoint_c) {
            ("temperature", Some(t)) if live.has_sensor => {
                ZoneControlReq::SetTemperature(Temperature::from_float(clamp_setpoint(t)))
            }
            _ => ZoneControlReq::SetAirflow(z.airflow_pct),
        };
        send_zone(manager, z.id, value_req).await?;
        let power = if z.enabled {
            ZonePower::On
        } else {
            ZonePower::Off
        };
        send_zone(manager, z.id, ZoneControlReq::Power(power)).await?;
    }

    // Phase 3: AC power, after zones are open.
    for a in &scene.acs {
        if !snap.acs.contains_key(&a.id) {
            continue;
        }
        let power = if a.power { AcPower::On } else { AcPower::Off };
        send_ac(manager, a.id, AcControlReq::Power(power)).await?;
    }

    Ok(())
}

/// Map an AC mode slug back to a control enum (unknown/empty -> skip).
fn slug_to_mode(slug: &str) -> Option<AcMode> {
    match slug {
        "auto" => Some(AcMode::Auto),
        "heat" => Some(AcMode::Heat),
        "dry" => Some(AcMode::Dry),
        "fan" => Some(AcMode::Fan),
        "cool" => Some(AcMode::Cool),
        _ => None,
    }
}

/// Map a fan speed slug back to a control enum (unknown/empty -> skip).
fn slug_to_fan(slug: &str) -> Option<FanSpeed> {
    match slug {
        "auto" => Some(FanSpeed::Auto),
        "quiet" => Some(FanSpeed::Quiet),
        "low" => Some(FanSpeed::Low),
        "medium" => Some(FanSpeed::Medium),
        "high" => Some(FanSpeed::High),
        "powerful" => Some(FanSpeed::Powerful),
        "turbo" => Some(FanSpeed::Turbo),
        "intelligentauto" => Some(FanSpeed::IntelligentAuto),
        _ => None,
    }
}

/// Strip characters that would break the name inside the `hx-vals` JSON
/// attribute (double quote, backslash, control chars), trim, and cap length.
fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| *c != '"' && *c != '\\' && !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(60)
        .collect()
}

fn render(state: &AppState) -> Html<String> {
    let cfg = state.scenes.get();
    let snap = state.manager.snapshot_rx.borrow().clone();
    Html(templates::render_presets(&cfg, &snap))
}

async fn send_ac(manager: &ManagerHandle, id: u8, req: AcControlReq) -> Result<(), AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(Command::ControlAc { id, req, reply: tx })
        .await
        .map_err(|_| AppError::msg("manager stopped"))?;
    rx.await
        .map_err(|_| AppError::msg("manager dropped reply"))?
        .map_err(AppError::msg)
}

async fn send_zone(manager: &ManagerHandle, id: u8, req: ZoneControlReq) -> Result<(), AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    manager
        .cmd_tx
        .send(Command::ControlZone { id, req, reply: tx })
        .await
        .map_err(|_| AppError::msg("manager stopped"))?;
    rx.await
        .map_err(|_| AppError::msg("manager dropped reply"))?
        .map_err(AppError::msg)
}

fn field(form: &[(String, String)], key: &str) -> String {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}
