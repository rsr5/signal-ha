//! # signal-ha
//!
//! Async Rust library for writing Home Assistant automations.
//!
//! Each automation runs as its own OS process, managed by systemd.
//! This library provides the building blocks:
//!
//! - [`HaClient`] — WebSocket connection to Home Assistant (auth, state,
//!   service calls, subscriptions)
//! - [`Scheduler`] — Sun-aware timer primitives (sunrise/sunset, daily, after)
//!
//! # Example
//!
//! ```rust,no_run
//! use signal_ha::{HaClient, Scheduler};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = HaClient::connect("ws://ha:8123/api/websocket", "token").await?;
//!     let scheduler = Scheduler::new(48.86, 2.35);
//!
//!     let state = client.get_state("sensor.porch_lux").await?;
//!     println!("Porch lux: {}", state.state);
//!     Ok(())
//! }
//! ```

mod client;
mod power_fsm;
mod scheduler;
mod status;
mod types;

pub use client::HaClient;
pub use client::HaError;
pub use power_fsm::{CompletedCycle, FsmEvent, PowerFsm, PowerFsmConfig, PowerState};
pub use scheduler::Scheduler;
pub use status::StatusPage;
pub use status::StatusValue;
pub use types::{EntityState, StateChange};
