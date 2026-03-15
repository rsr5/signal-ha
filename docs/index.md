<p align="center">
  <img src="assets/banner.png" alt="signal-ha banner" />
</p>

# signal-ha

Async Rust toolkit for writing Home Assistant automations as standalone
processes managed by systemd.

---

## What it does

**signal-ha** gives you a WebSocket client, sun-aware scheduler, lighting
engine, embedded LLM agent, and Python bindings — everything needed to build
Home Assistant automations that run as plain systemd services.

Each automation is its own OS process. systemd handles restarts, dependency
ordering, and resource limits. No plugin loader, no shared runtime, no
framework. The OS *is* the framework.

## Crates

| Crate | Description |
|:------|:------------|
| [`signal-ha`](crates/signal-ha.md) | Core library — `HaClient` (WebSocket + REST) and `Scheduler` (sun-aware timers) |
| [`signal-ha-lighting`](crates/signal-ha-lighting.md) | Lighting primitives — actuator, overlay, reconciler, lux-adaptive targets |
| [`signal-ha-agent`](crates/signal-ha-agent.md) | Embedded LLM observation agent (HA Conversation API) |
| [`signal-ha-shell`](crates/signal-ha-shell.md) | Portable Monty Python runtime for agent code execution |
| [`signal-ha-py`](crates/signal-ha-py.md) | PyO3 bindings — use signal-ha from Python |
| [`message-board`](crates/message-board.md) | SQLite-backed findings board REST API for agents |

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

## Philosophy

- **One process per automation** — crash isolation, independent restarts
- **systemd is the supervisor** — no custom daemon, no watchdog
- **Read-only agents** — LLM observers that watch but don't act (unless you let them)
- **Sun-aware scheduling** — first-class sunrise/sunset primitives, not cron hacks
- **Rust all the way down** — Python only at the edges (PyO3 bindings, agent scripts)
