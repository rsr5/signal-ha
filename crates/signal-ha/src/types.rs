//! Core types shared across the library.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The current state of a Home Assistant entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityState {
    /// The entity's state value (e.g. "on", "off", "23.5").
    pub state: String,
    /// Arbitrary key-value attributes (brightness, friendly_name, etc.).
    pub attributes: serde_json::Value,
    /// When this state was last changed.
    pub last_changed: DateTime<Utc>,
}

/// A state change event received from a subscription.
#[derive(Debug, Clone)]
pub struct StateChange {
    /// The entity that changed.
    pub entity_id: String,
    /// Previous state (None on first event after subscribe).
    pub old: Option<EntityState>,
    /// New state.
    pub new: Option<EntityState>,
}
