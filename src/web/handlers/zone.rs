//! Zone control handlers: `POST /zone/:id/*`.

use axum::extract::{Form, Path, State};
use axum::response::Html;

use airtouch5::types::Temperature;
use airtouch5::types::control::ZonePower;

use crate::manager::command::{Command, ZoneControlReq};
use crate::manager::snapshot::{
    BulkModeView, clamp_setpoint, parse_airflow, parse_setpoint, temp_to_f32,
};
use crate::templates;
use crate::web::error::AppError;
use crate::web::state::AppState;

/// `POST /zone/:id/power` -- form field `power = on | off | turbo | toggle`.
pub async fn power(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let p = field(&form, "power");
    let zpower = match p.as_str() {
        "on" => ZonePower::On,
        "off" => ZonePower::Off,
        "turbo" => ZonePower::Turbo,
        "toggle" => ZonePower::Toggle,
        other => return Err(AppError::msg(format!("unknown power: {other:?}"))),
    };
    send_zone(state.manager.clone(), id, ZoneControlReq::Power(zpower)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/control-type` -- form field `type = airflow | temperature`.
///
/// Rather than sending a control-type-only message (which the console silently
/// ignores -- the request returns 200 but the zone stays in its old mode, so
/// the UI never updates), we switch mode by sending an absolute value in the
/// target mode: `SetAirflow(current_pct)` for airflow, `SetTemperature(t)` for
/// temperature. This is the same trick the bulk presets use and it is what the
/// console actually honours -- and, crucially, neither sets the power field, so
/// switching an OFF zone's mode does not power it on. Temperature control is
/// rejected (422) for sensorless zones (a protocol constraint).
pub async fn control_type(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let t = field(&form, "type");
    let snap = state.manager.snapshot_rx.borrow().clone();
    let zone = snap
        .zones
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("zone {id} not found")))?;
    let req = match t.as_str() {
        "airflow" => ZoneControlReq::SetAirflow(zone.airflow_pct),
        "temperature" => {
            if !zone.has_sensor {
                return Err(AppError::msg(
                    "zone has no sensor; cannot temperature-control",
                ));
            }
            // Keep the zone's existing setpoint when re-entering temperature
            // mode; fall back to a neutral 20.0 C when switching over from
            // airflow mode (which carries no setpoint).
            let target = zone.setpoint.and_then(temp_to_f32).unwrap_or(20.0);
            ZoneControlReq::SetTemperature(Temperature::from_float(clamp_setpoint(target)))
        }
        other => return Err(AppError::msg(format!("unknown control type: {other:?}"))),
    };
    send_zone(state.manager.clone(), id, req).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/control-type/toggle` -- no form fields.
///
/// Switches the zone to the opposite control mode: airflow -> temperature or
/// temperature -> airflow. This is the single tap target for the zone row's
/// setpoint value button (which doubles as the %/C mode switch). Temperature
/// is rejected (422) for sensorless zones, so for those the button is rendered
/// disabled and this handler is never reached in practice.
pub async fn toggle_control_type(
    State(state): State<AppState>,
    Path(id): Path<u8>,
) -> Result<Html<String>, AppError> {
    let snap = state.manager.snapshot_rx.borrow().clone();
    let zone = snap
        .zones
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("zone {id} not found")))?;
    let t = if zone.is_temp() { "airflow" } else { "temperature" };
    // Reuse the explicit control-type logic so setpoint fallbacks, sensor
    // rejection, and the no-power-field behaviour stay in one place.
    let form = vec![("type".to_string(), t.to_string())];
    control_type(State(state), Path(id), Form(form)).await
}

/// `POST /zone/:id/step` -- form field `dir = up | down`.
///
/// The +/- stepper steps the zone's value in its current control mode: +/- 5%
/// airflow (clamped 0-100) or +/- 1.0 C setpoint (clamped 10.0-25.0). We
/// compute the target server-side and send it as an absolute `SetAirflow` /
/// `SetTemperature` rather than the protocol's `Increment`/`Decrement` opcode.
/// The opcode form powers an OFF zone on (the console treats a relative step
/// as "the user wants to interact, turn it on"), which silently breaks the
/// "adjust an off zone without waking it" expectation. Absolute values carry no
/// power field, so an OFF zone stays off while its value still updates -- the
/// same property the bulk presets rely on.
pub async fn step(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let dir = field(&form, "dir");
    let up = match dir.as_str() {
        "up" => true,
        "down" => false,
        other => return Err(AppError::msg(format!("unknown dir: {other:?}"))),
    };
    let snap = state.manager.snapshot_rx.borrow().clone();
    let zone = snap
        .zones
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("zone {id} not found")))?;
    let req = if zone.is_temp() {
        let cur = zone.setpoint.and_then(temp_to_f32).unwrap_or(20.0);
        let target = clamp_setpoint(if up { cur + 1.0 } else { cur - 1.0 });
        ZoneControlReq::SetTemperature(Temperature::from_float(target))
    } else {
        let cur = zone.airflow_pct as i16;
        let target = (cur + if up { 5 } else { -5 }).clamp(0, 100) as u8;
        ZoneControlReq::SetAirflow(target)
    };
    send_zone(state.manager.clone(), id, req).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/airflow` -- form field `pct = 0..100`.
pub async fn airflow(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let pct = parse_airflow(&field(&form, "pct"))?;
    send_zone(state.manager.clone(), id, ZoneControlReq::SetAirflow(pct)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zone/:id/setpoint` -- form field `temp = 10.0..25.0`.
pub async fn setpoint(
    State(state): State<AppState>,
    Path(id): Path<u8>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let t = parse_setpoint(&field(&form, "temp"))?;
    send_zone(state.manager.clone(), id, ZoneControlReq::SetTemperature(t)).await?;
    render_current_zone(&state.manager, id)
}

/// `POST /zones/power` -- form field `power = on | off`.
///
/// Turns every zone on or off in one shot. Unlike the per-zone power toggle
/// (which supports `turbo`/`toggle`), the bulk bar only exposes the two
/// states that make sense for "all zones": plain on and plain off.
pub async fn set_all_power(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let power = field(&form, "power");
    let power = match power.as_str() {
        "on" => ZonePower::On,
        "off" => ZonePower::Off,
        other => return Err(AppError::msg(format!("unknown power: {other:?}"))),
    };

    let snap = state.manager.snapshot_rx.borrow().clone();
    let manager = state.manager.clone();
    for (&id, zone) in &snap.zones {
        // Skip zones that are already in the target state: re-sending the
        // command would be wasted traffic and, for some consoles, would
        // reset a Turbo timer the user may have just set.
        let already = match power {
            ZonePower::On => zone.is_on(),
            ZonePower::Off => matches!(zone.power, crate::manager::snapshot::ZonePowerView::Off),
            _ => false,
        };
        if already {
            continue;
        }
        send_zone(manager.clone(), id, ZoneControlReq::Power(power)).await?;
    }

    // Preserve the user's last bulk-mode selection on the bar.
    let bulk_mode = state.manager.snapshot_rx.borrow().bulk_mode();
    let snap = state.manager.snapshot_rx.borrow().clone();
    Ok(Html(templates::render_zones_with_bulk(&snap, bulk_mode)))
}

/// `POST /zones/preset` -- form fields `mode = airflow | temperature` and
/// `value` (an airflow percentage `0..100` or a temperature setpoint
/// `10.0..25.0`). Sets every zone to that value in the given mode. Temperature
/// is applied only to sensor-equipped zones (and forces temperature mode for
/// them); airflow is applied to every zone. Re-renders the whole zones partial
/// keeping the requested mode active on the bulk bar.
pub async fn set_all_preset(
    State(state): State<AppState>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Html<String>, AppError> {
    let mode = field(&form, "mode");
    let value = field(&form, "value");

    let snap = state.manager.snapshot_rx.borrow().clone();
    let manager = state.manager.clone();
    let bulk_mode = match mode.as_str() {
        "airflow" => {
            let pct = parse_airflow(&value)?;
            for &id in snap.zones.keys() {
                send_zone(manager.clone(), id, ZoneControlReq::SetAirflow(pct)).await?;
            }
            BulkModeView::Airflow
        }
        "temperature" => {
            let t = parse_setpoint(&value)?;
            for (&id, zone) in &snap.zones {
                if !zone.has_sensor {
                    continue;
                }
                send_zone(manager.clone(), id, ZoneControlReq::SetTemperature(t)).await?;
            }
            BulkModeView::Temperature
        }
        other => return Err(AppError::msg(format!("unknown mode: {other:?}"))),
    };

    let snap = state.manager.snapshot_rx.borrow().clone();
    Ok(Html(templates::render_zones_with_bulk(&snap, bulk_mode)))
}

async fn send_zone(
    manager: crate::manager::ManagerHandle,
    id: u8,
    req: ZoneControlReq,
) -> Result<(), AppError> {
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

fn render_current_zone(
    manager: &crate::manager::ManagerHandle,
    id: u8,
) -> Result<Html<String>, AppError> {
    let snap = manager.snapshot_rx.borrow().clone();
    let zone = snap
        .zones
        .get(&id)
        .ok_or_else(|| AppError::msg(format!("zone {id} not found")))?;
    Ok(Html(templates::render_zone(zone)))
}

fn field(form: &[(String, String)], key: &str) -> String {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}
