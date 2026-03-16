//! # signal-ha-agent
//!
//! Embedded LLM observation agent for signal-ha automations.
//!
//! Uses the **markdown-agent pattern** from Signal Deck: send a system
//! prompt + conversation to a Home Assistant Conversation entity (any LLM)
//! via the `conversation/process` WebSocket API, parse fenced code blocks
//! from the response, execute them as **Python** via the Monty runtime,
//! inject results, loop.
//!
//! # Architecture
//!
//! This crate uses **signal-ha-shell** for the Python execution engine —
//! the same portable Monty REPL layer that Signal Deck's shell-engine uses.
//! The LLM writes Python code in ```signal-deck blocks, the engine executes
//! it, and external function calls (e.g. `state("sensor.temp")`) are
//! fulfilled via `HaClient` against the HA WebSocket/REST APIs.
//!
//! - **Signal Deck** (WASM, in-browser): TypeScript calls `conversation/process`,
//!   Rust shell engine executes Python snippets, results rendered in card UI.
//! - **signal-ha-agent** (native, in systemd): Rust calls `conversation/process`
//!   via `HaClient`, Python executes via Monty, results written to journald.
//!
//! Both share:
//! - `signal-ha-shell` for REPL lifecycle, external function registry,
//!   host call mapping, and MontyObject↔JSON conversion
//! - The same markdown parsing of fenced code blocks
//! - The same `conversation/process` protocol
//!
//! # Key components
//!
//! - **`conversation`** — `conversation/process` WebSocket wrapper
//! - **`parser`** — extract fenced `signal-deck` blocks from LLM markdown
//! - **`engine`** — Python execution engine wrapping signal-ha-shell's REPL
//! - **`ha_host`** — HA-specific host call fulfillment via HaClient + REST
//! - **`memory`** — persistent agent memory across sessions (JSON file)
//! - **`session`** — `AgentConfig`, `AgentHandle::spawn()`, session lifecycle
//!
//! # Integration
//!
//! ```rust,ignore
//! use signal_ha_agent::{AgentConfig, AgentHandle};
//! use signal_ha_agent::ha_host::HaHost;
//!
//! let ha_host = Arc::new(HaHost::new(
//!     client.clone(),
//!     "http://homeassistant.local:8123".into(),
//!     "ws://homeassistant.local:8123/api/websocket".into(),
//!     token.clone(),
//!     Some("http://localhost:9102/?format=json".into()),
//! ));
//!
//! let handle = AgentHandle::spawn(AgentConfig {
//!     name: "garage-agent".into(),
//!     role: "You are the garage lighting specialist.".into(),
//!     ha_client: client.clone(),
//!     conversation_entity: Some("conversation.claude_conversation".into()),
//!     description: "Garage lights automation...".into(),
//!     ha_host,
//!     memory_path: "/var/lib/signal-ha/agent-memory/garage.json".into(),
//!     disallowed_calls: vec![],
//!     // ...
//! });
//! ```
//!
//! # Safety
//!
//! By default the agent is **read-only** — `call_service` is available
//! but can be blocked via `disallowed_calls`.  Agent panics are caught
//! and logged; they never propagate to the host automation.

mod conversation;
mod engine;
pub mod ha_host;
pub mod memory;
mod parser;
mod session;

// Keep old modules around but deprecated, for gradual migration
#[deprecated(note = "Use ha_host instead — Python runtime replaces Tool dispatch")]
pub mod ha_tools;
#[deprecated(note = "Use engine instead — Python runtime replaces Tool dispatch")]
pub mod tools;

pub use ha_host::HostExtension;
pub use session::{AgentConfig, AgentHandle};
