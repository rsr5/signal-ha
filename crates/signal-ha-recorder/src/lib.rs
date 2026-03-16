//! # signal-ha-recorder
//!
//! Pluggable entity state recorder for Home Assistant.
//!
//! Provides a [`RecordStore`] trait with two backends:
//! - [`SqliteStore`] — lightweight, file-based (feature `sqlite`)
//! - [`MysqlStore`] — production MySQL/InnoDB (feature `mysql`)
//!
//! The [`EntityFilter`] allowlist supports exact matches and wildcard
//! patterns (`sensor.*`, `light.porch_*`).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use signal_ha_recorder::{Recorder, EntityFilter, SqliteStore};
//! use signal_ha::HaClient;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = HaClient::connect("ws://ha:8123/api/websocket", "token").await?;
//! let store = SqliteStore::open("/tmp/recorder.db")?;
//! let filter = EntityFilter::new(vec!["sensor.*".into(), "light.*".into()]);
//! let recorder = Recorder::new(Box::new(store), filter);
//! let stats = recorder.stats();
//! recorder.run(client).await?;
//! # Ok(())
//! # }
//! ```

mod error;
mod filter;
mod recorder;
mod store_trait;
mod deletion_reason;
pub mod curator;

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "mysql")]
mod mysql_store;

pub use deletion_reason::DeletionReason;
pub use error::RecorderError;
pub use filter::EntityFilter;
pub use recorder::{backfill_current_states, Recorder, RecorderStats};
pub use store_trait::{
    AgeBucket, DomainFlagCount, DomainFlagResult, DomainStat, EntityProfile, EntitySummary,
    FlagPreview, FlagResult, Record, RecordStore, RecentChange, StateCount, UnflagResult,
};

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;
#[cfg(feature = "mysql")]
pub use mysql_store::MysqlStore;
