use std::sync::Mutex;

use chrono::{DateTime, Utc};
use mysql::prelude::*;
use mysql::{Opts, Pool, PooledConn};

use crate::error::RecorderError;
use crate::store_trait::{Record, RecordStore};

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
                id         BIGINT AUTO_INCREMENT PRIMARY KEY,
                entity_id  VARCHAR(255) NOT NULL,
                state      TEXT NOT NULL,
                attributes JSON,
                timestamp  DATETIME(3) NOT NULL,
                UNIQUE KEY idx_dedup (entity_id, timestamp, state(191)),
                INDEX idx_entity_ts (entity_id, timestamp)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
        )?;
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
        let ts = older_than.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let result = conn.exec_drop(
            "DELETE FROM state_log WHERE timestamp < ?",
            (&ts,),
        );
        result?;
        // affected_rows from the last query
        Ok(conn.affected_rows())
    }
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
