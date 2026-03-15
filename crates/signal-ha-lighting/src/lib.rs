//! # signal-ha-lighting
//!
//! Shared lighting primitives for Home Assistant automations.
//!
//! Ported from the Python `appdaemon-lighting` package with identical
//! semantics and a 1:1 test suite for verification.
//!
//! ## Modules
//!
//! - [`target`] — `LightTarget` struct (per-entity on/off/brightness/CT)
//! - [`actuator`] — Rate-limited, deadband-aware actuator
//! - [`overlay`] — Snapshot/restore overlay manager (toothbrush, movie, etc.)
//! - [`reconcile`] — Entity availability watcher with retry
//! - [`signature`] — Deterministic change-detection signatures
//! - [`util`] — Pure helper functions (clamp, brightness conversions, etc.)

pub mod actuator;
pub mod lux;
pub mod overlay;
pub mod reconcile;
pub mod signature;
pub mod target;
pub mod util;

// Re-exports for convenience
pub use actuator::{Actuator, ActuatorConfig, ApplyResult, HAService};
pub use lux::{
    brightness_for_target_lux, ct_from_lux, CtFromLuxParams, LuxTargetPolicy, TimeWindow,
};
pub use overlay::{LightSnapshot, OverlayHAService, OverlayManager};
pub use reconcile::{
    is_unavailable, ReconcileConfig, ReconcileHAService, ReconcileScheduler, Reconciler,
    TimerHandle,
};
pub use signature::stable_signature;
pub use target::LightTarget;
pub use util::{
    as_bool, clamp, clamp_int, ha_brightness_to_pct, kelvin_to_mired, lerp, linmap,
    pct_to_ha_brightness, safe_float, smoothstep,
};
