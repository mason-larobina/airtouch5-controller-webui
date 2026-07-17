# DESIGN — `aircon` AirTouch 5 web UI

A small web server wrapping the [`airtouch5`](https://codeberg.org/kbriggs/airtouch5)
crate: discovers the local AirTouch 5 console, exposes its state, and lets you
control AC units and zones from a browser UI built with **htmx** and live-updated
via **htmx-sse** (Server-Sent Events). Server-rendered HTML fragments are emitted
over SSE and swapped in-place by the browser — no client-side JS framework.

> Stack decisions (confirmed):
> - **Server:** `axum`
> - **Templating:** `askama` (compile-time Jinja-like `.html` templates)
> - **Live updates:** SSE delivering **HTML fragments** swapped via `hx-sse`
> - **Console discovery:** UDP auto-discovery only
>   (`airtouch5::discovery::discover_timeout`)

---

## 1. Goals & scope

**In scope (phase 1):**
- Discover the AirTouch 5 console on the LAN and connect.
- Show system status (console name/address/id, firmware versions, AC count, zone count, connection state).
- Enumerate AC units (with capabilities) and zones (with names).
- Show live AC status (power, mode, fan speed, setpoint, current temp, flags, errors).
- Show live zone status (power, control mode = % airflow or temperature, current airflow %, sensor temp, setpoint, flags).
- Zone control:
  - Power: on / off / turbo.
  - Switch zone control mode between **airflow %** and **temperature setpoint** (only for zones with a sensor).
  - Adjust value: increment / decrement, set airflow % directly, set temperature setpoint.
- Live updates pushed to all connected browsers over SSE.

**Explicitly out of scope (phase 1):**
- AC unit control UI (power/mode/fan/setpoint) — designed but lower priority; see §8.
- Authentication, multi-user, multi-console selection.
- Persisting/caching the discovered console address across restarts.
- Timers/schedules.

---

## 2. Tech stack & dependencies

```toml
[dependencies]
airtouch5 = { version = "0.2", features = ["control"] }
# `timeout` (default) stays on; `control` enables ZoneControl/AcControl + control_zone/control_ac.
axum = "0.8"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal", "time", "sync"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["fs", "trace"] }   # static assets + logs
askama = "0.12"                                                 # templates
askama_axum = "0.4"                                              # axum IntoResponse for askama
tracing = "0.1"
tracing-subscriber = "0.3"
futures-util = "0.3"                                            # for SSE stream combinators
```

Vendor **latest htmx** (2.x) and the **`sse` extension** under **versioned,
un-minified** paths in `static/vendor/`, e.g.:

```
static/vendor/htmx/2.0.4/htmx.js               # un-minified, readable
static/vendor/htmx-ext-sse/2.0.4/sse.js        # from the htmx-extensions repo
```

Serve them via `tower_http::services::ServeDir` mounted at `/vendor` with a
**long-expiry immutable cache** — the versioned paths make this safe:
`Cache-Control: public, max-age=31536000, immutable`. Reference the exact versioned
path in `<script src=...>` so a version bump is a cache-bust.

> **htmx SSE note:** SSE-driven fragment swapping needs the `sse` extension
> (`hx-ext="sse"`, `sse-connect`, `sse-swap`) loaded *after* core htmx.

---

## 3. Architecture

```
                ┌──────────────────────────────────────────────┐
                │              tokio runtime (multi-thread)       │
                │                                                │
   ┌────────────┴───────────┐        ┌──────────────────────────┴────┐
   │  connection manager     │        │           axum router          │
   │  (owns AirTouch5)       │        │                                │
   │                         │  cmd   │  GET  /            index.html  │
   │  • discover + connect   │◀───────│  GET  /partials/*  fragments  │
   │  • prefill status       │        │  GET  /events      SSE stream  │
   │  • watch CurrentStatus  │        │  POST /zone/:id/... controls   │
   │  • apply commands       │  reply │  POST /ac/:id/...  (phase 2)   │
   │  • reconnect loop       │───────▶│                                │
   │                         │        │  handlers clone ManagerHandle  │
   │  publishes Snapshot via │        └────────────────────────────────┘
   │  watch::Sender<Snapshot>│                     ▲
   │  + broadcast for SSE    │─────────────────────┘
   └─────────────────────────┘   (handlers read current Snapshot,
                                  send Commands, render fragments)
```

### 3.1 Why an actor for the connection

`AirTouch5` owns a spawned I/O task (`JoinHandle`) and `oneshot::Sender`, so it is
**not `Clone`**, and we should not share the raw struct across request handlers.
Instead, **one long-lived task owns the `AirTouch5`**. The web layer talks to it
through:

- `ManagerHandle.snapshot_rx: tokio::sync::watch::Receiver<Snapshot>` — read-only
  current state for rendering. Cloneable; perfect fan-out for many SSE clients.
- `ManagerHandle.cmd_tx: mpsc::Sender<Command>` — request a control action; reply
  via an embedded `oneshot`.
- `ManagerHandle.updates: tokio::sync::watch::Sender<()>` (or `broadcast`) — a
  lightweight "snapshot changed" tick that SSE endpoints await.

`ManagerHandle` is `Clone` and stored in axum state (`Arc<ManagerHandle>`).

### 3.2 Connection manager loop

```text
loop:
  1. discover_timeout(Some(3s))  →  Console { address, name, airtouch_id, console_id }
       (on failure: log, backoff (1s → 30s exp), retry)
  2. AirTouch5::with_ipaddr(console.address)  →  handle io errors same as step 1
  3. one-shot queries (concurrent via try_join):
        • ac_capabilities()      → names, supported modes/fans, setpoint ranges, zone ranges
        • zone_names()          → BTreeMap<u8, String>
        • console_version()     → versions[], update_available
  4. prefill live state:
        • ac_status(), zone_status()   (these also prime the internal watch)
  5. subscribe_status()  → watch::Receiver<CurrentStatus>
     subscribe_changes() → Option<broadcast::Receiver<StatusChange>>   (optional; the
     CurrentStatus watch already merges these, so changes() is only for low-latency tick)
  6. main select!:
        - cmd_rx.recv()      → apply Command on &AirTouch5; reply Result<()>
        - status_rx.changed()→ rebuild Snapshot (merge static info + CurrentStatus), set watch
        - status_rx error   → connection lost → goto step 1 (reconnect)
        - shutdown signal    → break
  7. on drop: AirTouch5::shutdown() (or just drop → io_loop is signaled to stop)
```

During disconnect, the last-known `Snapshot` is preserved but `connected = false`;
SSE clients receive a `system` event reflecting the disconnected state.

### 3.3 Snapshot

The canonical, `Clone + Send + Sync` state struct the web layer renders from. It is
**our own type** (not `airtouch5`'s, since the crate's `CurrentStatus` has private
fields and no name/capability data). We map the crate's types into it.

```rust
#[derive(Clone, PartialEq)]   // PartialEq powers per-id SSE dirty diffing (§5)
struct Snapshot {
    connected: bool,
    console:   ConsoleInfo,                 // static, from discovery + console_version
    acs:       BTreeMap<u8, AcView>,        // caps (static) + live status
    zones:     BTreeMap<u8, ZoneView>,      // names (static) + live status + owning ac
    updated_at: Option<std::time::Instant>,
}

struct ConsoleInfo {
    name: String, address: IpAddr,
    airtouch_id: u32, console_id: String,
    versions: Vec<String>, update_available: bool,
}

struct AcView {
    id: u8, name: String,
    zone_start_index: u8, zone_count: u8,
    supported_modes: Vec<&'static str>,     // e.g. ["Heat","Cool","Auto"]
    supported_fan_speeds: Vec<&'static str>,
    setpoint_cool: (u8, u8), setpoint_heat: (u8, u8),
    status: Option<AcStatusView>,           // None until first status received
}

struct AcStatusView {
    power:      Option<&'static str>,       // "On"/"Off"/"AwayOff"/"Sleep"/None(unknown)
    mode:       Option<&'static str>,
    fan_speed:  Option<(&'static str, bool)>,// (speed, intelligent_auto)
    setpoint:   Option<Temperature>,        // keep the crate type for Display
    temperature: Option<Temperature>,
    flags:      Vec<&'static str>,           // Timer/Spill/ByPass/Turbo
    error:      Option<u16>,
}

struct ZoneView {
    id: u8, name: String,
    ac_id: Option<u8>,                       // derived from AcCapability zone range
    power: ZonePowerView,                   // Off/On/Turbo (status variant)
    has_sensor: bool,
    control_mode: ControlModeView,          // Airflow | Temperature | Unknown
    airflow_pct: u8,                         // always available (both modes report a %)
    setpoint: Option<Temperature>,           // Some only in Temperature mode
    sensor: Option<SensorView>,             // None=NoSensor, Some(NotAvailable|Temperature)
    flags: Vec<&'static str>,               // LowBattery/Spill
}
```

> **Temperature caveat:** `airtouch5::types::Temperature` has **no public numeric
> accessor** (see `temperature.rs` TODO: "conversion methods to integer/float
> values?"). We keep the `Temperature` value through to the template and render via
> `format!("{:#}", t)` → e.g. `24.3℃`. For numeric inputs (rare — see §6) we parse
> the `Display` string. The natural control path uses `Increment`/`Decrement`, which
> sidesteps the missing accessor entirely.

> **Derives for dirty diffing:** every view struct (`ConsoleInfo`, `AcView`,
> `AcStatusView`, `ZoneView`, `ControlModeView`, `SensorView`) derives
> `Clone, PartialEq` so the SSE handler can diff old vs new `Snapshot` per id (§5).

### 3.4 Mapping details (crate → view)

| crate type (`types::status`) | view field | notes |
|---|---|---|
| `AcStatus.power: Option<AcPower>` | `AcStatusView.power` | `On/Off/AwayOff/AwayOn/Sleep` |
| `AcStatus.mode: Option<AcMode>` | `.mode` | `Auto/Heat/Dry/Fan/Cool/AutoHeat/AutoCool` |
| `AcStatus.fan_speed: Option<(FanSpeed,bool)>` | `.fan_speed` | bool = IntelligentAuto modifier |
| `AcStatus.setpoint/temperature: Option<Temperature>` | kept as `Temperature` | render via Display |
| `AcFlags` (bitflags) | `.flags: Vec<&str>` | `iter_names()` |
| `ZoneStatus.power: ZonePower` | `ZoneView.power` | `Off/On/Turbo` (status enum) |
| `ZoneStatus.control: ZoneControl` | `.control_mode` + `.airflow_pct` + `.setpoint` | `Airflow(pct)` → Airflow mode, pct=pct; `Temperature(pct,temp)` → Temp mode, pct=pct, setpoint=Some(temp) |
| `ZoneStatus.sensor_reading: ZoneSensorReading` | `.has_sensor` + `.sensor` | `NoSensor`→false/None; `NotAvailable`→true/Some(NA); `Temperature(t)`→true/Some(t) |
| `ZoneFlags` (bitflags) | `.flags` | `LowBattery/Spill` |

Zone→AC ownership: while building the snapshot, for each `AcCapability`, assign
`ac_id` to zones in `zone_start_index .. zone_start_index + zone_count`.

> ⚠️ **Two different `ZonePower` enums:** `types::status::ZonePower`
> (`Off/On/Turbo`) is *what the zone is doing now*; `types::control::ZonePower`
> (`Toggle/Off/On/Turbo`) is a *command*. They are distinct types despite the
> shared name. Same for `AcPower`/`AcMode`/`FanSpeed` (status vs control variants).
> The mapping functions must use the correct module for each direction.

### 3.5 Commands (web → manager)

```rust
enum Command {
    ControlZone { id: u8, req: ZoneControlReq, reply: oneshot::Sender<Result<(), Error>> },
    ControlAc   { id: u8, req: AcControlReq,   reply: oneshot::Sender<Result<(), Error>> },
    Refresh,                                          // re-pull full status now
}

enum ZoneControlReq {
    Power(types::control::ZonePower),                // On/Off/Turbo (Toggle optional)
    SetControlType(types::control::ZoneControlType), // Airflow | Temperature
    SetValue(types::control::ZoneControlValue),       // Increment/Decrement/Airflow(%)/Temperature(t)
}
```

The manager translates each into `ZoneControl { power, control, value }` (the rest
`None`) and calls `AirTouch5::control_zone`. `control_zone` returns the updated
`ZoneStatusMessage`, which we can fold into the snapshot immediately for snappy UX;
the async `watch` update will confirm/reconcile shortly after.

> A single `control_zone` call may legitimately set all three fields at once
> (e.g. switch to Temperature mode **and** set a setpoint). The handler layer may
> compose a combined `ZoneControlReq` when the form provides both. (TODO: decide
> form shape in §6.)

---

## 4. airtouch5 API surface we use (reference)

Discovery:
```rust
use airtouch5::discovery::{discover_timeout, Console, DiscoveryError};
let console: Console = discover_timeout(Some(Duration::from_secs(3))).await?;
// Console { address: IpAddr, name: String, airtouch_id: u32, console_id: String }
```

Connection (all methods take `&self`; not `Clone`):
```rust
let at5 = AirTouch5::with_ipaddr(console.address).await?;
let caps   = at5.ac_capabilities().await?;   // AcCapabilityResponse, .by_index()
let names  = at5.zone_names().await?;        // ZoneNameResponse { zones: BTreeMap<u8,String> }
let ver    = at5.console_version().await?;   // { update_available: bool, versions: Vec<String> }
let acs    = at5.ac_status().await?;         // AcStatusMessage { acs: BTreeMap<u8, AcStatus> }
let zones  = at5.zone_status().await?;       // ZoneStatusMessage { zones: BTreeMap<u8, ZoneStatus> }
// prefill: must call ac_status() + zone_status() to populate subscribe_status() watch.
```

Live updates:
```rust
let status_rx  = at5.subscribe_status().expect("conn alive");  // watch::Receiver<CurrentStatus>
// CurrentStatus.acs(): &AcStatusSet, .zones(): &ZoneStatusSet
// both impl StatusSet: .iter() -> (&u8,&Entry), .get(i), .len(), .is_empty()
let changes_rx = at5.subscribe_changes();                        // Option<broadcast::Rx<StatusChange>>
```

Control (`feature = "control"`):
```rust
use airtouch5::types::control::{ZoneControl, ZoneControlType, ZoneControlValue, ZonePower};
use airtouch5::types::Temperature;

at5.control_zone(zone_idx, ZoneControl {
    power:   Some(ZonePower::On),                       // Toggle/Off/On/Turbo
    control: Some(ZoneControlType::Temperature),        // Toggle/Airflow/Temperature
    value:   Some(ZoneControlValue::Temperature(23.0.into())), // or Increment/Decrement/Airflow(pct)
    // control must be None if the zone has no sensor (has_sensor == false)
}).await?; // -> ZoneStatusMessage (post-change status of affected zones)

use airtouch5::types::control::{AcControl, AcPower, AcMode, FanSpeed};
at5.control_ac(ac_idx, AcControl {
    power: Some(AcPower::Toggle), mode: None, fan_speed: None, setpoint: None,
}).await?; // -> AcStatusMessage

at5.shutdown().await?; // graceful; also dropped on Drop (io_loop signaled)
```

Constraints:
- Setpoint temperatures must satisfy `Temperature::is_setpoint_valid()` → **10.0–25.0 °C**.
- Airflow percentages: **0–100** inclusive; out of range → `Err(InvalidData)`.
- `ZoneControl.control` must be `None` for sensor-less zones (cannot temp-control them).

---

## 5. HTTP routes & htmx/SSE contracts

All fragment responses are `text/html` (partial). Status updates are pushed over a
single SSE stream.

### Pages & fragments
| Method | Path | Returns | Notes |
|---|---|---|---|
| GET | `/` | `index.html` | shell + initial inline fragments (server-rendered) |
| GET | `/partials/system` | `system.html` | console card |
| GET | `/partials/acs` | `acs.html` | AC unit cards |
| GET | `/partials/zones` | `zones.html` | all zone cards |
| GET | `/partials/zone/:id` | `zone.html` | single zone card (targeted swap target) |
| GET | `/partials/ac/:id` | `ac.html` | single AC card |

### SSE
| Method | Path | Returns |
|---|---|---|
| GET | `/events` | `text/event-stream` |

SSE event types & payloads (each `data:` is an HTML fragment with a stable
`id="..."` matching what the client expects to swap):

| event | `data:` | browser target |
|---|---|---|
| `system` | `<div id="system" ...>...</div>` | swap `#system` |
| `ac` | `<div id="ac-<id>" ...>...</div>` | swap `#ac-<id>` |
| `zone` | `<div id="zone-<id>" ...>...</div>` | swap `#zone-<id>` |
| `state` | `<div id="connection-state" ...>...</div>` | connected/disconnected banner |

**Per-id dirty diffing (chosen from the start):** the SSE handler keeps the previous
`Snapshot` (clone). On each `snapshot_rx.changed()` it diffs `prev.zones` vs
`new.zones` and `prev.acs` vs `new.acs` key-by-key (view types are `PartialEq`), plus
`prev.console`/`prev.connected` vs `new` for the `system`/`state` events. Only changed
ids are rendered and emitted. Newly-appearing ids emit their full fragment; ids that
vanish are not re-emitted (a zone/ac count change is rare in phase 1 and is covered
by a full `system` re-render on reconnect).

Client wiring (in `index.html`):
```html
<div hx-ext="sse" sse-connect="/events">
  <!-- each sse-swap swaps the matching fragment by id -->
</div>
```
Each fragment element that should auto-update carries its own `sse-swap` attribute
matching the event name (e.g. `<div id="zone-3" sse-swap="zone" hx-swap="outerHTML">`).

### Control endpoints (zones — phase 1)
| Method | Path | Form fields | Action |
|---|---|---|---|
| POST | `/zone/:id/power` | `power=on|off|turbo` | `ZonePower` |
| POST | `/zone/:id/control-type` | `type=airflow|temperature` | `ZoneControlType` (temp rejected if `!has_sensor`) |
| POST | `/zone/:id/step` | `dir=up|down` | `ZoneControlValue::Increment`/`Decrement` |
| POST | `/zone/:id/airflow` | `pct=0..100` | `ZoneControlValue::Airflow(pct)` |
| POST | `/zone/:id/setpoint` | `temp=10.0..25.0` | `ZoneControlValue::Temperature(t)` |

Each POST handler: send `Command` → await reply → render the affected `zone.html`
fragment → return it. The browser swaps it in (htmx `hx-target`/`hx-swap`).
The subsequent async watch update may re-emit the same fragment over SSE; that's
fine (idempotent swap). Response `HX-Redirect`/`HX-Trigger` not needed.

### Control endpoints (AC units — phase 2)
| Method | Path | Form fields | Action |
|---|---|---|---|
| POST | `/ac/:id/power` | `power=on|off|away|sleep|toggle` | `AcPower` (control enum) |
| POST | `/ac/:id/mode` | `mode=auto|heat|dry|fan|cool` | `AcMode` |
| POST | `/ac/:id/fan` | `fan=auto|quiet|low|medium|high|powerful|turbo` | `FanSpeed` |
| POST | `/ac/:id/setpoint` | `temp=<float>` | `setpoint` (validated against AC's cool/heat range) |

---

## 6. UI layout

```
┌──────────────────────────── AirTouch 5 — <SystemName> ───────────────────────────┐
│ ● Connected   192.168.x.x   ID #13   FW v…   [refresh]                           │  ← #system / #connection-state
├──────────────────────────────────────────────────────────────────────────────────┤
│ AC units                                                                         │
│  ┌─────────────────────┐ ┌─────────────────────┐                                 │  ← #ac-<id>
│  │ Upstairs  ON  Heat    │ │ Downstairs OFF      │                                 │
│  │ Fan: Low   22.0→23.0  │ │ …                   │                                 │
│  └─────────────────────┘ └─────────────────────┘                                 │
├──────────────────────────────────────────────────────────────────────────────────┤
│ Zones                                                                            │
│  ┌────────────────────────────────────────────────────────────────────────────┐  │  ← #zone-<id>
│  │ Living Room   ● ON   [ % Airflow | Temp Setpoint ]   turbo [off]            │  │
│  │ Airflow: ████████░░ 65%    Sensor: 24.3℃   Setpoint: 23.0℃                │  │
│  │ [ − ] [ + ]  (mode=airflow)  airflow slider: 0────●───100  [Set]           │  │
│  └────────────────────────────────────────────────────────────────────────────┘  │
│  ┌────────────────────────────────────────────────────────────────────────────┐  │
│  │ Bedroom  ○ OFF   (no sensor → airflow only)                                │  │
│  │ Airflow: ██░░░░░░░░ 20%                                                    │  │
│  │ [ − ] [ + ]  airflow slider 0──●─────100  [Set]  [ON]                      │  │
│  └────────────────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────────────────┘
```

Zone card controls (matches §5 endpoints):
- **Mode toggle:** two segmented buttons `% Airflow` / `Temp Setpoint` → `POST /zone/:id/control-type`.
  The temperature option is disabled (`disabled`, `aria-disabled`) when `!has_sensor`.
- **Step:** `−` / `+` → `POST /zone/:id/step` (`dir=down|up`). Works in either mode
  (−1 °C or −5 % / +1 °C or +5 %).
- **Airflow slider:** 0–100 → `POST /zone/:id/airflow` (submit on `change`/release,
  htmx `hx-trigger="change changed delay:400ms"` for debounce).
- **Setpoint field:** 10.0–25.0 → `POST /zone/:id/setpoint` (shown only in Temp mode).
- **Power:** `ON`/`OFF`/`turbo` buttons → `POST /zone/:id/power`.

---

## 7. Project structure

```
aircon/
├── Cargo.toml
├── DESIGN.md
├── static/
│   └── vendor/                 # htmx.min.js, htmx-ext-sse.js (vendored)
├── templates/
│   ├── index.html              # full page shell + SSE bootstrap
│   ├── base.html               # <head>, htmx script tags, block content
│   ├── partials/
│   │   ├── system.html         # #system + #connection-state
│   │   ├── acs.html
│   │   ├── ac.html
│   │   ├── zones.html
│   │   └── zone.html           # the swap target for #zone-<id>
│   └── macros.html             # shared bits (temp display, flags badges)
└── src/
    ├── main.rs                 # tracing init, build ManagerHandle, serve axum
    ├── config.rs               # listen addr, discovery timeout, log level
    ├── manager/
    │   ├── mod.rs              # ManagerHandle, spawn_manager(), supervisor loop
    │   ├── command.rs          # Command / *Req enums
    │   └── snapshot.rs         # Snapshot + view types + crate→view mapping
    ├── airtouch/
    │   └── mod.rs              # thin helpers: discover_with_retry(), prefill()
    ├── web/
    │   ├── mod.rs              # router builder, AppState
    │   ├── state.rs            # AppState { manager: Arc<ManagerHandle> }
    │   ├── error.rs            # AppError → IntoResponse (renders a fragment err)
    │   ├── sse.rs              # /events: convert watch<Snapshot> → EventStream
    │   └── handlers/
    │       ├── mod.rs
    │       ├── pages.rs        # GET /, GET /partials/*
    │       ├── zone.rs        # POST /zone/:id/*
    │       └── ac.rs           # POST /ac/:id/* (phase 2)
    └── templates.rs            # askama struct definitions mapping to templates
```

---

## 8. Implementation TODOs

Phased. Check off as you go.

### Phase 0 — scaffolding
- [ ] Update `Cargo.toml` with deps in §2; enable `airtouch5` `control` feature.
- [ ] `tracing_subscriber::fmt().init()` + `tower_http::trace::TraceLayer`.
- [ ] Vendor latest htmx (2.x) + `sse` extension, **un-minified**, under versioned
      paths in `static/vendor/` (e.g. `htmx/2.0.4/htmx.js`, `htmx-ext-sse/2.0.4/sse.js`);
      mount `ServeDir` at `/vendor` with `Cache-Control: public, max-age=31536000, immutable`;
      reference the versioned paths in `<script src>` so bumps cache-bust.
- [ ] Add `askama` template dir + `#[derive(Template)]` stubs (`templates.rs`).

### Phase 1a — connection manager
- [ ] `manager/snapshot.rs`: define `Snapshot`, `ConsoleInfo`, `AcView`,
      `AcStatusView`, `ZoneView`, `ControlModeView`, `SensorView`.
- [ ] Mapping functions: `AcStatus→AcStatusView`, `ZoneStatus→ZoneView`,
      `AcCapability→AcView (static part)`, bitflags→`Vec<&str>`, zone→AC ownership.
      (Use `types::status::` enums for status; `types::control::` only in handlers.)
- [ ] `manager/command.rs`: `Command`, `ZoneControlReq`, `AcControlReq`.
- [ ] `manager/mod.rs`: `ManagerHandle { snapshot_rx, cmd_tx }` + `Clone`.
- [ ] `airtouch/mod.rs`: `discover_with_retry()` (backoff), `prefill(&AirTouch5)`
      (`try_join` of caps+names+version+status).
- [ ] Supervisor loop (`select!` over cmd_rx, `status_rx.changed()`, shutdown).
      Reconnect on `status_rx` error → rebuild from step 1.
- [ ] Snapshot rebuild: merge static (caps/names/version/console) + live
      (`CurrentStatus.acs()`/`.zones()`) → `Snapshot`; publish via `watch::send`.

### Phase 1b — web layer (read-only first)
- [ ] `web/state.rs` AppState; router with `/`, `/partials/*`, `/events`.
- [ ] Templates: `index.html`, `partials/{system,zones,zone}.html`.
- [ ] Render initial page from `snapshot_rx.borrow().clone()`.
- [ ] `web/sse.rs`: subscribe to `snapshot_rx`; keep a prev `Snapshot`; on
      `changed()` do a per-id diff (view types `PartialEq`) and emit only the
      changed `system`/`ac`/`zone`/`state` fragments (see §5).
- [ ] Manual smoke test against a real (or mocked) console.

### Phase 1c — zone control
- [ ] `handlers/zone.rs`: implement the 5 POST endpoints from §5.
- [ ] Form → `ZoneControlReq` → `Command::ControlZone` → await reply → render
      `zone.html`. Return `400` (with inline error) for: temp on sensor-less
      zone, out-of-range airflow, invalid setpoint.
- [ ] Wire htmx attributes in `zone.html` (`hx-post`, `hx-target="#zone-<id>"`,
      `hx-swap="outerHTML"`). Debounce slider with `hx-trigger`.
- [ ] End-to-end: toggle power, switch %↔temp, step, set airflow, set setpoint.

### Phase 1d — polish
- [ ] Connection-state banner over SSE (`#connection-state`).
- [ ] Disable `Temp Setpoint` button when `!has_sensor`.
- [ ] Clamp/validate setpoint to 10.0–25.0 (and warn if outside AC capability range).
- [ ] Show AC errors (`error: Option<u16>`) and zone flags (`LowBattery`,`Spill`).
- [ ] `tower_http::trace` + structured logs at INFO, DEBUG for AT5 frames.
- [ ] Graceful shutdown on SIGINT (drain SSE, `AirTouch5::shutdown()`).

### Phase 2 — AC unit control
- [ ] `handlers/ac.rs`: power/mode/fan/setpoint endpoints (§5 phase-2 table).
- [ ] `partials/ac.html`: mode buttons (filtered by `supported_modes`), fan buttons
      (filtered by `supported_fan_speeds`), setpoint constrained to AC cool/heat range.
- [ ] SSE `ac` events; ensure `ac.html` is an `#ac-<id>` swap target.

### Phase 3 — robustness & niceties
- [ ] Reconnect backoff with jitter; surface retries in the UI banner.
- [ ] Optional: cache discovered console address to a file for faster reconnect.
- [ ] Optional: AC toggle smart logic (on↔off based on current `AcStatus.power`).
- [ ] Tests (deferred from phase 1): if needed, add a thin `trait At5` around the
      `airtouch5` methods we use + a `FakeAt5` to drive the manager end-to-end.
      The manager stays concretely tied to `AirTouch5` until then.

---

## 9. Resolved questions & residual risks

**Resolved (decisions locked in):**

1. **Testability.** Skip automated tests in phase 1 — the manager stays concretely
   tied to `AirTouch5` (no `At5` trait). Pure-function transforms are easily testable
   later via public crate constructors (`AcStatusMessage::new(...).into()` →
   `AcStatusSet`, `CurrentStatus::default()` + `apply(StatusChange::...)`), so a
   trait is only worth adding to drive the *async glue* in tests. Deferred to
   phase 3 if needed.
2. **`Temperature` has no numeric getter.** Accepted. Render via `Display`
   (`format!("{:#}", t)`), parse the `Display` string for input defaults; controls
   use `Increment`/`Decrement`/`Airflow(pct)`/`Temperature::from_float`. Optionally
   contribute an upstream accessor later.
3. **SSE fan-out.** Per-id dirty tracking **from the start** (not a phase-3
   optimization): view types derive `PartialEq`; the SSE handler diffs prev vs new
   `Snapshot` and emits only changed `system`/`ac`/`zone`/`state` fragments.
4. **htmx assets.** Vendor **latest htmx** (2.x) + the `sse` extension, **un-minified**,
   under versioned paths with `Cache-Control: public, max-age=31536000, immutable`.

**Residual risks (noted, no action in phase 1):**

- **Multiple consoles on one LAN.** `discover_timeout()` returns the first responder
  → non-deterministic on multi-console networks. Accepted (§1 out of scope); only
  revisit if a real deployment hits it.
- **Zone-name non-ASCII encoding bug.** The crate already decodes via lossy UTF-8 in
  `zone_names()`; names may appear truncated/garbled for non-ASCII. Upstream issue,
  no action here.
