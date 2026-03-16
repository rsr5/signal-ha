//! Curator tool functions for the recorder agent.
//!
//! These functions provide **context-efficient** aggregate views of the
//! recorded data — suitable for feeding to an LLM agent that decides
//! which rows to flag for deletion.
//!
//! Every function takes a `&dyn RecordStore` and returns a small,
//! serialisable struct.  No function returns more than ~50 lines of
//! JSON so the agent's context window stays clean.
//!
//! # Example
//!
//! ```rust,no_run
//! use signal_ha_recorder::{SqliteStore, curator};
//!
//! let store = SqliteStore::open("/tmp/recorder.db")?;
//! let overview = curator::db_overview(&store)?;
//! println!("{}", serde_json::to_string_pretty(&overview)?);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::store_trait::{
    AgeBucket, DomainFlagCount, DomainFlagResult, DomainStat, EntityProfile, EntitySummary,
    FlagPreview, FlagResult, RecordStore, UnflagResult,
};
use crate::DeletionReason;
use crate::RecorderError;

// ── Curator-only return types ──────────────────────────────────────

/// Top-level database health snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct DbOverview {
    pub total_rows: u64,
    pub unflagged_rows: u64,
    pub flagged_rows: u64,
    pub flagged_by_reason: Vec<ReasonCount>,
    pub distinct_entities: u64,
    pub oldest_record: Option<DateTime<Utc>>,
    pub newest_record: Option<DateTime<Utc>>,
    pub rows_per_hour: f64,
}

/// Count of rows for a single deletion reason.
#[derive(Debug, Clone, Serialize)]
pub struct ReasonCount {
    pub reason: String,
    pub count: u64,
}

/// Summary of all flagging activity.
#[derive(Debug, Clone, Serialize)]
pub struct FlaggedSummary {
    pub by_reason: Vec<ReasonCount>,
    pub by_domain: Vec<DomainFlagCount>,
    pub total_flagged: u64,
    pub total_unflagged: u64,
}

/// Growth projection.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionEstimate {
    pub current_rows: u64,
    pub current_flagged: u64,
    pub rows_per_hour: f64,
    pub projected_days: u64,
    pub projected_total_rows: u64,
    pub projected_after_pruning: u64,
}

// ── Tool functions ─────────────────────────────────────────────────

/// One-shot database health overview.
pub fn db_overview(store: &dyn RecordStore) -> Result<DbOverview, RecorderError> {
    let total_rows = store.count(None)?;
    let flagged_by_reason = store.flagged_counts_by_reason()?;
    let flagged_rows: u64 = flagged_by_reason.iter().map(|(_, c)| c).sum();
    let unflagged_rows = total_rows.saturating_sub(flagged_rows);

    let distinct_entities = store.entities()?.len() as u64;
    let (oldest_record, newest_record) = store.time_range()?;

    let rows_per_hour = match (&oldest_record, &newest_record) {
        (Some(oldest), Some(newest)) => {
            let hours = (*newest - *oldest).num_seconds() as f64 / 3600.0;
            if hours > 0.0 {
                total_rows as f64 / hours
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    let flagged_by_reason = flagged_by_reason
        .into_iter()
        .map(|(r, count)| ReasonCount {
            reason: r.as_str().to_string(),
            count,
        })
        .collect();

    Ok(DbOverview {
        total_rows,
        unflagged_rows,
        flagged_rows,
        flagged_by_reason,
        distinct_entities,
        oldest_record,
        newest_record,
        rows_per_hour,
    })
}

/// Per-domain aggregate breakdown.
pub fn domain_stats(store: &dyn RecordStore) -> Result<Vec<DomainStat>, RecorderError> {
    store.domain_stats()
}

/// Top N entities by row count, optionally filtered to a domain.
pub fn top_entities(
    store: &dyn RecordStore,
    n: usize,
    domain: Option<&str>,
) -> Result<Vec<EntitySummary>, RecorderError> {
    store.top_entities(n, domain)
}

/// Top N fastest-updating entities, optionally filtered to a domain.
pub fn fastest_entities(
    store: &dyn RecordStore,
    n: usize,
    domain: Option<&str>,
) -> Result<Vec<EntitySummary>, RecorderError> {
    store.fastest_entities(n, domain)
}

/// Detailed profile of a single entity.
pub fn entity_profile(
    store: &dyn RecordStore,
    entity_id: &str,
) -> Result<EntityProfile, RecorderError> {
    store.entity_profile(entity_id)
}

/// Row count broken down by age buckets.
pub fn age_distribution(store: &dyn RecordStore) -> Result<Vec<AgeBucket>, RecorderError> {
    store.age_distribution()
}

/// Flag rows for a specific entity.
///
/// - `reason`: why the rows are being flagged
/// - `older_than_hours`: if set, only flag rows older than this many hours ago
/// - `keep_every_n`: if set, keep every Nth row (downsample) — only flag
///   the rows *between* the kept ones
pub fn flag_entity(
    store: &dyn RecordStore,
    entity_id: &str,
    reason: DeletionReason,
    older_than_hours: Option<u64>,
    keep_every_n: Option<u64>,
) -> Result<FlagResult, RecorderError> {
    let cutoff = older_than_hours.map(|h| Utc::now() - Duration::hours(h as i64));
    store.flag_entity(entity_id, reason, cutoff, keep_every_n)
}

/// Bulk-flag all rows for entities in a domain.
pub fn flag_domain(
    store: &dyn RecordStore,
    domain: &str,
    reason: DeletionReason,
    older_than_hours: Option<u64>,
) -> Result<DomainFlagResult, RecorderError> {
    let cutoff = older_than_hours.map(|h| Utc::now() - Duration::hours(h as i64));
    store.flag_domain(domain, reason, cutoff)
}

/// Remove flags from rows (corrections).
pub fn unflag(
    store: &dyn RecordStore,
    entity_id: Option<&str>,
    reason: Option<DeletionReason>,
) -> Result<UnflagResult, RecorderError> {
    store.unflag(entity_id, reason)
}

/// Preview how `flag_entity` would behave without changing data.
pub fn flag_preview(
    store: &dyn RecordStore,
    entity_id: &str,
    older_than_hours: Option<u64>,
    keep_every_n: Option<u64>,
) -> Result<FlagPreview, RecorderError> {
    let cutoff = older_than_hours.map(|h| Utc::now() - Duration::hours(h as i64));
    store.flag_preview(entity_id, cutoff, keep_every_n)
}

/// Summary of all flagging work so far.
pub fn flagged_summary(store: &dyn RecordStore) -> Result<FlaggedSummary, RecorderError> {
    let by_reason_raw = store.flagged_counts_by_reason()?;
    let total_flagged: u64 = by_reason_raw.iter().map(|(_, c)| c).sum();
    let total_rows = store.count(None)?;
    let total_unflagged = total_rows.saturating_sub(total_flagged);

    let by_reason = by_reason_raw
        .into_iter()
        .map(|(r, count)| ReasonCount {
            reason: r.as_str().to_string(),
            count,
        })
        .collect();

    let by_domain = store.flagged_counts_by_domain()?;

    Ok(FlaggedSummary {
        by_reason,
        by_domain,
        total_flagged,
        total_unflagged,
    })
}

/// Growth projection over N days.
pub fn retention_estimate(
    store: &dyn RecordStore,
    days: u64,
) -> Result<RetentionEstimate, RecorderError> {
    let current_rows = store.count(None)?;
    let flagged_raw = store.flagged_counts_by_reason()?;
    let current_flagged: u64 = flagged_raw.iter().map(|(_, c)| c).sum();

    let (oldest, newest) = store.time_range()?;
    let rows_per_hour = match (&oldest, &newest) {
        (Some(o), Some(n)) => {
            let hours = (*n - *o).num_seconds() as f64 / 3600.0;
            if hours > 0.0 {
                current_rows as f64 / hours
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    let projected_new = (rows_per_hour * 24.0 * days as f64) as u64;
    let projected_total_rows = current_rows + projected_new;
    let flag_ratio = if current_rows > 0 {
        current_flagged as f64 / current_rows as f64
    } else {
        0.0
    };
    let projected_flagged = current_flagged + (projected_new as f64 * flag_ratio) as u64;
    let projected_after_pruning = projected_total_rows.saturating_sub(projected_flagged);

    Ok(RetentionEstimate {
        current_rows,
        current_flagged,
        rows_per_hour,
        projected_days: days,
        projected_total_rows,
        projected_after_pruning,
    })
}

/// Permanently delete all flagged rows whose `flagged_at` is older
/// than `grace_hours` ago.  Returns the number of deleted rows.
pub fn prune_flagged(
    store: &dyn RecordStore,
    grace_hours: u64,
) -> Result<u64, RecorderError> {
    let cutoff = Utc::now() - Duration::hours(grace_hours as i64);
    store.prune_flagged(cutoff)
}
