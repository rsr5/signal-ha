use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::error::RecorderError;
use crate::store_trait::{Record, RecordStore};

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
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                entity_id  TEXT NOT NULL,
                state      TEXT NOT NULL,
                attributes TEXT,
                timestamp  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_state_log_entity_ts
                ON state_log(entity_id, timestamp);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_state_log_dedup
                ON state_log(entity_id, timestamp, state);",
        )?;
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
}
