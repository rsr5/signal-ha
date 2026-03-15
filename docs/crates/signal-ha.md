# signal-ha

> Core library — async WebSocket client and sun-aware scheduler for Home Assistant.

## Overview

`signal-ha` is the foundation crate. It provides two primary types:

- **`HaClient`** — WebSocket connection to Home Assistant with auth, state
  queries, service calls, and event subscriptions.
- **`Scheduler`** — Sun-aware timer primitives. Calculate sunrise/sunset for
  a given location, schedule daily callbacks, and run time-windowed logic.

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
