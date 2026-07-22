//! Askama template struct definitions.
//!
//! Each struct maps to one template file under `templates/`. Fragment templates
//! render a single root element carrying a stable `id` plus `sse-swap` /
//! `hx-swap="outerHTML"` so the htmx-sse extension can swap them in place.

use askama::Template;

use crate::automation::{AutomationConfig, IdleOffStatus, SetpointOffStatus};
use crate::manager::snapshot::{AcView, BulkModeView, Snapshot, ZoneView};
use crate::scenes::SceneConfig;
use crate::web::theme::{THEMES, Theme};

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate<'a> {
    pub snapshot: &'a Snapshot,
    pub bulk_mode: BulkModeView,
    pub config: &'a AutomationConfig,
    pub status: &'a SetpointOffStatus,
    pub idle: &'a IdleOffStatus,
    /// Saved presets plus whether one currently matches live state (for the
    /// Presets card the index includes above the AC units).
    pub presets: Vec<PresetRow>,
    pub has_active: bool,
    /// The active theme (from the `theme` cookie) and every available theme,
    /// for <html data-theme> and the footer theme selector.
    pub theme: &'static Theme,
    pub themes: &'static [Theme],
}

pub fn render_index(
    snapshot: &Snapshot,
    config: &AutomationConfig,
    status: &SetpointOffStatus,
    idle: &IdleOffStatus,
    scenes: &SceneConfig,
    theme: &'static Theme,
) -> String {
    let (presets, has_active) = build_preset_rows(scenes, snapshot);
    IndexTemplate {
        snapshot,
        bulk_mode: snapshot.bulk_mode(),
        config,
        status,
        idle,
        presets,
        has_active,
        theme,
        themes: THEMES,
    }
    .render()
    .unwrap_or_default()
}

/// One row in the Presets card: a preset name and whether it currently matches
/// the live state (so its tile is highlighted and Remove targets it).
pub struct PresetRow {
    pub name: String,
    pub active: bool,
}

#[derive(Template)]
#[template(path = "partials/presets.html")]
pub struct PresetsTemplate {
    pub presets: Vec<PresetRow>,
    pub has_active: bool,
}

/// Build the preset rows plus the "some preset is active" flag from the saved
/// config and the live snapshot.
fn build_preset_rows(cfg: &SceneConfig, snap: &Snapshot) -> (Vec<PresetRow>, bool) {
    let active = cfg.active_name(snap);
    let rows = cfg
        .scenes
        .iter()
        .map(|s| PresetRow {
            name: s.name.clone(),
            active: Some(&s.name) == active.as_ref(),
        })
        .collect();
    (rows, active.is_some())
}

/// Render the presets card fragment (`#presets`).
pub fn render_presets(cfg: &SceneConfig, snap: &Snapshot) -> String {
    let (presets, has_active) = build_preset_rows(cfg, snap);
    PresetsTemplate {
        presets,
        has_active,
    }
    .render()
    .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "partials/connection_state.html")]
pub struct ConnectionStateTemplate<'a> {
    pub snapshot: &'a Snapshot,
}

#[derive(Template)]
#[template(path = "partials/system.html")]
pub struct SystemTemplate<'a> {
    pub snapshot: &'a Snapshot,
}

#[derive(Template)]
#[template(path = "partials/acs.html")]
pub struct AcsTemplate<'a> {
    pub snapshot: &'a Snapshot,
}

#[derive(Template)]
#[template(path = "partials/ac.html")]
pub struct AcTemplate<'a> {
    pub ac: &'a AcView,
}

#[derive(Template)]
#[template(path = "partials/zones.html")]
pub struct ZonesTemplate<'a> {
    pub snapshot: &'a Snapshot,
    pub bulk_mode: BulkModeView,
}

#[derive(Template)]
#[template(path = "partials/zone.html")]
pub struct ZoneTemplate<'a> {
    pub zone: &'a ZoneView,
}

#[derive(Template)]
#[template(path = "partials/automation.html")]
pub struct AutomationTemplate<'a> {
    pub config: &'a AutomationConfig,
    pub status: &'a SetpointOffStatus,
    pub idle: &'a IdleOffStatus,
}

/// Render a fragment to a String for use as an SSE `data:` payload or a POST
/// response body.
pub fn render_zone(zone: &ZoneView) -> String {
    ZoneTemplate { zone }.render().unwrap_or_default()
}

pub fn render_ac(ac: &AcView) -> String {
    AcTemplate { ac }.render().unwrap_or_default()
}

pub fn render_system(snapshot: &Snapshot) -> String {
    SystemTemplate { snapshot }.render().unwrap_or_default()
}

pub fn render_connection_state(snapshot: &Snapshot) -> String {
    ConnectionStateTemplate { snapshot }
        .render()
        .unwrap_or_default()
}

pub fn render_acs(snapshot: &Snapshot) -> String {
    AcsTemplate { snapshot }.render().unwrap_or_default()
}

pub fn render_zones(snapshot: &Snapshot) -> String {
    render_zones_with_bulk(snapshot, snapshot.bulk_mode())
}

/// Render the zones partial with an explicit bulk-bar mode. Used by the bulk
/// control-type / preset POST handlers so the bar reflects the user's last
/// selection rather than only the live zone states.
pub fn render_zones_with_bulk(snapshot: &Snapshot, bulk_mode: BulkModeView) -> String {
    ZonesTemplate {
        snapshot,
        bulk_mode,
    }
    .render()
    .unwrap_or_default()
}

/// Render the automation programs configuration partial (`#automation`).
pub fn render_automation(
    config: &AutomationConfig,
    status: &SetpointOffStatus,
    idle: &IdleOffStatus,
) -> String {
    AutomationTemplate {
        config,
        status,
        idle,
    }
    .render()
    .unwrap_or_default()
}
