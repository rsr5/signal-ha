use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::RecorderError;

/// A single recorded state-change entry.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub entity_id: String,
    pub state: String,
    pub attributes: Option<serde_json::Value>,
    pub timestamp: DateTime<Utc>,
}

/// Pluggable storage backend for the recorder.
///
/// Implement this trait to add a new storage backend (SQLite, MySQL,
/// Postgres, Parquet, etc.).  All methods are synchronous — wrap in
/// `tokio::task::spawn_blocking` if the backend does blocking I/O.
pub trait RecordStore: Send + Sync {
    /// Insert a state record. Implementations should silently ignore
    /// exact duplicates (same entity_id + timestamp + state).
    fn record(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Option<&serde_json::Value>,
        timestamp: DateTime<Utc>,
    ) -> Result<(), RecorderError>;

    /// Query records for an entity within a time range (inclusive), ordered by timestamp ASC.
    fn query(
        &self,
        entity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<Record>, RecorderError>;

    /// Get the most recent record for an entity.
    fn latest(&self, entity_id: &str) -> Result<Option<Record>, RecorderError>;

    /// List all distinct entity IDs in the store.
    fn entities(&self) -> Result<Vec<String>, RecorderError>;

    /// Count records for an entity (or all if None).
    fn count(&self, entity_id: Option<&str>) -> Result<u64, RecorderError>;

    /// Delete records older than the given timestamp. Returns rows deleted.
    fn prune(&self, older_than: DateTime<Utc>) -> Result<u64, RecorderError>;
}
