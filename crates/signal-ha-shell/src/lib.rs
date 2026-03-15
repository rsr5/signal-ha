//! # signal-ha-shell
//!
//! Shared Python runtime for Signal Deck and signal-ha-agent.
//!
//! Wraps the [Monty](https://github.com/pydantic/monty) pure-Rust Python
//! interpreter with Home Assistant host call support.
//!
//! ## Architecture
//!
//! This crate owns three things:
//!
//! 1. **REPL lifecycle** — `init_repl`, `feed_snippet`, `start_snippet`,
//!    `resume_call`.  These drive Monty's `MontyRepl` with proper
//!    error recovery and external function suspension.
//!
//! 2. **Host call mapping** — when Python calls `state("sensor.temp")`,
//!    Monty suspends and this crate maps the function name + args to a
//!    `HostCall { method, params }` that the caller fulfills (via HA
//!    WebSocket, REST, or mock).
//!
//! 3. **Data conversion** — `MontyObject` ↔ `serde_json::Value`,
//!    HA state JSON → `EntityState` dataclass, etc.
//!
//! ## Consumers
//!
//! - **signal-ha-agent** (native, systemd): imports this crate, fulfills
//!   host calls via `HaClient` WebSocket.
//! - **shell-engine** (WASM, browser): imports this crate, adds WASM
//!   wrapper + UI rendering (RenderSpec, icons, magic commands, charts).
//!
//! ## Example
//!
//! ```rust,ignore
//! use signal_ha_shell::{init_repl, start_snippet, resume_call, ReplEvalResult};
//! use signal_ha_shell::{map_ext_call_to_host_call, json_to_entity_state};
//! use monty::MontyObject;
//!
//! let repl = init_repl("").unwrap();
//! let result = start_snippet(repl, "state('sensor.temp')");
//!
//! match result {
//!     ReplEvalResult::HostCallNeeded { function_name, args, call, .. } => {
//!         let (method, params) = map_ext_call_to_host_call(&function_name, &args).unwrap();
//!         // ... fulfill via HA API ...
//!         let entity_json = serde_json::json!({"entity_id": "sensor.temp", "state": "22.5", "attributes": {}});
//!         let monty_val = json_to_entity_state(&entity_json);
//!         let resumed = resume_call(call, monty_val);
//!         // ... handle resumed result ...
//!     }
//!     _ => {}
//! }
//! ```

pub mod convert;
pub mod ext_functions;
pub mod host_call;
pub mod repl;

// Re-export the public API.

// REPL lifecycle
pub use repl::{init_repl, init_repl_with_functions, feed_snippet, start_snippet, start_snippet_with_extras};
pub use repl::{resume_call, resume_call_with_extras};
pub use repl::{is_name_error_for_external_fn, is_name_error_for_external_fn_with_extras};
pub use repl::{ReplEvalResult, format_monty_error};

// External function registry
pub use ext_functions::HA_EXTERNAL_FUNCTIONS;

// Host call mapping
pub use host_call::{map_ext_call_to_host_call, parse_ago};

// Data conversion
pub use convert::{
    monty_obj_to_json, json_to_monty_obj,
    json_to_entity_state, json_to_entity_state_list,
};

// Re-export key Monty types so consumers don't need a direct monty dependency
// for basic usage (they can still add monty directly for advanced use).
pub use monty::{ExtFunctionResult, MontyObject, MontyRepl, NoLimitTracker, ReplFunctionCall};
