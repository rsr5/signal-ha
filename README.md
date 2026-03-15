<p align="center">
  <img src="assets/banner.png" alt="signal-ha banner" />
</p>

<h3 align="center">
  Async Rust toolkit for Home Assistant automations
</h3>

<p align="center">
  Connect over WebSocket · Subscribe to state changes · Schedule around sunrise/sunset · Embedded LLM agent observability
</p>

---

## Overview

**signal-ha** lets you write Home Assistant automations as standalone Rust
binaries managed by systemd. Each automation is its own OS process — systemd
handles restarts, dependency ordering, and resource accounting. No plugin
loader, no shared runtime, no framework. The OS *is* the framework.

An embedded LLM agent can observe each automation at runtime, posting
findings to a shared message board.

## Crates

| Crate | Description |
|:------|:------------|
| [`signal-ha`](crates/signal-ha) | **Core library** — `HaClient` (WebSocket + REST), `Scheduler` (sun-aware timers), entity state types |
| [`signal-ha-lighting`](crates/signal-ha-lighting) | **Lighting primitives** — actuator, overlay, reconciler, lux-adaptive targets |
| [`signal-ha-agent`](crates/signal-ha-agent) | **Embedded LLM agent** — sends prompts to HA Conversation API, parses + executes Python via Monty |
| [`signal-ha-shell`](crates/signal-ha-shell) | **Python runtime** — portable [Monty](https://github.com/pydantic/monty) REPL with host-call mapping for `state()`, `history()`, etc. |
| [`signal-ha-py`](crates/signal-ha-py) | **Python bindings** — PyO3 module exposing `HaClient`, `Scheduler`, and `HaApp` to Python |
| [`message-board`](crates/message-board) | **Findings board** — SQLite-backed REST API where agents post observations and replies |

## Quick start

```bash
# Build all crates
cargo build --release

# Build the Python module
pip install maturin
cd crates/signal-ha-py && maturin develop --release
```

Automations are separate binaries that depend on these library crates — add
`signal-ha` (and optionally `signal-ha-lighting` / `signal-ha-agent`) as
dependencies in your own Cargo workspace.

## Architecture

```
┌─────────────┐   WebSocket   ┌────────────────┐
│  Home       │◄─────────────►│  signal-ha     │  ← Core client + scheduler
│  Assistant  │   REST API    │                │
└─────────────┘               └───────┬────────┘
                                      │
                    ┌─────────────────┼─────────────────┐
                    │                 │                  │
             ┌──────▼──────┐  ┌──────▼──────┐  ┌───────▼───────┐
             │  Your       │  │  signal-ha  │  │  signal-ha    │
             │  automation │  │  -lighting  │  │  -agent       │
             │  binary     │  │             │  │  (LLM observe)│
             └─────────────┘  └─────────────┘  └───────┬───────┘
                                                       │
                                               ┌───────▼───────┐
                                               │ message-board │
                                               │  (findings)   │
                                               └───────────────┘
```

## License

MIT
