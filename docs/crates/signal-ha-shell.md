# signal-ha-shell

> Portable Monty Python runtime for agent code execution.

## Overview

Wraps the [Monty](https://github.com/pydantic/monty) pure-Rust Python
interpreter with Home Assistant host call support. Shared between
`signal-ha-agent` (native/systemd) and Signal Deck (WASM/browser).

This crate has **no HA client dependency** — it's a pure runtime. The caller
fulfils host calls by providing entity data when the REPL suspends.

## How it works

When Python code calls `state("sensor.temperature")`, the Monty interpreter
**suspends execution** and returns a `HostCallNeeded` result. The caller
(typically `signal-ha-agent`) fulfils the call via the HA WebSocket API and
resumes the REPL with the result.

```
Python code                    signal-ha-shell              Caller
    │                               │                         │
    │  state("sensor.temp")         │                         │
    │──────────────────────────────►│                         │
    │                               │  HostCallNeeded         │
    │                               │  {state, sensor.temp}   │
    │                               │────────────────────────►│
    │                               │                         │ HA query
    │                               │  resume(result)         │
    │                               │◄────────────────────────│
    │  ← "22.5"                     │                         │
    │◄──────────────────────────────│                         │
```

## API Reference

### REPL Lifecycle

| Function | Purpose |
|:---------|:--------|
| `init_repl()` | Initialize a Monty REPL session |
| `init_repl_with_functions()` | Initialize with custom external functions |
| `feed_snippet()` | Feed incomplete/multi-line Python to the REPL |
| `start_snippet()` | Execute a Python snippet; returns `ReplEvalResult` |
| `resume_call()` | Resume REPL after host call fulfillment |
| `format_monty_error()` | Format Monty exceptions to readable strings |

### ReplEvalResult

```rust
enum ReplEvalResult {
    Complete(Option<String>),     // Execution finished (with optional output)
    HostCallNeeded { .. },        // Suspended — waiting for host call
    Error(String),                // Execution error
}
```

### Host Call Mapping

| Function | Purpose |
|:---------|:--------|
| `map_ext_call_to_host_call()` | Map Python function call → `(method, params)` |
| `parse_ago()` | Parse time strings like `"5 minutes ago"` |
| `HA_EXTERNAL_FUNCTIONS` | Registry of available host functions |

### Available Host Functions

| Python function | Maps to |
|:----------------|:--------|
| `state(entity_id)` | Get current entity state |
| `history(entity_id, ago)` | Get state history |
| `statistics(entity_id, ago)` | Get statistics (mean, min, max) |
| `relatives(entity_id)` | Get related entities |
| `call_service(domain, service, data)` | Call HA service |

### Data Conversion

| Function | Purpose |
|:---------|:--------|
| `monty_obj_to_json()` | `MontyObject` → `serde_json::Value` |
| `json_to_monty_obj()` | `serde_json::Value` → `MontyObject` |
| `json_to_entity_state()` | JSON → HA `EntityState` dataclass |
