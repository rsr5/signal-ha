use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use mysql::prelude::*;
use mysql::{Opts, Pool, PooledConn};

use crate::deletion_reason::DeletionReason;
use crate::error::RecorderError;
use crate::store_trait::{
    AgeBucket, DomainFlagCount, DomainFlagResult, DomainStat, EntityProfile, EntitySummary,
    FlagPreview, FlagResult, Record, RecordStore, RecentChange, StateCount, UnflagResult,
};

/// MySQL-backed storage — production backend for long-term recording.
///
/// Connects via a connection pool. Supports Unix socket auth (no password)
/// or TCP with username/password.
///
/// # Examples
///
/// ```rust,no_run
/// use signal_ha_recorder::MysqlStore;
///
/// // Unix socket auth (NixOS default — user matches OS user)
/// let store = MysqlStore::open("mysql://signal@localhost/signal_recorder")?;
///
/// // TCP with password
/// let store = MysqlStore::open("mysql://signal:pass@127.0.0.1:3306/signal_recorder")?;
/// # Ok::<(), signal_ha_recorder::RecorderError>(())
/// ```
pub struct MysqlStore {
    pool: Pool,
    /// Serialize writes to avoid deadlocks on INSERT IGNORE.
    write_lock: Mutex<()>,
}

impl MysqlStore {
    /// Open a connection pool to the MySQL database.
    ///
    /// `url` is a MySQL connection URL, e.g.:
    /// - `mysql://signal@localhost/signal_recorder` (Unix socket)
    /// - `mysql://signal:pass@127.0.0.1:3306/signal_recorder` (TCP)
    pub fn open(url: &str) -> Result<Self, RecorderError> {
        let opts = Opts::from_url(url).map_err(|e| RecorderError::Other(e.to_string()))?;
        let pool = Pool::new(opts)?;
        let store = Self {
            pool,
            write_lock: Mutex::new(()),
        };
        store.migrate()?;
        Ok(store)
    }

    fn conn(&self) -> Result<PooledConn, RecorderError> {
        Ok(self.pool.get_conn()?)
    }

    fn migrate(&self) -> Result<(), RecorderError> {
        let mut conn = self.conn()?;
        conn.query_drop(
            "CREATE TABLE IF NOT EXISTS state_log (
                id               BIGINT AUTO_INCREMENT PRIMARY KEY,
                entity_id        VARCHAR(255) NOT NULL,
                state            TEXT NOT NULL,
                attributes       JSON,
                timestamp        DATETIME(3) NOT NULL,
                deletion_reason  TINYINT,
                flagged_at       DATETIME(3),
                UNIQUE KEY idx_dedup (entity_id, timestamp, state(191)),
                INDEX idx_entity_ts (entity_id, timestamp),
                INDEX idx_deletion (deletion_reason)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
        )?;
        // Migration for existing tables: add new columns if missing.
        let cols: Vec<String> = conn.exec(
            "SELECT COLUMN_NAME FROM INFORMATION_SCHEMA.COLUMNS
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'state_log'",
            (),
        )?;
        if !cols.iter().any(|c| c == "deletion_reason") {
            conn.query_drop(
                "ALTER TABLE state_log
                 ADD COLUMN deletion_reason TINYINT,
                 ADD COLUMN flagged_at DATETIME(3),
                 ADD INDEX idx_deletion (deletion_reason)",
            )?;
        }
        Ok(())
    }
}

impl RecordStore for MysqlStore {
    fn record(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Option<&serde_json::Value>,
        timestamp: DateTime<Utc>,
    ) -> Result<(), RecorderError> {
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;
        let ts = timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let attrs = attributes.map(|a| a.to_string());
        conn.exec_drop(
            "INSERT IGNORE INTO state_log (entity_id, state, attributes, timestamp)
             VALUES (?, ?, ?, ?)",
            (entity_id, state, attrs, &ts),
        )?;
        Ok(())
    }

    fn query(
        &self,
        entity_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<Record>, RecorderError> {
        let mut conn = self.conn()?;
        let from_s = from.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let to_s = to.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let rows: Vec<(String, String, Option<String>, String)> = conn.exec(
            "SELECT entity_id, state, attributes, timestamp
             FROM state_log
             WHERE entity_id = ? AND timestamp >= ? AND timestamp <= ?
             ORDER BY timestamp ASC",
            (entity_id, &from_s, &to_s),
        )?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    fn latest(&self, entity_id: &str) -> Result<Option<Record>, RecorderError> {
        let mut conn = self.conn()?;
        let rows: Vec<(String, String, Option<String>, String)> = conn.exec(
            "SELECT entity_id, state, attributes, timestamp
             FROM state_log
             WHERE entity_id = ?
             ORDER BY timestamp DESC
             LIMIT 1",
            (entity_id,),
        )?;
        Ok(rows.into_iter().next().map(row_to_record))
    }

    fn entities(&self) -> Result<Vec<String>, RecorderError> {
        let mut conn = self.conn()?;
        let rows: Vec<String> =
            conn.query("SELECT DISTINCT entity_id FROM state_log ORDER BY entity_id")?;
        Ok(rows)
    }

    fn count(&self, entity_id: Option<&str>) -> Result<u64, RecorderError> {
        let mut conn = self.conn()?;
        let n: u64 = match entity_id {
            Some(eid) => conn
                .exec_first(
                    "SELECT COUNT(*) FROM state_log WHERE entity_id = ?",
                    (eid,),
                )?
                .unwrap_or(0),
            None => conn
                .query_first("SELECT COUNT(*) FROM state_log")?
                .unwrap_or(0),
        };
        Ok(n)
    }

    fn prune(&self, older_than: DateTime<Utc>) -> Result<u64, RecorderError> {
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;
        let ts = fmt_ts(older_than);
        let result = conn.exec_drop(
            "DELETE FROM state_log WHERE timestamp < ?",
            (&ts,),
        );
        result?;
        Ok(conn.affected_rows())
    }

    // ── Curator: aggregates ────────────────────────────────────────

    fn time_range(&self) -> Result<(Option<DateTime<Utc>>, Option<DateTime<Utc>>), RecorderError> {
        let mut conn = self.conn()?;
        let row: Option<(Option<String>, Option<String>)> = conn.query_first(
            "SELECT CAST(MIN(timestamp) AS CHAR), CAST(MAX(timestamp) AS CHAR) FROM state_log",
        )?;
        let (oldest, newest) = row.unwrap_or((None, None));
        Ok((oldest.and_then(|s| parse_mysql_ts(&s)), newest.and_then(|s| parse_mysql_ts(&s))))
    }

    fn domain_stats(&self) -> Result<Vec<DomainStat>, RecorderError> {
        let mut conn = self.conn()?;
        let total: u64 = conn
            .query_first("SELECT COUNT(*) FROM state_log")?
            .unwrap_or(0);
        let rows: Vec<(String, u64, u64)> = conn.query(
            "SELECT SUBSTRING_INDEX(entity_id, '.', 1) AS domain,
                    COUNT(*) AS row_count,
                    COUNT(DISTINCT entity_id) AS entity_count
             FROM state_log
             GROUP BY domain
             ORDER BY row_count DESC",
        )?;
        Ok(rows
            .into_iter()
            .map(|(domain, row_count, entity_count)| {
                DomainStat {
                    domain,
                    row_count,
                    entity_count,
                    avg_per_entity: if entity_count > 0 {
                        row_count as f64 / entity_count as f64
                    } else {
                        0.0
                    },
                    pct_of_total: if total > 0 {
                        row_count as f64 / total as f64 * 100.0
                    } else {
                        0.0
                    },
                }
            })
            .collect())
    }

    fn top_entities(
        &self,
        n: usize,
        domain: Option<&str>,
    ) -> Result<Vec<EntitySummary>, RecorderError> {
        let mut conn = self.conn()?;
        let prefix = domain.map(|d| format!("{d}.%"));
        let (sql, p) = mysql_entity_summary_sql(&prefix, "row_count DESC", n);
        let rows: Vec<(String, u64, Option<f64>, Option<String>, Option<String>)> = if let Some(pf) = p {
            conn.exec(&sql, (pf,))?
        } else {
            conn.query(&sql)?
        };
        Ok(rows_to_summaries(rows))
    }

    fn fastest_entities(
        &self,
        n: usize,
        domain: Option<&str>,
    ) -> Result<Vec<EntitySummary>, RecorderError> {
        let mut conn = self.conn()?;
        let prefix = domain.map(|d| format!("{d}.%"));
        let (sql, p) = mysql_entity_summary_sql(&prefix, "avg_interval_secs ASC", n);
        let rows: Vec<(String, u64, Option<f64>, Option<String>, Option<String>)> = if let Some(pf) = p {
            conn.exec(&sql, (pf,))?
        } else {
            conn.query(&sql)?
        };
        Ok(rows_to_summaries(rows))
    }

    fn entity_profile(&self, entity_id: &str) -> Result<EntityProfile, RecorderError> {
        let mut conn = self.conn()?;

        let row_count: u64 = conn
            .exec_first("SELECT COUNT(*) FROM state_log WHERE entity_id = ?", (entity_id,))?
            .unwrap_or(0);

        let distinct_state_count: u64 = conn
            .exec_first(
                "SELECT COUNT(DISTINCT state) FROM state_log WHERE entity_id = ?",
                (entity_id,),
            )?
            .unwrap_or(0);

        let time_span: Option<(Option<String>, Option<String>)> = conn.exec_first(
            "SELECT CAST(MIN(timestamp) AS CHAR), CAST(MAX(timestamp) AS CHAR) FROM state_log WHERE entity_id = ?",
            (entity_id,),
        )?;

        let avg_interval_secs = match &time_span {
            Some((Some(min_s), Some(max_s))) if row_count > 1 => {
                match (parse_mysql_ts(min_s), parse_mysql_ts(max_s)) {
                    (Some(a), Some(b)) => {
                        Some((b - a).num_milliseconds() as f64 / 1000.0 / (row_count - 1) as f64)
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        let hist_rows: Vec<(String, u64)> = conn.exec(
            "SELECT state, COUNT(*) AS cnt FROM state_log
             WHERE entity_id = ?
             GROUP BY state ORDER BY cnt DESC LIMIT 10",
            (entity_id,),
        )?;
        let state_histogram: Vec<StateCount> = hist_rows
            .into_iter()
            .map(|(state, count)| StateCount { state, count })
            .collect();

        let recent_rows: Vec<(String, String)> = conn.exec(
            "SELECT state, CAST(timestamp AS CHAR) FROM state_log
             WHERE entity_id = ?
             ORDER BY timestamp DESC LIMIT 10",
            (entity_id,),
        )?;
        let recent_changes: Vec<RecentChange> = recent_rows
            .into_iter()
            .map(|(state, ts)| RecentChange {
                state,
                timestamp: parse_mysql_ts(&ts).unwrap_or_else(Utc::now),
            })
            .collect();

        let flagged_count: u64 = conn
            .exec_first(
                "SELECT COUNT(*) FROM state_log
                 WHERE entity_id = ? AND deletion_reason IS NOT NULL",
                (entity_id,),
            )?
            .unwrap_or(0);

        Ok(EntityProfile {
            entity_id: entity_id.to_string(),
            row_count,
            avg_interval_secs,
            distinct_state_count,
            state_histogram,
            recent_changes,
            flagged_count,
            pct_flagged: if row_count > 0 {
                flagged_count as f64 / row_count as f64 * 100.0
            } else {
                0.0
            },
        })
    }

    fn age_distribution(&self) -> Result<Vec<AgeBucket>, RecorderError> {
        let mut conn = self.conn()?;
        let now = Utc::now();
        let boundaries = age_boundaries_mysql(now);
        let mut result = Vec::new();
        for (label, from, to) in &boundaries {
            let cnt: u64 = conn
                .exec_first(
                    "SELECT COUNT(*) FROM state_log WHERE timestamp >= ? AND timestamp < ?",
                    (&fmt_ts(*from), &fmt_ts(*to)),
                )?
                .unwrap_or(0);
            result.push(AgeBucket {
                label: label.to_string(),
                row_count: cnt,
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
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;
        let now = fmt_ts(Utc::now());
        let reason_val = reason.as_u8() as u32;

        let cutoff_clause = cutoff
            .map(|c| format!("AND timestamp < '{}'", fmt_ts(c)))
            .unwrap_or_default();

        let rows_flagged = if let Some(n) = keep_every_n {
            let sql = format!(
                "UPDATE state_log SET deletion_reason = ?, flagged_at = ?
                 WHERE id IN (
                     SELECT id FROM (
                         SELECT id, ROW_NUMBER() OVER (
                             PARTITION BY entity_id ORDER BY timestamp
                         ) AS rn
                         FROM state_log
                         WHERE entity_id = ?
                           AND deletion_reason IS NULL
                           {cutoff_clause}
                     ) AS t WHERE (rn - 1) % ? != 0
                 )"
            );
            conn.exec_drop(&sql, (reason_val, &now, entity_id, n as u64))?;
            conn.affected_rows()
        } else {
            let sql = format!(
                "UPDATE state_log SET deletion_reason = ?, flagged_at = ?
                 WHERE entity_id = ? AND deletion_reason IS NULL {cutoff_clause}"
            );
            conn.exec_drop(&sql, (reason_val, &now, entity_id))?;
            conn.affected_rows()
        };

        Ok(FlagResult { rows_flagged })
    }

    fn flag_domain(
        &self,
        domain: &str,
        reason: DeletionReason,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<DomainFlagResult, RecorderError> {
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;
        let now = fmt_ts(Utc::now());
        let reason_val = reason.as_u8() as u32;
        let prefix = format!("{domain}.%");

        let cutoff_clause = cutoff
            .map(|c| format!("AND timestamp < '{}'", fmt_ts(c)))
            .unwrap_or_default();

        let entities_affected: u64 = conn
            .exec_first(
                &format!(
                    "SELECT COUNT(DISTINCT entity_id) FROM state_log
                     WHERE entity_id LIKE ? AND deletion_reason IS NULL {cutoff_clause}"
                ),
                (&prefix,),
            )?
            .unwrap_or(0);

        let sql = format!(
            "UPDATE state_log SET deletion_reason = ?, flagged_at = ?
             WHERE entity_id LIKE ? AND deletion_reason IS NULL {cutoff_clause}"
        );
        conn.exec_drop(&sql, (reason_val, &now, &prefix))?;
        let rows_flagged = conn.affected_rows();

        Ok(DomainFlagResult {
            rows_flagged,
            entities_affected,
        })
    }

    fn unflag(
        &self,
        entity_id: Option<&str>,
        reason: Option<DeletionReason>,
    ) -> Result<UnflagResult, RecorderError> {
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;
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
        conn.query_drop(&sql)?;
        Ok(UnflagResult {
            rows_unflagged: conn.affected_rows(),
        })
    }

    fn flagged_counts_by_reason(&self) -> Result<Vec<(DeletionReason, u64)>, RecorderError> {
        let mut conn = self.conn()?;
        let rows: Vec<(u32, u64)> = conn.query(
            "SELECT deletion_reason, COUNT(*) FROM state_log
             WHERE deletion_reason IS NOT NULL
             GROUP BY deletion_reason ORDER BY deletion_reason",
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(r, c)| DeletionReason::from_u8(r as u8).map(|reason| (reason, c)))
            .collect())
    }

    fn flagged_counts_by_domain(&self) -> Result<Vec<DomainFlagCount>, RecorderError> {
        let mut conn = self.conn()?;
        let rows: Vec<(String, u64)> = conn.query(
            "SELECT SUBSTRING_INDEX(entity_id, '.', 1) AS domain,
                    COUNT(*) AS cnt
             FROM state_log
             WHERE deletion_reason IS NOT NULL
             GROUP BY domain ORDER BY cnt DESC",
        )?;
        Ok(rows
            .into_iter()
            .map(|(domain, count)| DomainFlagCount { domain, count })
            .collect())
    }

    fn flag_preview(
        &self,
        entity_id: &str,
        cutoff: Option<DateTime<Utc>>,
        keep_every_n: Option<u64>,
    ) -> Result<FlagPreview, RecorderError> {
        let mut conn = self.conn()?;
        let cutoff_clause = cutoff
            .map(|c| format!("AND timestamp < '{}'", fmt_ts(c)))
            .unwrap_or_default();

        let total_rows: u64 = conn
            .exec_first(
                &format!(
                    "SELECT COUNT(*) FROM state_log
                     WHERE entity_id = ? AND deletion_reason IS NULL {cutoff_clause}"
                ),
                (entity_id,),
            )?
            .unwrap_or(0);

        let n = keep_every_n.unwrap_or(1).max(1);
        let would_keep = (total_rows + n - 1) / n;
        let would_flag = total_rows.saturating_sub(would_keep);
        let pct_reduction = if total_rows > 0 {
            would_flag as f64 / total_rows as f64 * 100.0
        } else {
            0.0
        };

        // Sample 5 kept timestamps
        let sample_kept_timestamps = if n > 1 {
            let rows: Vec<(String,)> = conn.exec(
                &format!(
                    "SELECT CAST(timestamp AS CHAR) FROM (
                         SELECT timestamp, ROW_NUMBER() OVER (ORDER BY timestamp) AS rn
                         FROM state_log
                         WHERE entity_id = ? AND deletion_reason IS NULL {cutoff_clause}
                     ) AS t WHERE (rn - 1) % ? = 0"
                ),
                (entity_id, n as u64),
            )?;
            let all_kept: Vec<DateTime<Utc>> = rows
                .into_iter()
                .filter_map(|(ts,)| parse_mysql_ts(&ts))
                .collect();
            sample_n(&all_kept, 5)
        } else {
            Vec::new()
        };

        Ok(FlagPreview {
            total_rows,
            would_flag,
            would_keep,
            pct_reduction,
            sample_kept_timestamps,
        })
    }

    fn prune_flagged(&self, grace_cutoff: DateTime<Utc>) -> Result<u64, RecorderError> {
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;
        conn.exec_drop(
            "DELETE FROM state_log
             WHERE deletion_reason IS NOT NULL AND flagged_at < ?",
            (fmt_ts(grace_cutoff),),
        )?;
        Ok(conn.affected_rows())
    }

    fn rotate_for_curation(
        &self,
        curating_qualified: &str,
        archive_qualified: &str,
    ) -> Result<(), RecorderError> {
        // Hold write_lock across the swap so concurrent record() calls
        // can't slip in between the CREATE and the RENAME.  The lock is
        // released as soon as the metadata ops complete (~milliseconds).
        let _lock = self.write_lock.lock().unwrap();
        let mut conn = self.conn()?;

        // Pre-create the empty replacement for the live table (same
        // schema as the current state_log).
        conn.query_drop("CREATE TABLE state_log_new LIKE state_log")?;

        // Does a previous snapshot already occupy the curating slot?
        let (curating_db, curating_tbl) = split_qualified(curating_qualified)?;
        let prior: Option<u64> = conn.exec_first(
            "SELECT 1 FROM information_schema.tables
             WHERE table_schema = ? AND table_name = ? LIMIT 1",
            (curating_db, curating_tbl),
        )?;

        // One atomic statement, with up to three renames:
        //   1. (optional) move stale curating → archive
        //   2. live state_log → curating
        //   3. fresh state_log_new → state_log
        let sql = if prior.is_some() {
            format!(
                "RENAME TABLE \
                 {curating} TO {archive}, \
                 state_log TO {curating}, \
                 state_log_new TO state_log",
                curating = curating_qualified,
                archive = archive_qualified,
            )
        } else {
            format!(
                "RENAME TABLE \
                 state_log TO {curating}, \
                 state_log_new TO state_log",
                curating = curating_qualified,
            )
        };
        if let Err(e) = conn.query_drop(&sql) {
            // Leave the freshly-created replacement around to debug; user
            // can DROP TABLE state_log_new manually if needed.
            return Err(e.into());
        }
        Ok(())
    }
}

/// Split `db.table` into `(db, table)`.
fn split_qualified(q: &str) -> Result<(&str, &str), RecorderError> {
    q.split_once('.')
        .ok_or_else(|| RecorderError::Other(format!("expected db.table, got `{q}`")))
}

fn row_to_record(row: (String, String, Option<String>, String)) -> Record {
    let (entity_id, state, attrs_str, ts) = row;
    let attributes = attrs_str.and_then(|s| serde_json::from_str(&s).ok());
    let timestamp = parse_mysql_ts(&ts).unwrap_or_else(|| Utc::now());
    Record {
        entity_id,
        state,
        attributes,
        timestamp,
    }
}

fn parse_mysql_ts(s: &str) -> Option<DateTime<Utc>> {
    // MySQL DATETIME format: "2025-01-15 10:30:00" or with millis
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
        .map(|ndt| ndt.and_utc())
}

fn fmt_ts(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

fn mysql_entity_summary_sql(
    prefix: &Option<String>,
    order: &str,
    limit: usize,
) -> (String, Option<String>) {
    let where_clause = if prefix.is_some() {
        "WHERE entity_id LIKE ?"
    } else {
        ""
    };
    // Omit COUNT(DISTINCT state) — state is TEXT (unbounded length) so
    // MySQL can't use a covering index, causing full table scans.
    // distinct_states is left as None in the returned summaries.
    let sql = format!(
        "SELECT entity_id,
                COUNT(*) AS row_count,
                TIMESTAMPDIFF(MICROSECOND, MIN(timestamp), MAX(timestamp))
                    / 1000000.0 / NULLIF(COUNT(*) - 1, 0) AS avg_interval_secs,
                CAST(MIN(timestamp) AS CHAR) AS first_seen,
                CAST(MAX(timestamp) AS CHAR) AS last_seen
         FROM state_log
         {where_clause}
         GROUP BY entity_id
         HAVING COUNT(*) >= 2
         ORDER BY {order}
         LIMIT {limit}"
    );
    (sql, prefix.clone())
}

fn rows_to_summaries(
    rows: Vec<(String, u64, Option<f64>, Option<String>, Option<String>)>,
) -> Vec<EntitySummary> {
    rows.into_iter()
        .map(|(entity_id, row_count, avg_interval_secs, first_seen, last_seen)| EntitySummary {
            entity_id,
            row_count,
            avg_interval_secs,
            distinct_states: None,
            first_seen: first_seen.and_then(|s| parse_mysql_ts(&s)),
            last_seen: last_seen.and_then(|s| parse_mysql_ts(&s)),
        })
        .collect()
}

fn age_boundaries_mysql(now: DateTime<Utc>) -> Vec<(&'static str, DateTime<Utc>, DateTime<Utc>)> {
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
