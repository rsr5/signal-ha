//! Reconciliation logic for entity availability and drift.
//!
//! Ensures the intended lighting state is reapplied after:
//! 1. An entity becomes available (was unavailable/unknown)
//! 2. A power-on event for a controlled device
//!
//! Direct port of `appdaemon_lighting.reconcile`.
//!
//! # Design
//!
//! The reconciler uses trait-based scheduling (`ReconcileScheduler`) so
//! that tests can use a fake scheduler with synchronous timer firing,
//! exactly matching the Python test pattern.

use std::collections::{HashMap, HashSet};

// ─────────────────────────────────────────────────────────────────────
// Traits
// ─────────────────────────────────────────────────────────────────────

/// Opaque handle to a scheduled timer. Cloneable, comparable.
pub type TimerHandle = u64;

/// Scheduler interface — abstraction over AppDaemon's `run_in` / `cancel_timer`.
pub trait ReconcileScheduler {
    /// Schedule `callback` to fire after `seconds`. Returns a handle.
    fn schedule_callback(&mut self, seconds: u32, entity: String, reason: String) -> TimerHandle;
    /// Cancel a previously scheduled timer.
    fn cancel_timer(&mut self, handle: TimerHandle);
}

/// Minimal HA interface for reconciliation.
pub trait ReconcileHAService {
    fn get_state(&self, entity_id: &str) -> Option<String>;
    fn set_state(&mut self, entity_id: &str, state: &str);
}

// ─────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────

/// Check if a state indicates the entity is unavailable.
pub fn is_unavailable(state: Option<&str>) -> bool {
    match state {
        None => true,
        Some(s) => {
            let lower = s.to_lowercase();
            matches!(lower.as_str(), "unavailable" | "unknown" | "")
        }
    }
}

/// Configuration for the reconciler.
#[derive(Debug, Clone)]
pub struct ReconcileConfig {
    pub enabled: bool,
    pub settle_seconds: u32,
    pub max_retries: u32,
    pub reason_entity: Option<String>,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settle_seconds: 5,
            max_retries: 3,
            reason_entity: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Reconciler
// ─────────────────────────────────────────────────────────────────────

/// Watches controlled entities and triggers reconciliation when they
/// become available after being unavailable/unknown.
pub struct Reconciler {
    config: ReconcileConfig,
    controlled_entities: HashSet<String>,
    per_entity_handles: HashMap<String, TimerHandle>,
    all_handle: Option<TimerHandle>,
    retries: HashMap<String, u32>,
    log_fn: Option<Box<dyn Fn(&str)>>,
}

impl Reconciler {
    pub fn new(config: ReconcileConfig, controlled_entities: HashSet<String>) -> Self {
        Self {
            config,
            controlled_entities,
            per_entity_handles: HashMap::new(),
            all_handle: None,
            retries: HashMap::new(),
            log_fn: None,
        }
    }

    /// Set an optional logging function.
    pub fn with_log(mut self, log: impl Fn(&str) + 'static) -> Self {
        self.log_fn = Some(Box::new(log));
        self
    }

    fn log(&self, msg: &str) {
        if let Some(ref f) = self.log_fn {
            f(msg);
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Register watchers. Returns the set of entities being watched.
    ///
    /// The caller is responsible for hooking up state-change listeners
    /// that call [`on_entity_change`] when states change.
    pub fn register_watchers(&self) -> Vec<String> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut entities: Vec<_> = self.controlled_entities.iter().cloned().collect();
        entities.sort();
        let count = entities.len();
        self.log(&format!(
            "[reconcile] registered watchers for {count} entities"
        ));
        entities
    }

    /// Called when a controlled entity's state changes.
    /// Returns `true` if reconciliation was scheduled.
    pub fn on_entity_change(
        &mut self,
        entity: &str,
        old: Option<&str>,
        new: Option<&str>,
        scheduler: &mut impl ReconcileScheduler,
        ha: &mut impl ReconcileHAService,
    ) -> bool {
        if !self.config.enabled {
            return false;
        }

        let old_unavail = is_unavailable(old);
        let new_unavail = is_unavailable(new);

        if old_unavail && !new_unavail {
            let reason = format!(
                "became_available:{}->{}",
                old.unwrap_or(""),
                new.unwrap_or("")
            );
            self.schedule(entity, &reason, scheduler, ha);
            return true;
        }
        false
    }

    /// Schedule reconciliation for a specific entity.
    pub fn schedule(
        &mut self,
        entity: &str,
        reason: &str,
        scheduler: &mut impl ReconcileScheduler,
        ha: &mut impl ReconcileHAService,
    ) {
        if !self.config.enabled {
            return;
        }

        let retries = *self.retries.get(entity).unwrap_or(&0);
        if retries >= self.config.max_retries {
            self.log(&format!(
                "[reconcile] max retries reached for {entity}, giving up"
            ));
            return;
        }

        // Cancel any existing timer for this entity
        self.cancel_entity_timer(entity, scheduler);

        let handle = scheduler.schedule_callback(
            self.config.settle_seconds,
            entity.to_string(),
            reason.to_string(),
        );
        self.per_entity_handles.insert(entity.to_string(), handle);

        self.publish_reason(
            &format!(
                "{reason}; reconcile in {}s: {entity}",
                self.config.settle_seconds
            ),
            ha,
        );
    }

    /// Called when a per-entity timer fires.
    ///
    /// Returns `true` if reconciliation should proceed (entity is available),
    /// `false` if a retry was scheduled (still unavailable).
    pub fn on_timer_fired(
        &mut self,
        entity: &str,
        reason: &str,
        scheduler: &mut impl ReconcileScheduler,
        ha: &mut impl ReconcileHAService,
    ) -> bool {
        self.per_entity_handles.remove(entity);

        let state = ha.get_state(entity);
        if is_unavailable(state.as_deref()) {
            // Still unavailable — retry
            let count = self.retries.entry(entity.to_string()).or_insert(0);
            *count += 1;
            let retry_reason = format!("{reason} (still unavailable, retry {count})");
            self.schedule(entity, &retry_reason, scheduler, ha);
            return false;
        }

        // Available — reset retries and let caller reconcile
        self.retries.insert(entity.to_string(), 0);
        self.publish_reason(&format!("reconciled:{entity} ({reason})"), ha);
        true
    }

    /// Schedule reconciliation of all controlled entities.
    pub fn schedule_reconcile_all(
        &mut self,
        delay_seconds: u32,
        scheduler: &mut impl ReconcileScheduler,
        ha: &mut impl ReconcileHAService,
    ) {
        if !self.config.enabled || delay_seconds == 0 {
            return;
        }

        // Cancel previous all-handle
        if let Some(handle) = self.all_handle.take() {
            scheduler.cancel_timer(handle);
        }

        let handle = scheduler.schedule_callback(delay_seconds, String::new(), "reconcile_all".to_string());
        self.all_handle = Some(handle);

        self.publish_reason(
            &format!("reconcile_all scheduled in {delay_seconds}s"),
            ha,
        );
    }

    /// Called when the reconcile-all timer fires.
    pub fn on_reconcile_all_fired(&mut self) {
        self.all_handle = None;
    }

    /// Reset all pending timers and internal state.
    pub fn reset(&mut self, scheduler: &mut impl ReconcileScheduler) {
        for (_, handle) in self.per_entity_handles.drain() {
            scheduler.cancel_timer(handle);
        }
        if let Some(handle) = self.all_handle.take() {
            scheduler.cancel_timer(handle);
        }
        self.retries.clear();
    }

    // ── Private ────────────────────────────────────────────────────

    fn cancel_entity_timer(
        &mut self,
        entity: &str,
        scheduler: &mut impl ReconcileScheduler,
    ) {
        if let Some(handle) = self.per_entity_handles.remove(entity) {
            scheduler.cancel_timer(handle);
        }
    }

    fn publish_reason(&self, reason: &str, ha: &mut impl ReconcileHAService) {
        if let Some(ref entity) = self.config.reason_entity {
            // Truncate to 255 chars like Python
            let truncated = if reason.len() > 255 {
                &reason[..255]
            } else {
                reason
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ha.set_state(entity, truncated);
            }));
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests — ported from test_reconcile.py
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    /// Fake scheduler — port of test_reconcile.py::FakeScheduler
    struct FakeScheduler {
        timers: HashMap<TimerHandle, (String, String)>, // handle -> (entity, reason)
        next_handle: TimerHandle,
    }

    impl FakeScheduler {
        fn new() -> Self {
            Self {
                timers: HashMap::new(),
                next_handle: 1,
            }
        }

        /// Fire a specific timer, calling back into the reconciler.
        fn fire_timer(
            &mut self,
            handle: TimerHandle,
            reconciler: &mut Reconciler,
            ha: &mut FakeReconcileHA,
        ) -> Option<bool> {
            if let Some((entity, reason)) = self.timers.remove(&handle) {
                if entity.is_empty() {
                    // reconcile_all timer
                    reconciler.on_reconcile_all_fired();
                    Some(true)
                } else {
                    Some(reconciler.on_timer_fired(&entity, &reason, self, ha))
                }
            } else {
                None
            }
        }

        /// Fire all pending timers. Returns number of on_reconcile calls.
        fn fire_all_timers(
            &mut self,
            reconciler: &mut Reconciler,
            ha: &mut FakeReconcileHA,
            on_reconcile: &mut Vec<bool>,
        ) {
            // Collect handles first to avoid borrow issues
            let handles: Vec<_> = self.timers.keys().cloned().collect();
            for handle in handles {
                if let Some((entity, reason)) = self.timers.remove(&handle) {
                    if entity.is_empty() {
                        reconciler.on_reconcile_all_fired();
                        on_reconcile.push(true);
                    } else {
                        let result = reconciler.on_timer_fired(&entity, &reason, self, ha);
                        if result {
                            on_reconcile.push(true);
                        }
                    }
                }
            }
        }
    }

    impl ReconcileScheduler for FakeScheduler {
        fn schedule_callback(
            &mut self,
            _seconds: u32,
            entity: String,
            reason: String,
        ) -> TimerHandle {
            let handle = self.next_handle;
            self.next_handle += 1;
            self.timers.insert(handle, (entity, reason));
            handle
        }
        fn cancel_timer(&mut self, handle: TimerHandle) {
            self.timers.remove(&handle);
        }
    }

    /// Fake HA — port of test_reconcile.py::FakeReconcileHA
    struct FakeReconcileHA {
        states: HashMap<String, String>,
        set_state_calls: Vec<(String, String)>,
    }

    impl FakeReconcileHA {
        fn new(states: HashMap<String, String>) -> Self {
            Self {
                states,
                set_state_calls: Vec::new(),
            }
        }
    }

    impl ReconcileHAService for FakeReconcileHA {
        fn get_state(&self, entity_id: &str) -> Option<String> {
            self.states.get(entity_id).cloned()
        }
        fn set_state(&mut self, entity_id: &str, state: &str) {
            self.set_state_calls
                .push((entity_id.to_string(), state.to_string()));
            self.states.insert(entity_id.to_string(), state.to_string());
        }
    }

    fn entity_set(entities: &[&str]) -> HashSet<String> {
        entities.iter().map(|e| e.to_string()).collect()
    }

    fn state_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── is_unavailable tests ───────────────────────────────────────

    #[test]
    fn unavailable_state() {
        assert!(is_unavailable(Some("unavailable")));
    }
    #[test]
    fn unknown_state() {
        assert!(is_unavailable(Some("unknown")));
    }
    #[test]
    fn empty_state() {
        assert!(is_unavailable(Some("")));
    }
    #[test]
    fn none_state() {
        assert!(is_unavailable(None));
    }
    #[test]
    fn on_state() {
        assert!(!is_unavailable(Some("on")));
    }
    #[test]
    fn off_state() {
        assert!(!is_unavailable(Some("off")));
    }
    #[test]
    fn case_insensitive() {
        assert!(is_unavailable(Some("UNAVAILABLE")));
        assert!(is_unavailable(Some("Unknown")));
    }

    // ── Reconciler tests ───────────────────────────────────────────

    #[test]
    fn register_watchers_when_disabled() {
        let config = ReconcileConfig {
            enabled: false,
            ..Default::default()
        };
        let reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let watchers = reconciler.register_watchers();
        assert!(watchers.is_empty());
    }

    #[test]
    fn register_watchers_when_enabled() {
        let config = ReconcileConfig {
            enabled: true,
            ..Default::default()
        };
        let reconciler = Reconciler::new(config, entity_set(&["light.a", "light.b"]));
        let watchers = reconciler.register_watchers();
        assert_eq!(watchers.len(), 2);
    }

    #[test]
    fn schedule_creates_timer() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 5,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(HashMap::new());

        reconciler.schedule("light.test", "test", &mut scheduler, &mut ha);

        assert_eq!(scheduler.timers.len(), 1);
    }

    #[test]
    fn schedule_when_disabled() {
        let config = ReconcileConfig {
            enabled: false,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(HashMap::new());

        reconciler.schedule("light.test", "test", &mut scheduler, &mut ha);

        assert!(scheduler.timers.is_empty());
    }

    #[test]
    fn reconcile_calls_on_reconcile() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 1,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(state_map(&[("light.test", "on")]));

        reconciler.schedule("light.test", "test", &mut scheduler, &mut ha);

        let mut reconcile_calls = Vec::new();
        scheduler.fire_all_timers(&mut reconciler, &mut ha, &mut reconcile_calls);

        assert_eq!(reconcile_calls.len(), 1);
    }

    #[test]
    fn reconcile_retries_if_still_unavailable() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 1,
            max_retries: 3,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(state_map(&[("light.test", "unavailable")]));

        reconciler.schedule("light.test", "test", &mut scheduler, &mut ha);

        let handle1 = *scheduler.timers.keys().next().unwrap();
        scheduler.fire_timer(handle1, &mut reconciler, &mut ha);

        // Should have scheduled a retry
        assert_eq!(scheduler.timers.len(), 1);
    }

    #[test]
    fn reconcile_stops_after_max_retries() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 1,
            max_retries: 2,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(state_map(&[("light.test", "unavailable")]));

        reconciler.schedule("light.test", "test", &mut scheduler, &mut ha);

        // Fire 3 times: initial + 2 retries = max_retries reached
        let mut dummy = Vec::new();
        scheduler.fire_all_timers(&mut reconciler, &mut ha, &mut dummy); // 1st
        scheduler.fire_all_timers(&mut reconciler, &mut ha, &mut dummy); // 2nd (retry 1)
        scheduler.fire_all_timers(&mut reconciler, &mut ha, &mut dummy); // 3rd (retry 2 = max)

        // Should stop scheduling
        assert!(scheduler.timers.is_empty());
    }

    #[test]
    fn entity_change_unavailable_to_available_triggers_schedule() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 5,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(state_map(&[("light.test", "on")]));

        let triggered = reconciler.on_entity_change(
            "light.test",
            Some("unavailable"),
            Some("on"),
            &mut scheduler,
            &mut ha,
        );

        assert!(triggered);
        assert_eq!(scheduler.timers.len(), 1);
    }

    #[test]
    fn entity_change_on_to_off_does_not_trigger() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 5,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(state_map(&[("light.test", "off")]));

        let triggered = reconciler.on_entity_change(
            "light.test",
            Some("on"),
            Some("off"),
            &mut scheduler,
            &mut ha,
        );

        assert!(!triggered);
        assert!(scheduler.timers.is_empty());
    }

    #[test]
    fn schedule_reconcile_all() {
        let config = ReconcileConfig {
            enabled: true,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(HashMap::new());

        reconciler.schedule_reconcile_all(10, &mut scheduler, &mut ha);

        let mut reconcile_calls = Vec::new();
        scheduler.fire_all_timers(&mut reconciler, &mut ha, &mut reconcile_calls);

        assert_eq!(reconcile_calls.len(), 1);
    }

    #[test]
    fn schedule_reconcile_all_cancels_previous() {
        let config = ReconcileConfig {
            enabled: true,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(HashMap::new());

        reconciler.schedule_reconcile_all(10, &mut scheduler, &mut ha);
        reconciler.schedule_reconcile_all(20, &mut scheduler, &mut ha);

        // Only one timer should exist
        assert_eq!(scheduler.timers.len(), 1);
    }

    #[test]
    fn reason_publishing() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 1,
            reason_entity: Some("input_text.reconcile_reason".to_string()),
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(state_map(&[("light.test", "on")]));

        reconciler.schedule("light.test", "test_reason", &mut scheduler, &mut ha);

        assert_eq!(ha.set_state_calls.len(), 1);
        let (entity, value) = &ha.set_state_calls[0];
        assert_eq!(entity, "input_text.reconcile_reason");
        assert!(value.contains("test_reason"));
    }

    #[test]
    fn reset_clears_timers() {
        let config = ReconcileConfig {
            enabled: true,
            settle_seconds: 5,
            ..Default::default()
        };
        let mut reconciler = Reconciler::new(config, entity_set(&["light.test"]));
        let mut scheduler = FakeScheduler::new();
        let mut ha = FakeReconcileHA::new(HashMap::new());

        reconciler.schedule("light.test", "test", &mut scheduler, &mut ha);
        reconciler.schedule_reconcile_all(10, &mut scheduler, &mut ha);
        assert_eq!(scheduler.timers.len(), 2);

        reconciler.reset(&mut scheduler);
        assert!(scheduler.timers.is_empty());
    }

    #[test]
    fn logs_on_register() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let logs_clone = logs.clone();

        let config = ReconcileConfig {
            enabled: true,
            ..Default::default()
        };
        let reconciler = Reconciler::new(config, entity_set(&["light.a", "light.b"]))
            .with_log(move |msg| logs_clone.borrow_mut().push(msg.to_string()));

        reconciler.register_watchers();

        let logs = logs.borrow();
        assert!(logs.iter().any(|l| l.contains("registered watchers")));
    }
}
