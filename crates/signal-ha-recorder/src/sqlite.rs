use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};

use crate::deletion_reason::DeletionReason;
use crate::error::RecorderError;
use crate::store_trait::{
    AgeBucket, DomainFlagCount, DomainFlagResult, DomainStat, EntityProfile, EntitySummary,
    FlagPreview, FlagResult, Record, RecordStore, RecentChange, StateCount, UnflagResult,
};

/// SQLite-backed storage — good for development and lightweight deployments.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (or create) the SQLite database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RecorderError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "foreign_keys", "on")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Create an in-memory store (useful for tests).
    pub fn open_in_memory() -> Result<Self, RecorderError> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), RecorderError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS state_log (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                entity_id        TEXT NOT NULL,
                state            TEXT NOT NULL,
                attributes       TEXT,
                timestamp        TEXT NOT NULL,
                deletion_reason  INTEGER,
                flagged_at       TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_state_log_entity_ts
                ON state_log(entity_id, timestamp);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_state_log_dedup
                ON state_log(entity_id, timestamp, state);
            CREATE INDEX IF NOT EXISTS idx_state_log_deletion
                ON state_log(deletion_reason);",
        )?;
        // Migration for existing tables: add columns if missing.
        let has_deletion = conn
            .prepare("SELECT deletion_reason FROM state_log LIMIT 0")
            .is_ok();
        if !has_deletion {
            conn.execute_batch(
                "ALTER TABLE state_log ADD COLUMN deletion_reason INTEGER;
                 ALTER TABLE state_log ADD COLUMN flagged_at TEXT;
                 CREATE INDEX IF NOT EXISTS idx_state_log_deletion
                     ON state_log(deletion_reason);",
            )?;
        }
        Ok(())
    }
}

impl RecordStore for SqliteStore {
    fn record(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Option<&serde_json::Value>,
        timestamp: DateTime<Utc>,
    ) -> Result<(), RecorderError> {
        let conn = self.conn.lock().unwrap();
        let ts = timestamp.to_rfc3339();
        let attrs = attributes.map(|a| a.to_string());
        conn.execute(
            "INSERT OR IGNORE INTO state_log (entity_id, state, attributes, timestamp)
             VALUES (?1, ?2, ?3, ?4)",
            params![entity_id, state, attrs, ts],
        )?;
        Ok(())
    }

    fn query(
        &self,
        entity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<Record>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT entity_id, state, attributes, timestamp
             FROM state_log
             WHERE entity_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3
             ORDER BY timestamp ASC",
        )?;
        let rows = stmt
            .query_map(
                params![entity_id, from.to_rfc3339(), to.to_rfc3339()],
                row_to_record,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn latest(&self, entity_id: &str) -> Result<Option<Record>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT entity_id, state, attributes, timestamp
             FROM state_log
             WHERE entity_id = ?1
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![entity_id], row_to_record)?;
        match rows.next() {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    fn entities(&self) -> Result<Vec<String>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT DISTINCT entity_id FROM state_log ORDER BY entity_id")?;
        let rows = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    fn count(&self, entity_id: Option<&str>) -> Result<u64, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = match entity_id {
            Some(eid) => conn.query_row(
                "SELECT COUNT(*) FROM state_log WHERE entity_id = ?1",
                params![eid],
                |row| row.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM state_log", [], |row| row.get(0))?,
        };
        Ok(n as u64)
    }

    fn prune(&self, older_than: DateTime<Utc>) -> Result<u64, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM state_log WHERE timestamp < ?1",
            params![older_than.to_rfc3339()],
        )?;
        Ok(deleted as u64)
    }

    // ── Curator: aggregates ────────────────────────────────────────

    fn time_range(&self) -> Result<(Option<DateTime<Utc>>, Option<DateTime<Utc>>), RecorderError> {
        let conn = self.conn.lock().unwrap();
        let oldest: Option<String> = conn.query_row(
            "SELECT MIN(timestamp) FROM state_log",
            [],
            |row| row.get(0),
        )?;
        let newest: Option<String> = conn.query_row(
            "SELECT MAX(timestamp) FROM state_log",
            [],
            |row| row.get(0),
        )?;
        Ok((parse_rfc3339_opt(&oldest), parse_rfc3339_opt(&newest)))
    }

    fn domain_stats(&self) -> Result<Vec<DomainStat>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let total: f64 = conn.query_row(
            "SELECT COUNT(*) FROM state_log",
            [],
            |row| row.get(0),
        )?;
        let mut stmt = conn.prepare(
            "SELECT
                SUBSTR(entity_id, 1, INSTR(entity_id, '.') - 1) AS domain,
                COUNT(*) AS row_count,
                COUNT(DISTINCT entity_id) AS entity_count
             FROM state_log
             GROUP BY domain
             ORDER BY row_count DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let domain: String = row.get(0)?;
                let row_count: i64 = row.get(1)?;
                let entity_count: i64 = row.get(2)?;
                let rc = row_count as u64;
                let ec = entity_count as u64;
                Ok(DomainStat {
                    domain,
                    row_count: rc,
                    entity_count: ec,
                    avg_per_entity: if ec > 0 { rc as f64 / ec as f64 } else { 0.0 },
                    pct_of_total: if total > 0.0 { rc as f64 / total * 100.0 } else { 0.0 },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn top_entities(
        &self,
        n: usize,
        domain: Option<&str>,
    ) -> Result<Vec<EntitySummary>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let (sql, filter) = entity_summary_sql(domain, "row_count DESC");
        let mut stmt = conn.prepare(&format!(
            "{sql} LIMIT ?{}",
            if filter.is_some() { 2 } else { 1 }
        ))?;
        let rows = if let Some(prefix) = filter {
            stmt.query_map(params![prefix, n as i64], map_entity_summary)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![n as i64], map_entity_summary)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    fn fastest_entities(
        &self,
        n: usize,
        domain: Option<&str>,
    ) -> Result<Vec<EntitySummary>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        // Only consider entities with >= 2 rows (need two points for an interval).
        let (sql, filter) =
            entity_summary_sql(domain, "avg_interval_secs ASC");
        let full_sql = format!(
            "{sql} LIMIT ?{}",
            if filter.is_some() { 2 } else { 1 }
        );
        let mut stmt = conn.prepare(&full_sql)?;
        let rows = if let Some(prefix) = filter {
            stmt.query_map(params![prefix, n as i64], map_entity_summary)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![n as i64], map_entity_summary)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    fn entity_profile(&self, entity_id: &str) -> Result<EntityProfile, RecorderError> {
        let conn = self.conn.lock().unwrap();

        let row_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM state_log WHERE entity_id = ?1",
            params![entity_id],
            |row| row.get(0),
        )?;

        let distinct_state_count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT state) FROM state_log WHERE entity_id = ?1",
            params![entity_id],
            |row| row.get(0),
        )?;

        let time_span: (Option<String>, Option<String>) = conn.query_row(
            "SELECT MIN(timestamp), MAX(timestamp) FROM state_log WHERE entity_id = ?1",
            params![entity_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let avg_interval_secs = match (&time_span.0, &time_span.1) {
            (Some(min_s), Some(max_s)) if row_count > 1 => {
                let min_dt = parse_rfc3339(min_s);
                let max_dt = parse_rfc3339(max_s);
                match (min_dt, max_dt) {
                    (Some(a), Some(b)) => {
                        Some((b - a).num_milliseconds() as f64 / 1000.0 / (row_count - 1) as f64)
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        // Top-10 state histogram
        let mut hist_stmt = conn.prepare(
            "SELECT state, COUNT(*) AS cnt FROM state_log
             WHERE entity_id = ?1
             GROUP BY state ORDER BY cnt DESC LIMIT 10",
        )?;
        let state_histogram = hist_stmt
            .query_map(params![entity_id], |row| {
                Ok(StateCount {
                    state: row.get(0)?,
                    count: row.get::<_, i64>(1)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Last 10 changes
        let mut recent_stmt = conn.prepare(
            "SELECT state, timestamp FROM state_log
             WHERE entity_id = ?1
             ORDER BY timestamp DESC LIMIT 10",
        )?;
        let recent_changes = recent_stmt
            .query_map(params![entity_id], |row| {
                let ts: String = row.get(1)?;
                Ok(RecentChange {
                    state: row.get(0)?,
                    timestamp: parse_rfc3339(&ts).unwrap_or_else(Utc::now),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let flagged_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM state_log
             WHERE entity_id = ?1 AND deletion_reason IS NOT NULL",
            params![entity_id],
            |row| row.get(0),
        )?;

        let rc = row_count as u64;
        let fc = flagged_count as u64;

        Ok(EntityProfile {
            entity_id: entity_id.to_string(),
            row_count: rc,
            avg_interval_secs,
            distinct_state_count: distinct_state_count as u64,
            state_histogram,
            recent_changes,
            flagged_count: fc,
            pct_flagged: if rc > 0 { fc as f64 / rc as f64 * 100.0 } else { 0.0 },
        })
    }

    fn age_distribution(&self) -> Result<Vec<AgeBucket>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now();
        let boundaries = age_boundaries(now);
        let mut result = Vec::new();
        for (label, from, to) in &boundaries {
            let cnt: i64 = conn.query_row(
                "SELECT COUNT(*) FROM state_log WHERE timestamp >= ?1 AND timestamp < ?2",
                params![from.to_rfc3339(), to.to_rfc3339()],
                |row| row.get(0),
            )?;
            result.push(AgeBucket {
                label: label.to_string(),
                row_count: cnt as u64,
            });
        }
        Ok(result)
    }

    // ── Curator: flagging ──────────────────────────────────────────

    fn flag_entity(
        &self,
        entity_id: &str,
        reason: DeletionReason,
        cutoff: Option<DateTime<Utc>>,
        keep_every_n: Option<u64>,
    ) -> Result<FlagResult, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let reason_val = reason.as_u8() as i64;

        let rows_flagged = if let Some(n) = keep_every_n {
            // Downsample: flag all except every Nth row.
            // Use ROW_NUMBER to pick which rows to keep.
            let cutoff_clause = cutoff
                .map(|c| format!("AND timestamp < '{}'", c.to_rfc3339()))
                .unwrap_or_default();
            let sql = format!(
                "UPDATE state_log SET deletion_reason = ?1, flagged_at = ?2
                 WHERE id IN (
                     SELECT id FROM (
                         SELECT id, ROW_NUMBER() OVER (
                             PARTITION BY entity_id ORDER BY timestamp
                         ) AS rn
                         FROM state_log
                         WHERE entity_id = ?3
                           AND deletion_reason IS NULL
                           {cutoff_clause}
                     ) WHERE (rn - 1) % ?4 != 0
                 )"
            );
            conn.execute(
                &sql,
                params![reason_val, now, entity_id, n as i64],
            )? as u64
        } else {
            let cutoff_clause = cutoff
                .map(|c| format!("AND timestamp < '{}'", c.to_rfc3339()))
                .unwrap_or_default();
            let sql = format!(
                "UPDATE state_log SET deletion_reason = ?1, flagged_at = ?2
                 WHERE entity_id = ?3 AND deletion_reason IS NULL {cutoff_clause}"
            );
            conn.execute(&sql, params![reason_val, now, entity_id])? as u64
        };

        Ok(FlagResult { rows_flagged })
    }

    fn flag_domain(
        &self,
        domain: &str,
        reason: DeletionReason,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<DomainFlagResult, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let reason_val = reason.as_u8() as i64;
        let prefix = format!("{domain}.%");

        // Count distinct entities that will be affected.
        let cutoff_clause = cutoff
            .map(|c| format!("AND timestamp < '{}'", c.to_rfc3339()))
            .unwrap_or_default();

        let entities_affected: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT entity_id) FROM state_log
                 WHERE entity_id LIKE ?1 AND deletion_reason IS NULL {cutoff_clause}"
            ),
            params![prefix],
            |row| row.get(0),
        )?;

        let sql = format!(
            "UPDATE state_log SET deletion_reason = ?1, flagged_at = ?2
             WHERE entity_id LIKE ?3 AND deletion_reason IS NULL {cutoff_clause}"
        );
        let rows_flagged = conn.execute(&sql, params![reason_val, now, prefix])? as u64;

        Ok(DomainFlagResult {
            rows_flagged,
            entities_affected: entities_affected as u64,
        })
    }

    fn unflag(
        &self,
        entity_id: Option<&str>,
        reason: Option<DeletionReason>,
    ) -> Result<UnflagResult, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let mut conditions = vec!["deletion_reason IS NOT NULL".to_string()];
        if let Some(eid) = entity_id {
            conditions.push(format!("entity_id = '{eid}'"));
        }
        if let Some(r) = reason {
            conditions.push(format!("deletion_reason = {}", r.as_u8()));
        }
        let where_clause = conditions.join(" AND ");
        let sql = format!(
            "UPDATE state_log SET deletion_reason = NULL, flagged_at = NULL WHERE {where_clause}"
        );
        let rows_unflagged = conn.execute(&sql, [])? as u64;
        Ok(UnflagResult { rows_unflagged })
    }

    fn flagged_counts_by_reason(&self) -> Result<Vec<(DeletionReason, u64)>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT deletion_reason, COUNT(*) FROM state_log
             WHERE deletion_reason IS NOT NULL
             GROUP BY deletion_reason ORDER BY deletion_reason",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let reason_val: i64 = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok((reason_val as u8, count as u64))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .filter_map(|(r, c)| DeletionReason::from_u8(r).map(|reason| (reason, c)))
            .collect())
    }

    fn flagged_counts_by_domain(&self) -> Result<Vec<DomainFlagCount>, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT SUBSTR(entity_id, 1, INSTR(entity_id, '.') - 1) AS domain,
                    COUNT(*) AS cnt
             FROM state_log
             WHERE deletion_reason IS NOT NULL
             GROUP BY domain ORDER BY cnt DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DomainFlagCount {
                    domain: row.get(0)?,
                    count: row.get::<_, i64>(1)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn flag_preview(
        &self,
        entity_id: &str,
        cutoff: Option<DateTime<Utc>>,
        keep_every_n: Option<u64>,
    ) -> Result<FlagPreview, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let cutoff_clause = cutoff
            .map(|c| format!("AND timestamp < '{}'", c.to_rfc3339()))
            .unwrap_or_default();

        let total_rows: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM state_log
                 WHERE entity_id = ?1 AND deletion_reason IS NULL {cutoff_clause}"
            ),
            params![entity_id],
            |row| row.get(0),
        )?;

        let n = keep_every_n.unwrap_or(1).max(1);
        let would_keep = if n > 0 {
            (total_rows as u64 + n - 1) / n
        } else {
            0
        };
        let would_flag = (total_rows as u64).saturating_sub(would_keep);

        let pct_reduction = if total_rows > 0 {
            would_flag as f64 / total_rows as f64 * 100.0
        } else {
            0.0
        };

        // Sample 5 kept timestamps (evenly spaced from the kept set).
        let sample_kept_timestamps = if n > 1 {
            let mut stmt = conn.prepare(&format!(
                "SELECT timestamp FROM (
                     SELECT timestamp, ROW_NUMBER() OVER (ORDER BY timestamp) AS rn
                     FROM state_log
                     WHERE entity_id = ?1 AND deletion_reason IS NULL {cutoff_clause}
                 ) WHERE (rn - 1) % ?2 = 0"
            ))?;
            let all_kept: Vec<DateTime<Utc>> = stmt
                .query_map(params![entity_id, n as i64], |row| {
                    let ts: String = row.get(0)?;
                    Ok(parse_rfc3339(&ts).unwrap_or_else(Utc::now))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            sample_n(&all_kept, 5)
        } else {
            Vec::new()
        };

        Ok(FlagPreview {
            total_rows: total_rows as u64,
            would_flag,
            would_keep,
            pct_reduction,
            sample_kept_timestamps,
        })
    }

    fn prune_flagged(&self, grace_cutoff: DateTime<Utc>) -> Result<u64, RecorderError> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM state_log
             WHERE deletion_reason IS NOT NULL AND flagged_at < ?1",
            params![grace_cutoff.to_rfc3339()],
        )?;
        Ok(deleted as u64)
    }

    fn rotate_for_curation(
        &self,
        _curating_qualified: &str,
        _archive_qualified: &str,
    ) -> Result<(), RecorderError> {
        // SQLite has no cross-database atomic rename; the snapshot/curate
        // workflow is a MySQL-only feature.  Return an explicit error so
        // misuse is loud rather than silent.
        Err(RecorderError::Other(
            "rotate_for_curation is not supported on SqliteStore — use MysqlStore in production".into(),
        ))
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> Result<Record, rusqlite::Error> {
    let entity_id: String = row.get(0)?;
    let state: String = row.get(1)?;
    let attrs_str: Option<String> = row.get(2)?;
    let ts: String = row.get(3)?;
    let attributes = attrs_str.and_then(|s| serde_json::from_str(&s).ok());
    let timestamp = DateTime::parse_from_rfc3339(&ts)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    Ok(Record {
        entity_id,
        state,
        attributes,
        timestamp,
    })
}

// ── Helpers for curator methods ────────────────────────────────────

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn parse_rfc3339_opt(s: &Option<String>) -> Option<DateTime<Utc>> {
    s.as_deref().and_then(parse_rfc3339)
}

/// Build the reusable SELECT for top_entities / fastest_entities.
/// Returns (sql, optional LIKE-prefix param).
fn entity_summary_sql(domain: Option<&str>, order: &str) -> (String, Option<String>) {
    let domain_filter = domain.map(|d| format!("{d}.%"));
    let where_clause = if domain_filter.is_some() {
        "WHERE entity_id LIKE ?1"
    } else {
        ""
    };
    let sql = format!(
        "SELECT entity_id,
                COUNT(*) AS row_count,
                (JULIANDAY(MAX(timestamp)) - JULIANDAY(MIN(timestamp))) * 86400.0
                    / NULLIF(COUNT(*) - 1, 0) AS avg_interval_secs,
                MIN(timestamp) AS first_seen,
                MAX(timestamp) AS last_seen,
                COUNT(DISTINCT state) AS distinct_states
         FROM state_log
         {where_clause}
         GROUP BY entity_id
         HAVING COUNT(*) >= 2
         ORDER BY {order}"
    );
    (sql, domain_filter)
}

fn map_entity_summary(row: &rusqlite::Row<'_>) -> Result<EntitySummary, rusqlite::Error> {
    let first_seen: Option<String> = row.get(3)?;
    let last_seen: Option<String> = row.get(4)?;
    Ok(EntitySummary {
        entity_id: row.get(0)?,
        row_count: row.get::<_, i64>(1)? as u64,
        avg_interval_secs: row.get(2)?,
        distinct_states: Some(row.get::<_, i64>(5)? as u64),
        first_seen: first_seen.as_deref().and_then(parse_rfc3339),
        last_seen: last_seen.as_deref().and_then(parse_rfc3339),
    })
}

/// Fixed age buckets for age_distribution.
fn age_boundaries(now: DateTime<Utc>) -> Vec<(&'static str, DateTime<Utc>, DateTime<Utc>)> {
    let far_future = now + Duration::days(365 * 100);
    vec![
        ("< 1h", now - Duration::hours(1), far_future),
        ("1h-6h", now - Duration::hours(6), now - Duration::hours(1)),
        ("6h-24h", now - Duration::hours(24), now - Duration::hours(6)),
        ("1d-7d", now - Duration::days(7), now - Duration::hours(24)),
        ("7d-30d", now - Duration::days(30), now - Duration::days(7)),
        ("30d-90d", now - Duration::days(90), now - Duration::days(30)),
        ("90d+", now - Duration::days(365 * 100), now - Duration::days(90)),
    ]
}

/// Sample `n` evenly-spaced items from a slice.
fn sample_n<T: Clone>(items: &[T], n: usize) -> Vec<T> {
    let len = items.len();
    if len <= n {
        return items.to_vec();
    }
    (0..n)
        .map(|i| items[i * len / n].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(hour: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 1, 15, hour, min, 0).unwrap()
    }

    #[test]
    fn record_and_query() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.record("sensor.temp", "21.5", None, ts(10, 0)).unwrap();
        store.record("sensor.temp", "22.0", None, ts(10, 30)).unwrap();
        store.record("sensor.temp", "22.3", None, ts(11, 0)).unwrap();

        let records = store.query("sensor.temp", ts(10, 0), ts(11, 0)).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].state, "21.5");
        assert_eq!(records[2].state, "22.3");
    }

    #[test]
    fn dedup_ignores_exact_duplicate() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.record("sensor.temp", "21.5", None, ts(10, 0)).unwrap();
        store.record("sensor.temp", "21.5", None, ts(10, 0)).unwrap();
        assert_eq!(store.count(Some("sensor.temp")).unwrap(), 1);
    }

    #[test]
    fn prune_removes_old_records() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.record("sensor.temp", "20.0", None, ts(8, 0)).unwrap();
        store.record("sensor.temp", "21.0", None, ts(10, 0)).unwrap();
        store.record("sensor.temp", "22.0", None, ts(12, 0)).unwrap();

        let deleted = store.prune(ts(10, 0)).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(store.count(Some("sensor.temp")).unwrap(), 2);
    }

    #[test]
    fn entities_lists_distinct() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.record("sensor.temp", "21.0", None, ts(10, 0)).unwrap();
        store.record("light.porch", "on", None, ts(10, 0)).unwrap();
        store.record("sensor.temp", "22.0", None, ts(10, 30)).unwrap();

        let entities = store.entities().unwrap();
        assert_eq!(entities, vec!["light.porch", "sensor.temp"]);
    }

    #[test]
    fn latest_returns_most_recent() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.record("sensor.temp", "20.0", None, ts(10, 0)).unwrap();
        store.record("sensor.temp", "23.0", None, ts(12, 0)).unwrap();

        let r = store.latest("sensor.temp").unwrap().unwrap();
        assert_eq!(r.state, "23.0");
    }

    #[test]
    fn latest_returns_none_for_unknown() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.latest("sensor.nope").unwrap().is_none());
    }

    #[test]
    fn record_with_attributes() {
        let store = SqliteStore::open_in_memory().unwrap();
        let attrs = serde_json::json!({"friendly_name": "Temperature", "unit": "°C"});
        store
            .record("sensor.temp", "21.5", Some(&attrs), ts(10, 0))
            .unwrap();

        let r = store.latest("sensor.temp").unwrap().unwrap();
        let a = r.attributes.unwrap();
        assert_eq!(a["friendly_name"], "Temperature");
        assert_eq!(a["unit"], "°C");
    }

    // ── Curator tests ──────────────────────────────────────────────

    /// Populate a store with realistic multi-domain data for curator tests.
    fn seed_store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().unwrap();
        // sensor.temp — moderate frequency (6 rows)
        for m in (0..60).step_by(10) {
            store
                .record("sensor.temp", &format!("{}.0", 20 + m / 10), None, ts(10, m))
                .unwrap();
        }
        // sensor.radar — high frequency (20 rows, 1-min intervals)
        for m in 0..20 {
            store
                .record("sensor.radar", &format!("{}", m % 5), None, ts(10, m))
                .unwrap();
        }
        // light.porch — toggle (4 rows)
        store.record("light.porch", "on", None, ts(10, 0)).unwrap();
        store.record("light.porch", "off", None, ts(10, 30)).unwrap();
        store.record("light.porch", "on", None, ts(11, 0)).unwrap();
        store.record("light.porch", "off", None, ts(11, 30)).unwrap();
        // binary_sensor.motion — 2 rows
        store.record("binary_sensor.motion", "on", None, ts(10, 15)).unwrap();
        store.record("binary_sensor.motion", "off", None, ts(10, 45)).unwrap();
        store
    }

    #[test]
    fn time_range_with_data() {
        let store = seed_store();
        let (oldest, newest) = store.time_range().unwrap();
        assert_eq!(oldest.unwrap(), ts(10, 0));
        assert_eq!(newest.unwrap(), ts(11, 30));
    }

    #[test]
    fn time_range_empty_db() {
        let store = SqliteStore::open_in_memory().unwrap();
        let (oldest, newest) = store.time_range().unwrap();
        assert!(oldest.is_none());
        assert!(newest.is_none());
    }

    #[test]
    fn domain_stats_returns_all_domains() {
        let store = seed_store();
        let stats = store.domain_stats().unwrap();
        let domains: Vec<&str> = stats.iter().map(|s| s.domain.as_str()).collect();
        assert!(domains.contains(&"sensor"));
        assert!(domains.contains(&"light"));
        assert!(domains.contains(&"binary_sensor"));

        let sensor = stats.iter().find(|s| s.domain == "sensor").unwrap();
        assert_eq!(sensor.entity_count, 2); // temp + radar
        assert_eq!(sensor.row_count, 26); // 6 + 20

        let total_pct: f64 = stats.iter().map(|s| s.pct_of_total).sum();
        assert!((total_pct - 100.0).abs() < 0.1);
    }

    #[test]
    fn top_entities_returns_in_order() {
        let store = seed_store();
        let top = store.top_entities(3, None).unwrap();
        assert_eq!(top[0].entity_id, "sensor.radar"); // 20 rows
        assert_eq!(top[0].row_count, 20);
        assert_eq!(top[1].entity_id, "sensor.temp"); // 6 rows
    }

    #[test]
    fn top_entities_domain_filter() {
        let store = seed_store();
        let top = store.top_entities(10, Some("light")).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].entity_id, "light.porch");
    }

    #[test]
    fn fastest_entities_returns_shortest_interval() {
        let store = seed_store();
        let fast = store.fastest_entities(2, None).unwrap();
        // sensor.radar updates every 1 min ≈ 60s, should be fastest
        assert_eq!(fast[0].entity_id, "sensor.radar");
        let interval = fast[0].avg_interval_secs.unwrap();
        assert!(interval > 55.0 && interval < 65.0, "got {interval}");
    }

    #[test]
    fn entity_profile_populated() {
        let store = seed_store();
        let profile = store.entity_profile("sensor.radar").unwrap();
        assert_eq!(profile.row_count, 20);
        assert_eq!(profile.distinct_state_count, 5); // 0,1,2,3,4
        assert_eq!(profile.flagged_count, 0);
        assert_eq!(profile.pct_flagged, 0.0);
        // state histogram should have 5 states
        assert_eq!(profile.state_histogram.len(), 5);
        // recent changes = 10 (capped)
        assert_eq!(profile.recent_changes.len(), 10);
    }

    #[test]
    fn entity_profile_unknown_entity() {
        let store = SqliteStore::open_in_memory().unwrap();
        let profile = store.entity_profile("sensor.nope").unwrap();
        assert_eq!(profile.row_count, 0);
        assert_eq!(profile.distinct_state_count, 0);
        assert!(profile.state_histogram.is_empty());
    }

    #[test]
    fn age_distribution_covers_all_rows() {
        let store = seed_store();
        let total = store.count(None).unwrap();
        let buckets = store.age_distribution().unwrap();
        // All test data is from 2025-01-15 ~10:00, which is >90d ago,
        // so everything should be in the "90d+" bucket.
        let in_90d = buckets.iter().find(|b| b.label == "90d+").unwrap();
        assert_eq!(in_90d.row_count, total);
    }

    // ── Flagging tests ─────────────────────────────────────────────

    #[test]
    fn flag_entity_all_rows() {
        let store = seed_store();
        let result = store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        assert_eq!(result.rows_flagged, 20);

        // Verify profile shows flags
        let profile = store.entity_profile("sensor.radar").unwrap();
        assert_eq!(profile.flagged_count, 20);
        assert!((profile.pct_flagged - 100.0).abs() < 0.01);
    }

    #[test]
    fn flag_entity_with_downsample() {
        let store = seed_store();
        // keep every 5th row of radar (20 rows → keep 4, flag 16)
        let result = store
            .flag_entity(
                "sensor.radar",
                DeletionReason::HighFrequency,
                None,
                Some(5),
            )
            .unwrap();
        assert_eq!(result.rows_flagged, 16);

        let profile = store.entity_profile("sensor.radar").unwrap();
        assert_eq!(profile.flagged_count, 16);
        assert_eq!(profile.row_count, 20); // rows still exist, just flagged
    }

    #[test]
    fn flag_entity_with_cutoff() {
        let store = seed_store();
        // Only flag radar rows before 10:10 — that's 10 rows (10:00..10:09)
        let result = store
            .flag_entity(
                "sensor.radar",
                DeletionReason::HighFrequency,
                Some(ts(10, 10)),
                None,
            )
            .unwrap();
        assert_eq!(result.rows_flagged, 10);
    }

    #[test]
    fn flag_entity_idempotent() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        // Flag again — already-flagged rows should not be double-flagged
        let result = store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        assert_eq!(result.rows_flagged, 0);
    }

    #[test]
    fn flag_domain_flags_all_entities_in_domain() {
        let store = seed_store();
        let result = store
            .flag_domain("sensor", DeletionReason::LowValue, None)
            .unwrap();
        assert_eq!(result.entities_affected, 2); // temp + radar
        assert_eq!(result.rows_flagged, 26); // 6 + 20
    }

    #[test]
    fn unflag_by_entity() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        let result = store.unflag(Some("sensor.radar"), None).unwrap();
        assert_eq!(result.rows_unflagged, 20);

        let profile = store.entity_profile("sensor.radar").unwrap();
        assert_eq!(profile.flagged_count, 0);
    }

    #[test]
    fn unflag_by_reason() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        store
            .flag_entity("sensor.temp", DeletionReason::LowValue, None, None)
            .unwrap();

        let result = store.unflag(None, Some(DeletionReason::HighFrequency)).unwrap();
        assert_eq!(result.rows_unflagged, 20);

        // temp should still be flagged
        let profile = store.entity_profile("sensor.temp").unwrap();
        assert_eq!(profile.flagged_count, 6);
    }

    #[test]
    fn flagged_counts_by_reason() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        store
            .flag_entity("sensor.temp", DeletionReason::LowValue, None, None)
            .unwrap();

        let counts = store.flagged_counts_by_reason().unwrap();
        assert_eq!(counts.len(), 2);
        let hf = counts
            .iter()
            .find(|(r, _)| *r == DeletionReason::HighFrequency)
            .unwrap();
        assert_eq!(hf.1, 20);
        let lv = counts
            .iter()
            .find(|(r, _)| *r == DeletionReason::LowValue)
            .unwrap();
        assert_eq!(lv.1, 6);
    }

    #[test]
    fn flagged_counts_by_domain() {
        let store = seed_store();
        store
            .flag_domain("sensor", DeletionReason::HighFrequency, None)
            .unwrap();
        let counts = store.flagged_counts_by_domain().unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].domain, "sensor");
        assert_eq!(counts[0].count, 26);
    }

    #[test]
    fn flag_preview_no_downsample() {
        let store = seed_store();
        let preview = store.flag_preview("sensor.radar", None, None).unwrap();
        assert_eq!(preview.total_rows, 20);
        assert_eq!(preview.would_keep, 20); // keep_every_n=1 means keep all
        assert_eq!(preview.would_flag, 0);
    }

    #[test]
    fn flag_preview_with_downsample() {
        let store = seed_store();
        let preview = store
            .flag_preview("sensor.radar", None, Some(5))
            .unwrap();
        assert_eq!(preview.total_rows, 20);
        assert_eq!(preview.would_keep, 4);
        assert_eq!(preview.would_flag, 16);
        assert!((preview.pct_reduction - 80.0).abs() < 0.1);
        assert!(preview.sample_kept_timestamps.len() <= 5);
    }

    #[test]
    fn prune_flagged_respects_grace_period() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();

        // Grace cutoff is way in the future — nothing should be pruned
        // because flagged_at is now (≈ 2025-now) which is before far_future
        let deleted = store.prune_flagged(ts(10, 0)).unwrap();
        // flagged_at is ~Utc::now(), which is >> ts(10,0), so nothing pruned
        assert_eq!(deleted, 0);

        // Use a cutoff far in the future
        let far_future = Utc::now() + Duration::hours(1);
        let deleted = store.prune_flagged(far_future).unwrap();
        assert_eq!(deleted, 20);
        assert_eq!(store.count(Some("sensor.radar")).unwrap(), 0);
    }

    #[test]
    fn prune_flagged_leaves_unflagged_rows() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();

        let far_future = Utc::now() + Duration::hours(1);
        store.prune_flagged(far_future).unwrap();

        // Other entities should be untouched
        assert_eq!(store.count(Some("sensor.temp")).unwrap(), 6);
        assert_eq!(store.count(Some("light.porch")).unwrap(), 4);
    }

    // ── Curator module tests ───────────────────────────────────────

    #[test]
    fn curator_db_overview() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();

        let overview = crate::curator::db_overview(&store).unwrap();
        assert_eq!(overview.total_rows, 32);
        assert_eq!(overview.flagged_rows, 20);
        assert_eq!(overview.unflagged_rows, 12);
        assert_eq!(overview.distinct_entities, 4);
        assert!(overview.rows_per_hour > 0.0);
        assert_eq!(overview.flagged_by_reason.len(), 1);
        assert_eq!(overview.flagged_by_reason[0].reason, "HighFrequency");
    }

    #[test]
    fn curator_flagged_summary() {
        let store = seed_store();
        store
            .flag_entity("sensor.radar", DeletionReason::HighFrequency, None, None)
            .unwrap();
        store
            .flag_entity("sensor.temp", DeletionReason::LowValue, None, None)
            .unwrap();

        let summary = crate::curator::flagged_summary(&store).unwrap();
        assert_eq!(summary.total_flagged, 26);
        assert_eq!(summary.total_unflagged, 6); // light + binary_sensor
        assert_eq!(summary.by_reason.len(), 2);
        assert_eq!(summary.by_domain.len(), 1); // only sensor domain has flags
    }

    #[test]
    fn curator_retention_estimate() {
        let store = seed_store();
        let est = crate::curator::retention_estimate(&store, 30).unwrap();
        assert_eq!(est.current_rows, 32);
        assert_eq!(est.projected_days, 30);
        assert!(est.projected_total_rows >= 32);
    }
}
