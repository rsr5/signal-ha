# signal-ha-lighting

> Shared lighting primitives — actuator, overlay, reconciler, lux-adaptive targets.

## Overview

Ported from the Python `appdaemon-lighting` package with identical semantics
and a 1:1 test suite. This crate is **pure logic** — no HA client dependency.
Your automation calls these functions and translates the results into HA
service calls.

## Modules

### LightTarget

Per-entity control structure specifying on/off, brightness, and colour
temperature.

```rust
use signal_ha_lighting::LightTarget;

let target = LightTarget {
    entity_id: "light.kitchen".into(),
    on: true,
    brightness: Some(180),
    color_temp: Some(350),
};
```

### Actuator

Rate-limited, deadband-aware state applicator. Prevents flooding HA with
redundant service calls when brightness only changes by 1%.

| Type | Purpose |
|:-----|:--------|
| `Actuator` | Applies light targets with rate-limiting |
| `ActuatorConfig` | Deadband, min interval, rate limit settings |
| `ApplyResult` | Whether a change was applied, skipped, or rate-limited |
| `HAService` | Abstraction over `light.turn_on` / `light.turn_off` calls |

### OverlayManager

Snapshot/restore system for transient lighting overrides — movie mode,
toothbrush timer, guest mode, etc. Captures current state, applies an
overlay, then restores when the overlay ends.

| Type | Purpose |
|:-----|:--------|
| `OverlayManager` | Manages active overlays + snapshots |
| `LightSnapshot` | Captured entity state for later restoration |
| `OverlayHAService` | Service call abstraction for overlays |

### Reconciler

Watches entity availability and retries failed state applications. Handles
bulbs that go offline momentarily (Zigbee mesh drops, power cycles).

| Type | Purpose |
|:-----|:--------|
| `Reconciler` | Availability watcher with retry queue |
| `ReconcileConfig` | Retry interval, max attempts, backoff |
| `TimerHandle` | Handle for scheduled reconciliation attempts |

### Lux Mapping

Maps ambient lux readings to brightness and colour temperature targets using
configurable time windows and policies.

```rust
use signal_ha_lighting::{brightness_for_target_lux, ct_from_lux};

let brightness = brightness_for_target_lux(150.0, &policy);
let ct = ct_from_lux(150.0, &ct_params);
```

| Function | Purpose |
|:---------|:--------|
| `brightness_for_target_lux()` | Lux → brightness (0–255) |
| `ct_from_lux()` | Lux → colour temperature (mireds) |
| `LuxTargetPolicy` | Policy config for lux mapping |
| `TimeWindow` | Time-of-day based parameter selection |
| `CtFromLuxParams` | Colour temperature mapping parameters |

### Utilities

| Function | Purpose |
|:---------|:--------|
| `stable_signature()` | Deterministic change-detection hash |
| `clamp`, `lerp`, `linmap` | Math helpers |
| `melvin_to_mired` | Kelvin ↔ mired conversion |
| `brightness_pct` | 0–255 ↔ percentage conversion |
| `smoothstep` | Smooth interpolation curve |
