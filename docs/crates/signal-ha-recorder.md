# signal-ha-recorder

> Pluggable entity state recorder — trait-based storage with SQLite and MySQL backends.

## Overview

`signal-ha-recorder` records Home Assistant entity state changes to a
database for later analysis. It provides:

- **`RecordStore` trait** — pluggable storage backend with two implementations
- **`SqliteStore`** — lightweight, file-based (WAL mode, dedup index)
- **`MysqlStore`** — production InnoDB with JSON attributes and connection pooling
- **`EntityFilter`** — glob-pattern allowlist (`sensor.*`, `light.porch_*`)
- **`Recorder`** — subscribes to matching HA entities and records state changes
- **`RecorderStats`** — live atomic counters for observability dashboards

## Feature flags

| Feature | Default | Enables |
|:--------|:--------|:--------|
| `sqlite` | ✅ | `SqliteStore` (bundled `rusqlite`) |
| `mysql` | | `MysqlStore` (`mysql` crate) |

## Quick start

```rust
use signal_ha_recorder::{Recorder, EntityFilter, SqliteStore};
use signal_ha::HaClient;

let client = HaClient::connect("ws://ha:8123/api/websocket", token).await?;
let store = SqliteStore::open("/tmp/recorder.db")?;
let filter = EntityFilter::new(vec!["sensor.*".into(), "light.*".into()]);

let recorder = Recorder::new(Box::new(store), filter);
let stats = recorder.stats();

// Stats handle is cheap to clone — wire into your status page
println!("Written: {}", stats.records_written());

// Blocks forever, recording state changes
recorder.run(client).await?;
```

## API Reference

### RecordStore trait

All storage backends implement this trait. Methods are synchronous (wrap in
`spawn_blocking` for async contexts if needed).

| Method | Signature | Purpose |
|:-------|:----------|:--------|
| `record` | `(&self, entity_id, state, attributes, timestamp)` | Insert a state record (ignores exact duplicates) |
| `query` | `(&self, entity_id, from, to) → Vec<Record>` | Query records in a time range (ASC) |
| `latest` | `(&self, entity_id) → Option<Record>` | Most recent record for an entity |
| `entities` | `(&self) → Vec<String>` | All distinct entity IDs in the store |
| `count` | `(&self, entity_id?) → u64` | Count records (all or per-entity) |
| `prune` | `(&self, older_than) → u64` | Delete old records, return rows deleted |

### Record

```rust
pub struct Record {
    pub entity_id: String,
    pub state: String,
    pub attributes: Option<serde_json::Value>,
    pub timestamp: DateTime<Utc>,
}
```

### SqliteStore

WAL-mode SQLite with a unique index on `(entity_id, timestamp, state)` for
automatic deduplication.

```rust
// File-based
let store = SqliteStore::open("/path/to/recorder.db")?;

// In-memory (for testing)
let store = SqliteStore::open_in_memory()?;
```

### MysqlStore

InnoDB-backed store with JSON attributes column, `DATETIME(3)` timestamps,
and connection pooling. Designed for the ZFS-tuned MySQL 8.4 instance on
southside.

```rust
// Unix socket (auth_socket — no password needed)
let store = MysqlStore::new("mysql://signal@localhost/signal_recorder?socket=/run/mysqld/mysqld.sock")?;

// TCP
let store = MysqlStore::new("mysql://user:pass@127.0.0.1:3306/signal_recorder")?;
```

Schema (auto-created):

```sql
CREATE TABLE IF NOT EXISTS entity_states (
    id         BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    entity_id  VARCHAR(255) NOT NULL,
    state      VARCHAR(255) NOT NULL,
    attributes JSON,
    timestamp  DATETIME(3)  NOT NULL,
    UNIQUE KEY dedup (entity_id, timestamp, state(191)),
    INDEX      idx_entity_time (entity_id, timestamp)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
```

### EntityFilter

Glob-pattern allowlist. Supports `*` wildcards that match any sequence of
characters.

```rust
let filter = EntityFilter::new(vec![
    "sensor.*_temperature".into(),
    "binary_sensor.*_motion".into(),
    "light.*".into(),
]);

assert!(filter.matches("sensor.office_temperature"));
assert!(filter.matches("light.porch"));
assert!(!filter.matches("switch.garage"));

// Match everything
let all = EntityFilter::allow_all();
```

### Recorder

Connects to HA, fetches all entities, subscribes to each that matches the
filter, and records state changes continuously.

```rust
let recorder = Recorder::new(Box::new(store), filter);
let stats = recorder.stats(); // cheap clone for dashboards

recorder.run(client).await?; // blocks forever
```

### RecorderStats

Atomic counters — clone-cheap, safe to share across threads.

| Method | Returns |
|:-------|:--------|
| `records_written()` | Total records written to the store |
| `records_skipped()` | Events skipped (lagged receiver) |
| `errors()` | Write errors encountered |
| `entities_seen()` | Distinct entities observed |

### backfill_current_states

Helper to snapshot the current state of entities before any changes occur.
Call on startup to avoid missing the initial state.

```rust
use signal_ha_recorder::backfill_current_states;

let count = backfill_current_states(&client, store.as_ref(), &filter, &entity_ids).await?;
println!("Backfilled {count} entities");
```

## Dependencies

| Crate | Version | Feature | Purpose |
|:------|:--------|:--------|:--------|
| `rusqlite` | 0.32 | `sqlite` | SQLite with bundled amalgamation |
| `mysql` | 25 | `mysql` | MySQL client with connection pooling |
| `signal-ha` | workspace | always | HaClient, EntityState, StateChange |
| `chrono` | 0.4 | always | DateTime timestamps |
| `serde_json` | 1 | always | JSON attributes |
| `tracing` | 0.1 | always | Structured logging |
| `thiserror` | 2 | always | Error types |
