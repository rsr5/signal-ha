use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::{debug, error, info, warn};

use signal_ha::HaClient;

use crate::filter::EntityFilter;
use crate::store_trait::RecordStore;
use crate::RecorderError;

/// Live recording statistics — clone-cheap, updated atomically.
///
/// Use this to build a status page or dashboard showing recorder health.
#[derive(Clone)]
pub struct RecorderStats {
    inner: Arc<StatsInner>,
}

struct StatsInner {
    records_written: AtomicU64,
    records_skipped: AtomicU64,
    errors: AtomicU64,
    entities_seen: AtomicU64,
}

impl RecorderStats {
    fn new() -> Self {
        Self {
            inner: Arc::new(StatsInner {
                records_written: AtomicU64::new(0),
                records_skipped: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                entities_seen: AtomicU64::new(0),
            }),
        }
    }

    /// Total records successfully written to the store.
    pub fn records_written(&self) -> u64 {
        self.inner.records_written.load(Ordering::Relaxed)
    }

    /// State changes skipped because they didn't match the filter.
    pub fn records_skipped(&self) -> u64 {
        self.inner.records_skipped.load(Ordering::Relaxed)
    }

    /// Number of write errors encountered.
    pub fn errors(&self) -> u64 {
        self.inner.errors.load(Ordering::Relaxed)
    }

    /// Number of distinct entities seen so far.
    pub fn entities_seen(&self) -> u64 {
        self.inner.entities_seen.load(Ordering::Relaxed)
    }

    fn inc_written(&self) {
        self.inner.records_written.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_errors(&self) {
        self.inner.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn set_entities_seen(&self, n: u64) {
        self.inner.entities_seen.store(n, Ordering::Relaxed);
    }
}

/// Entity state recorder — subscribes to HA state changes and writes
/// matching entities to a [`RecordStore`] backend.
///
/// # Usage
///
/// ```rust,no_run
/// use signal_ha_recorder::{Recorder, EntityFilter, SqliteStore};
/// use signal_ha::HaClient;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = HaClient::connect("ws://ha:8123/api/websocket", "token").await?;
/// let store = SqliteStore::open("/tmp/recorder.db")?;
/// let filter = EntityFilter::new(vec!["sensor.*".into()]);
/// let recorder = Recorder::new(Box::new(store), filter);
///
/// // Get stats handle before running (for dashboard/status page)
/// let stats = recorder.stats();
/// println!("Written: {}", stats.records_written());
///
/// // Run blocks forever, recording state changes
/// recorder.run(client).await?;
/// # Ok(())
/// # }
/// ```
pub struct Recorder {
    store: Box<dyn RecordStore>,
    filter: EntityFilter,
    stats: RecorderStats,
}

impl Recorder {
    /// Create a new recorder with a storage backend and entity filter.
    pub fn new(store: Box<dyn RecordStore>, filter: EntityFilter) -> Self {
        Self {
            store,
            filter,
            stats: RecorderStats::new(),
        }
    }

    /// Get a cheap clone of the stats handle for observability.
    pub fn stats(&self) -> RecorderStats {
        self.stats.clone()
    }

    /// Subscribe to matching entity state changes and record them.
    ///
    /// This method runs indefinitely. It fetches all HA entities,
    /// filters them through the allowlist, subscribes to each, and
    /// records state changes to the store.
    ///
    /// Returns on HA disconnection or fatal error.
    pub async fn run(self, client: HaClient) -> Result<(), RecorderError> {
        info!("Recorder starting");

        // Fetch all current entities to find which ones match our filter
        let all_states = client
            .send_raw(serde_json::json!({"type": "get_states"}))
            .await?;

        let entities: Vec<String> = all_states["result"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s["entity_id"].as_str().map(String::from))
                    .filter(|eid| self.filter.matches(eid))
                    .collect()
            })
            .unwrap_or_default();

        info!(count = entities.len(), "Matched entities for recording");

        if entities.is_empty() {
            warn!("No entities match the filter — nothing to record");
            // Wait forever (the caller can cancel)
            std::future::pending::<()>().await;
            return Ok(());
        }

        // Subscribe to each matching entity
        let mut receivers = Vec::new();
        for eid in &entities {
            match client.subscribe_state(eid).await {
                Ok(rx) => receivers.push((eid.clone(), rx)),
                Err(e) => {
                    warn!(entity = %eid, error = %e, "Failed to subscribe — skipping");
                }
            }
        }

        info!(
            subscribed = receivers.len(),
            "Recording state changes"
        );

        let mut entities_seen = std::collections::HashSet::new();

        // Multiplex all receivers into a single recording loop
        loop {
            for (eid, rx) in &mut receivers {
                match rx.try_recv() {
                    Ok(change) => {
                        self.handle_change(&change, &mut entities_seen);
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        warn!(entity = %eid, skipped = n, "Recorder lagged");
                        self.stats
                            .inner
                            .records_skipped
                            .fetch_add(n, Ordering::Relaxed);
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        info!(entity = %eid, "Subscription closed");
                    }
                }
            }
            // Yield to avoid busy-spinning — check every 50ms
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn handle_change(
        &self,
        change: &signal_ha::StateChange,
        entities_seen: &mut std::collections::HashSet<String>,
    ) {
        let Some(ref new_state) = change.new else {
            return;
        };

        // Track unique entities
        if entities_seen.insert(change.entity_id.clone()) {
            self.stats.set_entities_seen(entities_seen.len() as u64);
            debug!(entity = %change.entity_id, "New entity seen by recorder");
        }

        let timestamp = new_state.last_changed;
        let attrs = if new_state.attributes.is_null() {
            None
        } else {
            Some(&new_state.attributes)
        };

        if let Err(e) = self.store.record(
            &change.entity_id,
            &new_state.state,
            attrs,
            timestamp,
        ) {
            error!(entity = %change.entity_id, error = %e, "Failed to record state change");
            self.stats.inc_errors();
        } else {
            self.stats.inc_written();
        }
    }
}

/// Helper: record the current state of specific entities (backfill on startup).
///
/// Useful for capturing the initial state of watched entities before
/// any changes occur.
pub async fn backfill_current_states(
    client: &HaClient,
    store: &dyn RecordStore,
    filter: &EntityFilter,
    entity_ids: &[String],
) -> Result<u64, RecorderError> {
    let mut count = 0u64;
    for eid in entity_ids {
        if !filter.matches(eid) {
            continue;
        }
        match client.get_state(eid).await {
            Ok(state) => {
                let attrs = if state.attributes.is_null() {
                    None
                } else {
                    Some(&state.attributes)
                };
                store.record(eid, &state.state, attrs, state.last_changed)?;
                count += 1;
            }
            Err(e) => {
                warn!(entity = %eid, error = %e, "Failed to backfill state");
            }
        }
    }
    info!(count, "Backfilled current states");
    Ok(count)
}
