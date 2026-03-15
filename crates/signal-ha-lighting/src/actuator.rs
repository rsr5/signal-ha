//! Shared actuator for applying [`LightTarget`] lists to Home Assistant.
//!
//! Handles:
//! - Rate limiting (global and per-entity)
//! - Deadband tolerances (don't update for tiny changes)
//! - Drift detection (repair manual changes / reboots)
//! - Signature-based change detection (skip if nothing changed)
//!
//! Direct port of `appdaemon_lighting.actuator`.

use crate::signature::stable_signature;
use crate::target::LightTarget;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────
// HA service trait  (sync — matches the Python HAService Protocol)
// ─────────────────────────────────────────────────────────────────────

/// Minimal interface for Home Assistant service calls.
///
/// Implement this with a mock for testing, or with an adapter that wraps
/// [`HaClient`](signal_ha::HaClient) for production.
pub trait HAService {
    fn turn_on(&mut self, entity_id: &str, kwargs: &HashMap<String, serde_json::Value>);
    fn turn_off(&mut self, entity_id: &str, kwargs: &HashMap<String, serde_json::Value>);
    fn get_state(&self, entity_id: &str) -> Option<String>;
    fn get_attributes(&self, entity_id: &str) -> HashMap<String, serde_json::Value>;
    fn now(&self) -> DateTime<Utc>;
}

// ─────────────────────────────────────────────────────────────────────
// Config + result
// ─────────────────────────────────────────────────────────────────────

/// Configuration for the shared actuator.
#[derive(Debug, Clone)]
pub struct ActuatorConfig {
    /// Global rate limit — minimum seconds between any two apply() calls.
    pub min_apply_interval_s: f64,
    /// Per-entity rate limit — minimum seconds between writes to the same light.
    pub per_light_min_interval_s: f64,
    /// Brightness deadband (0–255). Don't write if within this tolerance.
    pub brightness_tol: i32,
    /// Colour temperature deadband (mireds).
    pub ct_tol_mired: i32,
    /// Whether to log changes via `tracing::info`.
    pub log_changes: bool,
}

impl Default for ActuatorConfig {
    fn default() -> Self {
        Self {
            min_apply_interval_s: 3.0,
            per_light_min_interval_s: 6.0,
            brightness_tol: 2,
            ct_tol_mired: 5,
            log_changes: true,
        }
    }
}

/// Result of an [`Actuator::apply`] call.
#[derive(Debug, Default)]
pub struct ApplyResult {
    /// Number of lights actually written.
    pub applied: u32,
    /// Suppressed by per-light rate limit.
    pub suppressed_rate: u32,
    /// Suppressed because already matches within deadband.
    pub suppressed_match: u32,
    /// Entire apply skipped because of global rate limit.
    pub skipped_global_rate: bool,
    /// Signature was same as last apply.
    pub sig_unchanged: bool,
}

// ─────────────────────────────────────────────────────────────────────
// Actuator
// ─────────────────────────────────────────────────────────────────────

/// Decision for a single target.
#[derive(Debug, PartialEq)]
enum WriteDecision {
    Write,
    Rate,
    Match,
}

/// Reusable actuator that applies `LightTarget` lists to HA.
pub struct Actuator<H: HAService> {
    pub config: ActuatorConfig,
    ha: H,

    last_sig: String,
    last_apply_at: Option<DateTime<Utc>>,
    light_last_write: HashMap<String, DateTime<Utc>>,
    light_last_values: HashMap<String, LightTarget>,
}

impl<H: HAService> Actuator<H> {
    pub fn new(config: ActuatorConfig, ha: H) -> Self {
        Self {
            config,
            ha,
            last_sig: String::new(),
            last_apply_at: None,
            light_last_write: HashMap::new(),
            light_last_values: HashMap::new(),
        }
    }

    /// Apply targets, respecting rate limits and deadbands.
    pub fn apply(&mut self, targets: &[LightTarget]) -> ApplyResult {
        let mut result = ApplyResult::default();
        let now = self.ha.now();

        // Signature for change detection
        let sig = stable_signature(targets, false);
        result.sig_unchanged = sig == self.last_sig;

        // Global rate limiting
        if let Some(last) = self.last_apply_at {
            let elapsed = (now - last).num_milliseconds() as f64 / 1000.0;
            if elapsed < self.config.min_apply_interval_s {
                result.skipped_global_rate = true;
                return result;
            }
        }

        // Categorise targets
        let mut to_turn_on = Vec::new();
        let mut to_turn_off = Vec::new();

        for t in targets {
            match self.should_write(t, now) {
                WriteDecision::Rate => result.suppressed_rate += 1,
                WriteDecision::Match => result.suppressed_match += 1,
                WriteDecision::Write => {
                    if t.on {
                        to_turn_on.push(t);
                    } else {
                        to_turn_off.push(t);
                    }
                }
            }
        }

        // Apply ON first, then OFF (matches Python order)
        for t in &to_turn_on {
            self.apply_on(t, now);
            result.applied += 1;
        }
        for t in &to_turn_off {
            self.apply_off(t, now);
            result.applied += 1;
        }

        // Update timestamps
        if result.applied > 0 {
            self.last_apply_at = Some(now);
        }

        // Only update signature when targets actually changed
        if !result.sig_unchanged {
            self.last_sig = sig;
        }

        if self.config.log_changes && result.applied > 0 {
            tracing::info!(
                applied = result.applied,
                suppressed_rate = result.suppressed_rate,
                suppressed_match = result.suppressed_match,
                "[actuator] apply"
            );
        }

        result
    }

    /// Reset all internal state.
    pub fn reset(&mut self) {
        self.last_sig.clear();
        self.last_apply_at = None;
        self.light_last_write.clear();
        self.light_last_values.clear();
    }

    /// Get a mutable reference to the inner HA service (useful for tests).
    pub fn ha_mut(&mut self) -> &mut H {
        &mut self.ha
    }

    // ── Private ────────────────────────────────────────────────────

    fn should_write(&self, t: &LightTarget, now: DateTime<Utc>) -> WriteDecision {
        // Per-light rate limiting
        if let Some(&last_write) = self.light_last_write.get(&t.entity_id) {
            let elapsed = (now - last_write).num_milliseconds() as f64 / 1000.0;
            if elapsed < self.config.per_light_min_interval_s {
                return WriteDecision::Rate;
            }
        }

        // Get actual HA state
        let actual_state = self.ha.get_state(&t.entity_id);
        let actual_attrs = self.ha.get_attributes(&t.entity_id);

        let actual_on = actual_state.as_deref() == Some("on");

        // State mismatch → write
        if t.on && !actual_on {
            return WriteDecision::Write;
        }
        if !t.on && actual_on {
            return WriteDecision::Write;
        }

        // If off and HA already off, no need
        if !t.on {
            return WriteDecision::Match;
        }

        // On: compare brightness
        let actual_bri = actual_attrs
            .get("brightness")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as i32;
        if (actual_bri - t.brightness).abs() > self.config.brightness_tol {
            return WriteDecision::Write;
        }

        // On: compare colour temperature
        if let Some(actual_ct) = actual_attrs.get("color_temp").and_then(|v| v.as_i64())
        {
            if (actual_ct as i32 - t.ct_mired).abs() > self.config.ct_tol_mired {
                return WriteDecision::Write;
            }
        }

        WriteDecision::Match
    }

    fn apply_on(&mut self, t: &LightTarget, now: DateTime<Utc>) {
        let mut kwargs = HashMap::new();
        kwargs.insert(
            "brightness".into(),
            serde_json::Value::Number(t.brightness.into()),
        );
        if t.ct_mired > 0 {
            kwargs.insert(
                "color_temp".into(),
                serde_json::Value::Number(t.ct_mired.into()),
            );
        }
        if t.transition > 0 {
            kwargs.insert(
                "transition".into(),
                serde_json::Value::Number(t.transition.into()),
            );
        }
        self.ha.turn_on(&t.entity_id, &kwargs);
        self.record_write(t, now);
    }

    fn apply_off(&mut self, t: &LightTarget, now: DateTime<Utc>) {
        let mut kwargs = HashMap::new();
        if t.transition > 0 {
            kwargs.insert(
                "transition".into(),
                serde_json::Value::Number(t.transition.into()),
            );
        }
        self.ha.turn_off(&t.entity_id, &kwargs);
        self.record_write(t, now);
    }

    fn record_write(&mut self, t: &LightTarget, now: DateTime<Utc>) {
        self.light_last_write.insert(t.entity_id.clone(), now);
        self.light_last_values.insert(t.entity_id.clone(), t.clone());
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests — ported from test_actuator.py
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// Mock HA service — direct port of test_actuator.py::MockHAService
    #[derive(Debug)]
    struct MockHA {
        now: DateTime<Utc>,
        states: HashMap<String, (String, HashMap<String, serde_json::Value>)>,
        calls: Vec<(String, String, HashMap<String, serde_json::Value>)>,
    }

    impl MockHA {
        fn new() -> Self {
            Self {
                now: chrono::Utc.with_ymd_and_hms(2025, 2, 1, 12, 0, 0).unwrap(),
                states: HashMap::new(),
                calls: Vec::new(),
            }
        }

        fn advance_time(&mut self, secs: f64) {
            self.now = self.now + Duration::milliseconds((secs * 1000.0) as i64);
        }

        fn set_state(&mut self, entity_id: &str, state: &str, attrs: HashMap<String, serde_json::Value>) {
            self.states.insert(entity_id.into(), (state.into(), attrs));
        }

        fn clear_calls(&mut self) {
            self.calls.clear();
        }
    }

    impl HAService for MockHA {
        fn turn_on(&mut self, entity_id: &str, kwargs: &HashMap<String, serde_json::Value>) {
            self.calls.push(("turn_on".into(), entity_id.into(), kwargs.clone()));
            // Update mock state
            let (_, attrs) = self.states.entry(entity_id.into()).or_insert_with(|| ("off".into(), HashMap::new()));
            *attrs = kwargs.clone();
            self.states.get_mut(entity_id).unwrap().0 = "on".into();
        }

        fn turn_off(&mut self, entity_id: &str, kwargs: &HashMap<String, serde_json::Value>) {
            self.calls.push(("turn_off".into(), entity_id.into(), kwargs.clone()));
            self.states.insert(entity_id.into(), ("off".into(), HashMap::new()));
        }

        fn get_state(&self, entity_id: &str) -> Option<String> {
            self.states.get(entity_id).map(|(s, _)| s.clone())
        }

        fn get_attributes(&self, entity_id: &str) -> HashMap<String, serde_json::Value> {
            self.states.get(entity_id).map(|(_, a)| a.clone()).unwrap_or_default()
        }

        fn now(&self) -> DateTime<Utc> {
            self.now
        }
    }

    use chrono::TimeZone;

    fn attrs(pairs: &[(&str, i64)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), serde_json::json!(v))).collect()
    }

    // ── TestActuatorBasics ─────────────────────────────────────────

    #[test]
    fn apply_single_light_on() {
        let ha = MockHA::new();
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200).ct_mired(300)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
        assert_eq!(actuator.ha.calls.len(), 1);
        assert_eq!(actuator.ha.calls[0].0, "turn_on");
        assert_eq!(actuator.ha.calls[0].1, "light.kitchen");
        assert_eq!(actuator.ha.calls[0].2["brightness"], 200);
        assert_eq!(actuator.ha.calls[0].2["color_temp"], 300);
    }

    #[test]
    fn apply_single_light_off() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "on", attrs(&[("brightness", 200)]));
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.kitchen", false)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
        assert_eq!(actuator.ha.calls.len(), 1);
        assert_eq!(actuator.ha.calls[0].0, "turn_off");
        assert_eq!(actuator.ha.calls[0].1, "light.kitchen");
    }

    #[test]
    fn apply_multiple_lights() {
        let mut ha = MockHA::new();
        ha.set_state("light.island", "on", attrs(&[("brightness", 100)]));
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![
            LightTarget::new("light.kitchen", true).brightness(200),
            LightTarget::new("light.dining", true).brightness(150),
            LightTarget::new("light.island", false),
        ];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 3);
        // ON calls first, then OFF
        assert_eq!(actuator.ha.calls[0].0, "turn_on");
        assert_eq!(actuator.ha.calls[1].0, "turn_on");
        assert_eq!(actuator.ha.calls[2].0, "turn_off");
    }

    #[test]
    fn apply_with_transition() {
        let ha = MockHA::new();
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200).transition(5)];
        actuator.apply(&targets);

        assert_eq!(actuator.ha.calls[0].2["transition"], 5);
    }

    #[test]
    fn apply_zero_ct_not_passed() {
        let ha = MockHA::new();
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200).ct_mired(0)];
        actuator.apply(&targets);

        assert!(!actuator.ha.calls[0].2.contains_key("color_temp"));
    }

    // ── TestGlobalRateLimiting ─────────────────────────────────────

    #[test]
    fn global_rate_limit_blocks_rapid_calls() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 3.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];

        let result1 = actuator.apply(&targets);
        assert_eq!(result1.applied, 1);
        assert!(!result1.skipped_global_rate);

        actuator.ha.clear_calls();
        let result2 = actuator.apply(&targets);
        assert_eq!(result2.applied, 0);
        assert!(result2.skipped_global_rate);
        assert!(actuator.ha.calls.is_empty());
    }

    #[test]
    fn global_rate_limit_allows_after_interval() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 3.0, per_light_min_interval_s: 3.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        actuator.apply(&targets);
        actuator.ha.clear_calls();

        actuator.ha.advance_time(4.0);

        let targets2 = vec![LightTarget::new("light.kitchen", true).brightness(210)];
        let result = actuator.apply(&targets2);
        assert_eq!(result.applied, 1);
        assert!(!result.skipped_global_rate);
    }

    // ── TestPerLightRateLimiting ───────────────────────────────────

    #[test]
    fn per_light_rate_limit() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, per_light_min_interval_s: 6.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        let result1 = actuator.apply(&targets);
        assert_eq!(result1.applied, 1);

        actuator.ha.advance_time(2.0);
        actuator.ha.clear_calls();

        let targets2 = vec![LightTarget::new("light.kitchen", true).brightness(210)];
        let result2 = actuator.apply(&targets2);
        assert_eq!(result2.applied, 0);
        assert_eq!(result2.suppressed_rate, 1);
        assert!(actuator.ha.calls.is_empty());
    }

    #[test]
    fn per_light_rate_limit_different_lights() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, per_light_min_interval_s: 6.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets1 = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        actuator.apply(&targets1);

        actuator.ha.advance_time(2.0);
        actuator.ha.clear_calls();

        let targets2 = vec![LightTarget::new("light.dining", true).brightness(200)];
        let result = actuator.apply(&targets2);
        assert_eq!(result.applied, 1);
    }

    #[test]
    fn per_light_rate_limit_allows_after_interval() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, per_light_min_interval_s: 6.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        actuator.apply(&targets);

        actuator.ha.advance_time(7.0);
        actuator.ha.clear_calls();

        let targets2 = vec![LightTarget::new("light.kitchen", true).brightness(210)];
        let result = actuator.apply(&targets2);
        assert_eq!(result.applied, 1);
    }

    // ── TestDeadbandTolerance ──────────────────────────────────────

    #[test]
    fn brightness_within_tolerance_not_written() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "on", attrs(&[("brightness", 200)]));
        let config = ActuatorConfig { brightness_tol: 5, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(202)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 0);
        assert_eq!(result.suppressed_match, 1);
    }

    #[test]
    fn brightness_outside_tolerance_written() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "on", attrs(&[("brightness", 200)]));
        let config = ActuatorConfig { brightness_tol: 5, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(210)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
    }

    #[test]
    fn ct_within_tolerance_not_written() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "on", attrs(&[("brightness", 200), ("color_temp", 300)]));
        let config = ActuatorConfig { brightness_tol: 5, ct_tol_mired: 10, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(202).ct_mired(305)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 0);
        assert_eq!(result.suppressed_match, 1);
    }

    #[test]
    fn ct_outside_tolerance_written() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "on", attrs(&[("brightness", 200), ("color_temp", 300)]));
        let config = ActuatorConfig { brightness_tol: 5, ct_tol_mired: 10, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200).ct_mired(350)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
    }

    // ── TestDriftDetection ─────────────────────────────────────────

    #[test]
    fn state_drift_detected() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "off", HashMap::new());
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
        assert_eq!(actuator.ha.calls[0].0, "turn_on");
    }

    #[test]
    fn brightness_drift_detected() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "on", attrs(&[("brightness", 100)]));
        let config = ActuatorConfig { brightness_tol: 5, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
    }

    #[test]
    fn signature_unchanged_still_checks_drift() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, per_light_min_interval_s: 1.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];

        actuator.apply(&targets);
        actuator.ha.advance_time(2.0);
        actuator.ha.clear_calls();

        // Simulate manual dimming
        actuator.ha.set_state("light.kitchen", "on", attrs(&[("brightness", 100)]));

        let result = actuator.apply(&targets);
        assert!(result.sig_unchanged);
        assert_eq!(result.applied, 1);
    }

    // ── TestSignatureBasedSkip ─────────────────────────────────────

    #[test]
    fn signature_tracked() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, per_light_min_interval_s: 1.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];

        let result1 = actuator.apply(&targets);
        assert!(!result1.sig_unchanged);

        actuator.ha.advance_time(2.0);

        let result2 = actuator.apply(&targets);
        assert!(result2.sig_unchanged);
    }

    #[test]
    fn signature_changes_on_different_targets() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, per_light_min_interval_s: 1.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets1 = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        actuator.apply(&targets1);

        actuator.ha.advance_time(2.0);

        let targets2 = vec![LightTarget::new("light.kitchen", true).brightness(210)];
        let result = actuator.apply(&targets2);
        assert!(!result.sig_unchanged);
    }

    // ── TestReset ──────────────────────────────────────────────────

    #[test]
    fn reset_clears_state() {
        let ha = MockHA::new();
        let config = ActuatorConfig { min_apply_interval_s: 1.0, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        actuator.apply(&targets);

        let result1 = actuator.apply(&targets);
        assert!(result1.skipped_global_rate);

        actuator.reset();
        actuator.ha.clear_calls();
        actuator.ha.set_state("light.kitchen", "off", HashMap::new());

        let result2 = actuator.apply(&targets);
        assert!(!result2.skipped_global_rate);
        assert_eq!(result2.applied, 1);
    }

    // ── TestEdgeCases ──────────────────────────────────────────────

    #[test]
    fn empty_targets_list() {
        let ha = MockHA::new();
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let result = actuator.apply(&[]);
        assert_eq!(result.applied, 0);
        assert!(actuator.ha.calls.is_empty());
    }

    #[test]
    fn unknown_entity_state() {
        let ha = MockHA::new();
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.unknown", true).brightness(200)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
    }

    #[test]
    fn light_off_when_already_off() {
        let mut ha = MockHA::new();
        ha.set_state("light.kitchen", "off", HashMap::new());
        let mut actuator = Actuator::new(ActuatorConfig::default(), ha);

        let targets = vec![LightTarget::new("light.kitchen", false)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 0);
        assert_eq!(result.suppressed_match, 1);
    }

    #[test]
    fn no_logger_no_crash() {
        let ha = MockHA::new();
        let config = ActuatorConfig { log_changes: true, ..Default::default() };
        let mut actuator = Actuator::new(config, ha);

        let targets = vec![LightTarget::new("light.kitchen", true).brightness(200)];
        let result = actuator.apply(&targets);

        assert_eq!(result.applied, 1);
    }
}
