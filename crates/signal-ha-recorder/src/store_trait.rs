use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::deletion_reason::DeletionReason;
use crate::RecorderError;

/// A single recorded state-change entry.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub entity_id: String,
    pub state: String,
    pub attributes: Option<serde_json::Value>,
    pub timestamp: DateTime<Utc>,
}

// ── Curator return types ───────────────────────────────────────────

/// Per-domain aggregate statistics.
#[derive(Debug, Clone, Serialize)]
pub struct DomainStat {
    pub domain: String,
    pub row_count: u64,
    pub entity_count: u64,
    pub avg_per_entity: f64,
    pub pct_of_total: f64,
}

/// Summary of a high-volume entity.
#[derive(Debug, Clone, Serialize)]
pub struct EntitySummary {
    pub entity_id: String,
    pub row_count: u64,
    pub avg_interval_secs: Option<f64>,
    /// `None` in bulk queries (too expensive on MySQL TEXT columns).
    /// Populated by `entity_profile`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distinct_states: Option<u64>,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
}

/// Detailed profile of a single entity.
#[derive(Debug, Clone, Serialize)]
pub struct EntityProfile {
    pub entity_id: String,
    pub row_count: u64,
    pub avg_interval_secs: Option<f64>,
    pub distinct_state_count: u64,
    pub state_histogram: Vec<StateCount>,
    pub recent_changes: Vec<RecentChange>,
    pub flagged_count: u64,
    pub pct_flagged: f64,
}

/// One bucket in a state-value histogram.
#[derive(Debug, Clone, Serialize)]
pub struct StateCount {
    pub state: String,
    pub count: u64,
}

/// A single recent state change (for the sample window).
#[derive(Debug, Clone, Serialize)]
pub struct RecentChange {
    pub state: String,
    pub timestamp: DateTime<Utc>,
}

/// Result of a flag operation.
#[derive(Debug, Clone, Serialize)]
pub struct FlagResult {
    pub rows_flagged: u64,
}

/// Result of a bulk domain flag operation.
#[derive(Debug, Clone, Serialize)]
pub struct DomainFlagResult {
    pub rows_flagged: u64,
    pub entities_affected: u64,
}

/// Result of an unflag operation.
#[derive(Debug, Clone, Serialize)]
pub struct UnflagResult {
    pub rows_unflagged: u64,
}

/// Flagged row count per domain.
#[derive(Debug, Clone, Serialize)]
pub struct DomainFlagCount {
    pub domain: String,
    pub count: u64,
}

/// Row count per age bucket.
#[derive(Debug, Clone, Serialize)]
pub struct AgeBucket {
    pub label: String,
    pub row_count: u64,
}

/// Dry-run preview of a flag operation.
#[derive(Debug, Clone, Serialize)]
pub struct FlagPreview {
    pub total_rows: u64,
    pub would_flag: u64,
    pub would_keep: u64,
    pub pct_reduction: f64,
    pub sample_kept_timestamps: Vec<DateTime<Utc>>,
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

    // ── Curator: aggregates ────────────────────────────────────────

    /// Earliest and latest record timestamps in the store.
    fn time_range(&self) -> Result<(Option<DateTime<Utc>>, Option<DateTime<Utc>>), RecorderError>;

    /// Per-domain row and entity counts.
    fn domain_stats(&self) -> Result<Vec<DomainStat>, RecorderError>;

    /// Top N entities by row count, optionally filtered by domain prefix.
    fn top_entities(
        &self,
        n: usize,
        domain: Option<&str>,
    ) -> Result<Vec<EntitySummary>, RecorderError>;

    /// Top N fastest-updating entities (lowest avg interval).
    fn fastest_entities(
        &self,
        n: usize,
        domain: Option<&str>,
    ) -> Result<Vec<EntitySummary>, RecorderError>;

    /// Detailed profile of a single entity.
    fn entity_profile(&self, entity_id: &str) -> Result<EntityProfile, RecorderError>;

    /// Row counts bucketed by age (<1h, 1-6h, 6-24h, 1-7d, 7-30d, >30d).
    fn age_distribution(&self) -> Result<Vec<AgeBucket>, RecorderError>;

    // ── Curator: flagging ──────────────────────────────────────────

    /// Flag rows for an entity. If `keep_every_n` is set, keep every Nth
    /// row and flag the rest (downsampling). If `cutoff` is set, only flag
    /// rows with timestamp before that point.
    fn flag_entity(
        &self,
        entity_id: &str,
        reason: DeletionReason,
        cutoff: Option<DateTime<Utc>>,
        keep_every_n: Option<u64>,
    ) -> Result<FlagResult, RecorderError>;

    /// Flag all unflagged rows for entities in a domain.
    fn flag_domain(
        &self,
        domain: &str,
        reason: DeletionReason,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<DomainFlagResult, RecorderError>;

    /// Remove flags from rows.
    fn unflag(
        &self,
        entity_id: Option<&str>,
        reason: Option<DeletionReason>,
    ) -> Result<UnflagResult, RecorderError>;

    /// Count flagged rows grouped by reason.
    fn flagged_counts_by_reason(&self) -> Result<Vec<(DeletionReason, u64)>, RecorderError>;

    /// Count flagged rows grouped by domain.
    fn flagged_counts_by_domain(&self) -> Result<Vec<DomainFlagCount>, RecorderError>;

    /// Preview what a flag operation would do without committing.
    fn flag_preview(
        &self,
        entity_id: &str,
        cutoff: Option<DateTime<Utc>>,
        keep_every_n: Option<u64>,
    ) -> Result<FlagPreview, RecorderError>;

    /// Delete rows that were flagged before `grace_cutoff`. Returns rows deleted.
    fn prune_flagged(&self, grace_cutoff: DateTime<Utc>) -> Result<u64, RecorderError>;

    /// Atomically rotate the live `state_log` into the curating slot,
    /// archiving any pre-existing curating snapshot first.  All renames
    /// happen in a single atomic operation where the backend supports it.
    ///
    /// `curating_qualified` — the curating-slot table (e.g.
    /// `"signal_recorder_curating.state_log"`).  Always written to.
    /// `archive_qualified` — where to move a pre-existing curating
    /// snapshot before the live → curating swap.  Only used if the
    /// curating slot was non-empty.
    ///
    /// After this returns, live writes against the active `state_log`
    /// land in a fresh empty table; the snapshot lives in the curating
    /// slot, ready for curation without contention with live writes.
    fn rotate_for_curation(
        &self,
        curating_qualified: &str,
        archive_qualified: &str,
    ) -> Result<(), RecorderError>;
}
