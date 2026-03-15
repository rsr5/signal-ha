# signal-ha

> Core library — async WebSocket client and sun-aware scheduler for Home Assistant.

## Overview

`signal-ha` is the foundation crate. It provides:

- **`HaClient`** — WebSocket connection to Home Assistant with auth, state
  queries, service calls, and event subscriptions.
- **`Scheduler`** — Sun-aware timer primitives. Calculate sunrise/sunset for
  a given location, schedule daily callbacks, and run time-windowed logic.
- **`DashboardManager`** — Lovelace dashboard CRUD over the WebSocket API,
  with card type validation.
- **`DashboardSpec`** — YAML-driven dashboard definitions. Each automation
  embeds a `dashboard.yaml` and pushes it to HA on startup.

Plus supporting types for state machines, status pages, and entity state tracking.

## API Reference

### HaClient

The WebSocket client handles the full HA WebSocket lifecycle: connect,
authenticate, subscribe to state changes, query current state, and call
services.

```rust
use signal_ha::HaClient;

let client = HaClient::connect("ws://homeassistant.local:8123/api/websocket", token).await?;

// Get current state
let state = client.get_state("sensor.temperature").await?;
println!("{}: {}", state.entity_id, state.state);

// Subscribe to all state changes
let mut rx = client.subscribe_state_changes().await?;
while let Some(change) = rx.recv().await {
    println!("{} changed: {} → {}", change.entity_id, change.old.state, change.new.state);
}

// Call a service
client.call_service("light", "turn_on", json!({"entity_id": "light.office"})).await?;
```

### Scheduler

Sun-aware scheduling backed by the `sunrise` crate and tokio timers.

```rust
use signal_ha::Scheduler;

let scheduler = Scheduler::new(48.86, 2.35); // lat, lon

let sunrise = scheduler.next_sunrise();
let sunset  = scheduler.next_sunset();

// Run a callback daily at sunset
scheduler.at_sunset(|| async {
    // turn on the porch lights
}).await;
```

### PowerFsm

Finite state machine for multi-step automations with named states and
event-driven transitions.

| Type | Purpose |
|:-----|:--------|
| `PowerFsm` | FSM instance with state transitions |
| `PowerFsmConfig` | Configuration for FSM states and transitions |
| `PowerState` | Current state enum |
| `FsmEvent` | Events that drive transitions |
| `CompletedCycle` | Record of a completed FSM cycle |

### StatusPage

Exposes an HTTP status endpoint (via axum) for monitoring.

```rust
use signal_ha::StatusPage;

let status = StatusPage::new("porch-lights", 9100);
status.set("mode", StatusValue::String("night".into()));
status.serve().await; // serves on :9100
```

### Core Types

| Type | Purpose |
|:-----|:--------|
| `EntityState` | Current state of a HA entity: `state`, `attributes`, `last_changed` |
| `StateChange` | State change event: `entity_id`, `old`, `new` |
| `HaError` | Error enum — WebSocket, JSON, auth, HA errors, timeouts |

### DashboardManager

Manages Lovelace dashboards and their configs over the WebSocket API.
Ships with a built-in card type registry; consumers can register additional
HACS/custom card types for validation.

```rust
use signal_ha::{HaClient, DashboardManager};
use serde_json::json;

let client = HaClient::connect(url, token).await?;
let mgr = DashboardManager::new(&client)
    .with_custom_cards(["mushroom-entity-card", "mini-graph-card"]);

// Ensure a dashboard exists (idempotent — safe on every deploy)
mgr.ensure("signal-porch", "Porch Lights", "mdi:coach-lamp", json!({
    "views": [{
        "title": "Porch",
        "cards": [
            { "type": "entities", "entities": ["light.porch"] },
            { "type": "history-graph", "entities": ["sensor.porch_lux"] }
        ]
    }]
})).await?;

// Read back
let config = mgr.get_config(Some("signal-porch")).await?;

// List all dashboards
let dashboards = mgr.list_dashboards().await?;

// Validate card types before pushing
let unknowns = mgr.validate_card_types(&config);
for u in &unknowns {
    println!("Unknown card: {} at view {} card {}", u.card_type, u.view_index, u.card_index);
}
```

| Method | Purpose |
|:-------|:--------|
| `new(client)` | Create a manager with built-in card types only |
| `with_custom_cards(cards)` | Register HACS/custom card types for validation |
| `list_dashboards()` | List all dashboards |
| `create_dashboard(url_path, title, icon)` | Create a new dashboard |
| `update_dashboard(id, updates)` | Update dashboard metadata |
| `delete_dashboard(id)` | Delete a dashboard |
| `get_config(url_path)` | Get full Lovelace config |
| `save_config(url_path, config)` | Replace entire dashboard config |
| `delete_config(url_path)` | Reset to auto-generated |
| `ensure(url_path, title, icon, config)` | Idempotent create-or-update |
| `validate_card_types(config)` | Find unknown card types |

#### Built-in Card Types

`BUILTIN_CARD_TYPES` contains all ~55 card types shipped with HA 2026.3,
including the always-loaded set (`entities`, `tile`, `button`, etc.),
lazy-loaded cards (`map`, `calendar`, `gauge`, etc.), and all energy cards.

Custom cards use the `custom:` prefix in HA YAML. Register them with
`with_custom_cards()` using the bare name (without the prefix).

### DashboardSpec

High-level wrapper for automations that own their dashboard. Each automation
ships a `dashboard.yaml` alongside its source, embeds it with `include_str!`,
and calls `ensure()` on startup. Idempotent — safe on every boot.

```rust
use signal_ha::{HaClient, DashboardSpec};

const DASHBOARD: &str = include_str!("../dashboard.yaml");

let client = HaClient::connect(url, token).await?;
DashboardSpec::from_yaml(DASHBOARD)?.ensure(&client).await?;
```

#### YAML format

```yaml
url_path: signal-porch-lights
title: "Porch Lights"
icon: "mdi:coach-lamp"

# Optional: HACS/custom card types used by this dashboard
custom_cards:
  - mushroom-entity-card

config:
  views:
    - title: "Porch Lights"
      cards:
        - type: entities
          entities:
            - light.porch
        - type: gauge
          entity: sensor.porch_lux
```

| Method | Purpose |
|:-------|:--------|
| `from_yaml(yaml)` | Parse a dashboard spec from a YAML string |
| `ensure(client)` | Push the dashboard to HA (create-or-update) |
