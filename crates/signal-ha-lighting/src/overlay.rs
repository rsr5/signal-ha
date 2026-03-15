//! Overlay management for temporary lighting effects.
//!
//! An overlay temporarily takes over lighting for specific entities,
//! saving their state before and restoring after.
//!
//! Use cases: toothbrush brushing mode, movie mode, focus mode.
//!
//! Direct port of `appdaemon_lighting.overlay`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────
// HA trait for overlay operations
// ─────────────────────────────────────────────────────────────────────

/// Minimal interface for overlay HA operations.
pub trait OverlayHAService {
    fn turn_on(&mut self, entity_id: &str, kwargs: &HashMap<String, serde_json::Value>);
    fn turn_off(&mut self, entity_id: &str, kwargs: &HashMap<String, serde_json::Value>);
    /// Get entity state. If `all` is true, returns full state dict as JSON Value.
    fn get_state_all(&self, entity_id: &str) -> Option<serde_json::Value>;
}

// ─────────────────────────────────────────────────────────────────────
// LightSnapshot
// ─────────────────────────────────────────────────────────────────────

/// Snapshot of a light's current state for later restoration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LightSnapshot {
    pub entity_id: String,
    pub state: Option<String>,
    pub brightness: Option<i64>,
    pub color_temp_kelvin: Option<i64>,
    pub hs_color: Option<(f64, f64)>,
    pub rgb_color: Option<(i64, i64, i64)>,
    pub xy_color: Option<(f64, f64)>,
    pub effect: Option<String>,
}

impl LightSnapshot {
    /// Create a snapshot from an HA state dict (the `get_state(entity, attribute="all")` result).
    pub fn from_ha_state(entity_id: &str, state_dict: Option<&serde_json::Value>) -> Self {
        let empty = Self {
            entity_id: entity_id.to_string(),
            state: None,
            brightness: None,
            color_temp_kelvin: None,
            hs_color: None,
            rgb_color: None,
            xy_color: None,
            effect: None,
        };

        let dict = match state_dict {
            Some(serde_json::Value::Object(m)) => m,
            _ => return empty,
        };

        let state = dict.get("state").and_then(|v| v.as_str()).map(String::from);
        let attrs = dict
            .get("attributes")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        let brightness = attrs.get("brightness").and_then(|v| v.as_i64());
        let color_temp_kelvin = attrs.get("color_temp_kelvin").and_then(|v| v.as_i64());

        let hs_color = attrs.get("hs_color").and_then(|v| {
            let arr = v.as_array()?;
            Some((arr.first()?.as_f64()?, arr.get(1)?.as_f64()?))
        });

        let rgb_color = attrs.get("rgb_color").and_then(|v| {
            let arr = v.as_array()?;
            Some((
                arr.first()?.as_i64()?,
                arr.get(1)?.as_i64()?,
                arr.get(2)?.as_i64()?,
            ))
        });

        let xy_color = attrs.get("xy_color").and_then(|v| {
            let arr = v.as_array()?;
            Some((arr.first()?.as_f64()?, arr.get(1)?.as_f64()?))
        });

        let effect = attrs
            .get("effect")
            .and_then(|v| v.as_str())
            .map(String::from);

        Self {
            entity_id: entity_id.to_string(),
            state,
            brightness,
            color_temp_kelvin,
            hs_color,
            rgb_color,
            xy_color,
            effect,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// OverlayState + OverlayManager
// ─────────────────────────────────────────────────────────────────────

struct OverlayState {
    mode: String,
    snapshots: HashMap<String, LightSnapshot>,
    metadata: HashMap<String, serde_json::Value>,
}

/// Manages temporary lighting overlays with snapshot/restore.
pub struct OverlayManager<H: OverlayHAService> {
    ha: H,
    restore_transition: f64,
    state: Option<OverlayState>,
    log_fn: Option<Box<dyn Fn(&str)>>,
}

impl<H: OverlayHAService> OverlayManager<H> {
    pub fn new(ha: H, restore_transition: f64) -> Self {
        Self {
            ha,
            restore_transition,
            state: None,
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

    /// Whether an overlay is currently active.
    pub fn active(&self) -> bool {
        self.state.is_some()
    }

    /// The current overlay mode name, or `None`.
    pub fn active_mode(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.mode.as_str())
    }

    /// Metadata for the current overlay.
    pub fn metadata(&self) -> HashMap<String, serde_json::Value> {
        self.state
            .as_ref()
            .map(|s| s.metadata.clone())
            .unwrap_or_default()
    }

    /// Enter an overlay mode, snapshotting current light states.
    ///
    /// Returns `true` if newly entered, `false` if transitioning within
    /// an existing overlay (snapshot preserved).
    pub fn enter(
        &mut self,
        mode: &str,
        entities: &[&str],
        metadata: Option<HashMap<String, serde_json::Value>>,
        skip_snapshot_if_active: bool,
    ) -> bool {
        if self.state.is_some() && skip_snapshot_if_active {
            let st = self.state.as_mut().unwrap();
            st.mode = mode.to_string();
            if let Some(md) = metadata {
                st.metadata.extend(md);
            }
            self.log(&format!(
                "[overlay] transition to mode={mode} (preserved snapshot)"
            ));
            return false;
        }

        let mut snapshots = HashMap::new();
        for &entity_id in entities {
            let state_dict = self.ha.get_state_all(entity_id);
            snapshots.insert(
                entity_id.to_string(),
                LightSnapshot::from_ha_state(entity_id, state_dict.as_ref()),
            );
        }

        self.state = Some(OverlayState {
            mode: mode.to_string(),
            snapshots,
            metadata: metadata.unwrap_or_default(),
        });
        self.log(&format!(
            "[overlay] entered mode={mode} entities={entities:?}"
        ));
        true
    }

    /// Exit the current overlay, optionally restoring lights.
    ///
    /// Returns `true` if an overlay was exited, `false` if none active.
    pub fn exit(&mut self, restore: bool) -> bool {
        let st = match self.state.take() {
            Some(s) => s,
            None => return false,
        };

        if restore {
            for snapshot in st.snapshots.values() {
                self.restore_light(snapshot);
            }
            self.log(&format!("[overlay] exited mode={} (restored)", st.mode));
        } else {
            self.log(&format!("[overlay] exited mode={} (no restore)", st.mode));
        }

        true
    }

    /// Get the snapshot for a specific entity.
    pub fn get_snapshot(&self, entity_id: &str) -> Option<&LightSnapshot> {
        self.state
            .as_ref()
            .and_then(|s| s.snapshots.get(entity_id))
    }

    /// Get a mutable reference to the inner HA service (for tests).
    pub fn ha_mut(&mut self) -> &mut H {
        &mut self.ha
    }

    // ── Private ────────────────────────────────────────────────────

    fn restore_light(&mut self, snapshot: &LightSnapshot) {
        match snapshot.state.as_deref() {
            None | Some("unknown") | Some("unavailable") => return,
            Some("off") => {
                let mut kwargs = HashMap::new();
                kwargs.insert(
                    "transition".into(),
                    serde_json::json!(self.restore_transition),
                );
                self.ha.turn_off(&snapshot.entity_id, &kwargs);
                return;
            }
            _ => {}
        }

        let mut kwargs: HashMap<String, serde_json::Value> = HashMap::new();

        if let Some(bri) = snapshot.brightness {
            kwargs.insert("brightness".into(), serde_json::json!(bri));
        }

        if let Some(k) = snapshot.color_temp_kelvin {
            kwargs.insert("kelvin".into(), serde_json::json!(k));
        }

        // Colour mode priority: xy > hs > rgb (matches Python)
        if let Some((x, y)) = snapshot.xy_color {
            kwargs.insert("xy_color".into(), serde_json::json!([x, y]));
        } else if let Some((h, s)) = snapshot.hs_color {
            kwargs.insert("hs_color".into(), serde_json::json!([h, s]));
        } else if let Some((r, g, b)) = snapshot.rgb_color {
            kwargs.insert("rgb_color".into(), serde_json::json!([r, g, b]));
        }

        if let Some(ref eff) = snapshot.effect {
            kwargs.insert("effect".into(), serde_json::json!(eff));
        }

        if kwargs.is_empty() {
            self.ha.turn_on(&snapshot.entity_id, &HashMap::new());
        } else {
            self.ha.turn_on(&snapshot.entity_id, &kwargs);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests — ported from test_overlay.py
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fake HA for overlay testing — port of FakeOverlayHA
    struct FakeHA {
        states: HashMap<String, serde_json::Value>,
        calls: Vec<(String, String, HashMap<String, serde_json::Value>)>,
    }

    impl FakeHA {
        fn new(states: HashMap<String, serde_json::Value>) -> Self {
            Self {
                states,
                calls: Vec::new(),
            }
        }
    }

    impl OverlayHAService for FakeHA {
        fn turn_on(
            &mut self,
            entity_id: &str,
            kwargs: &HashMap<String, serde_json::Value>,
        ) {
            self.calls
                .push(("turn_on".into(), entity_id.into(), kwargs.clone()));
        }
        fn turn_off(
            &mut self,
            entity_id: &str,
            kwargs: &HashMap<String, serde_json::Value>,
        ) {
            self.calls
                .push(("turn_off".into(), entity_id.into(), kwargs.clone()));
        }
        fn get_state_all(&self, entity_id: &str) -> Option<serde_json::Value> {
            self.states.get(entity_id).cloned()
        }
    }

    fn states(pairs: Vec<(&str, serde_json::Value)>) -> HashMap<String, serde_json::Value> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    // ── LightSnapshot tests ────────────────────────────────────────

    #[test]
    fn snapshot_from_none() {
        let snap = LightSnapshot::from_ha_state("light.test", None);
        assert_eq!(snap.entity_id, "light.test");
        assert!(snap.state.is_none());
        assert!(snap.brightness.is_none());
    }

    #[test]
    fn snapshot_from_empty_dict() {
        let v = json!({});
        let snap = LightSnapshot::from_ha_state("light.test", Some(&v));
        assert!(snap.state.is_none());
    }

    #[test]
    fn snapshot_basic() {
        let v = json!({
            "state": "on",
            "attributes": { "brightness": 128, "color_temp_kelvin": 3000 }
        });
        let snap = LightSnapshot::from_ha_state("light.test", Some(&v));
        assert_eq!(snap.state.as_deref(), Some("on"));
        assert_eq!(snap.brightness, Some(128));
        assert_eq!(snap.color_temp_kelvin, Some(3000));
    }

    #[test]
    fn snapshot_with_colour_modes() {
        let v = json!({
            "state": "on",
            "attributes": {
                "brightness": 200,
                "hs_color": [180.0, 50.0],
                "rgb_color": [100, 200, 150],
                "xy_color": [0.3, 0.4],
                "effect": "rainbow"
            }
        });
        let snap = LightSnapshot::from_ha_state("light.test", Some(&v));
        assert_eq!(snap.hs_color, Some((180.0, 50.0)));
        assert_eq!(snap.rgb_color, Some((100, 200, 150)));
        assert_eq!(snap.xy_color, Some((0.3, 0.4)));
        assert_eq!(snap.effect.as_deref(), Some("rainbow"));
    }

    #[test]
    fn snapshot_off() {
        let v = json!({"state": "off", "attributes": {}});
        let snap = LightSnapshot::from_ha_state("light.test", Some(&v));
        assert_eq!(snap.state.as_deref(), Some("off"));
        assert!(snap.brightness.is_none());
    }

    // ── OverlayManager tests ───────────────────────────────────────

    #[test]
    fn initially_inactive() {
        let ha = FakeHA::new(HashMap::new());
        let mgr = OverlayManager::new(ha, 0.8);
        assert!(!mgr.active());
        assert!(mgr.active_mode().is_none());
        assert!(mgr.metadata().is_empty());
    }

    #[test]
    fn enter_becomes_active() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "on", "attributes": {"brightness": 100}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        let result = mgr.enter("brushing", &["light.test"], None, true);

        assert!(result);
        assert!(mgr.active());
        assert_eq!(mgr.active_mode(), Some("brushing"));
    }

    #[test]
    fn enter_takes_snapshot() {
        let ha = FakeHA::new(states(vec![
            (
                "light.sink",
                json!({"state": "on", "attributes": {"brightness": 150}}),
            ),
            (
                "light.shower",
                json!({"state": "off", "attributes": {}}),
            ),
        ]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        mgr.enter("test", &["light.sink", "light.shower"], None, true);

        let snap_sink = mgr.get_snapshot("light.sink").unwrap();
        assert_eq!(snap_sink.state.as_deref(), Some("on"));
        assert_eq!(snap_sink.brightness, Some(150));

        let snap_shower = mgr.get_snapshot("light.shower").unwrap();
        assert_eq!(snap_shower.state.as_deref(), Some("off"));
    }

    #[test]
    fn enter_with_metadata() {
        let ha = FakeHA::new(HashMap::new());
        let mut mgr = OverlayManager::new(ha, 0.8);

        let mut md = HashMap::new();
        md.insert("scene".into(), json!("action"));
        mgr.enter("movie", &[], Some(md), true);

        assert_eq!(mgr.metadata()["scene"], json!("action"));
    }

    #[test]
    fn exit_restores_light_on() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "on", "attributes": {"brightness": 200, "color_temp_kelvin": 4000}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.5);

        mgr.enter("test", &["light.test"], None, true);
        mgr.ha_mut().calls.clear();

        mgr.exit(true);

        assert!(!mgr.active());
        assert_eq!(mgr.ha_mut().calls.len(), 1);
        let (action, entity, kwargs) = &mgr.ha.calls[0];
        assert_eq!(action, "turn_on");
        assert_eq!(entity, "light.test");
        assert_eq!(kwargs["brightness"], 200);
        assert_eq!(kwargs["kelvin"], 4000);
    }

    #[test]
    fn exit_restores_light_off() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "off", "attributes": {}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 1.0);

        mgr.enter("test", &["light.test"], None, true);
        mgr.ha_mut().calls.clear();

        mgr.exit(true);

        assert_eq!(mgr.ha.calls.len(), 1);
        let (action, entity, kwargs) = &mgr.ha.calls[0];
        assert_eq!(action, "turn_off");
        assert_eq!(entity, "light.test");
        assert_eq!(kwargs["transition"], 1.0);
    }

    #[test]
    fn exit_without_restore() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "on", "attributes": {"brightness": 100}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        mgr.enter("test", &["light.test"], None, true);
        mgr.ha_mut().calls.clear();

        mgr.exit(false);

        assert!(!mgr.active());
        assert!(mgr.ha.calls.is_empty());
    }

    #[test]
    fn exit_when_not_active() {
        let ha = FakeHA::new(HashMap::new());
        let mut mgr = OverlayManager::new(ha, 0.8);

        let result = mgr.exit(true);
        assert!(!result);
    }

    #[test]
    fn transition_between_modes_preserves_snapshot() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "on", "attributes": {"brightness": 50}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        mgr.enter("mode1", &["light.test"], None, true);

        let result = mgr.enter("mode2", &["light.test"], None, true);

        assert!(!result); // transition, not new entry
        assert_eq!(mgr.active_mode(), Some("mode2"));

        // Original snapshot preserved
        let snap = mgr.get_snapshot("light.test").unwrap();
        assert_eq!(snap.brightness, Some(50));
    }

    #[test]
    fn restore_with_rgb_color() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "on", "attributes": {"brightness": 255, "rgb_color": [255, 0, 0]}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        mgr.enter("test", &["light.test"], None, true);
        mgr.ha_mut().calls.clear();
        mgr.exit(true);

        let (_, _, kwargs) = &mgr.ha.calls[0];
        assert_eq!(kwargs["rgb_color"], json!([255, 0, 0]));
    }

    #[test]
    fn restore_with_effect() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "on", "attributes": {"brightness": 100, "effect": "colorloop"}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        mgr.enter("test", &["light.test"], None, true);
        mgr.ha_mut().calls.clear();
        mgr.exit(true);

        let (_, _, kwargs) = &mgr.ha.calls[0];
        assert_eq!(kwargs["effect"], "colorloop");
    }

    #[test]
    fn restore_unavailable_light_skipped() {
        let ha = FakeHA::new(states(vec![(
            "light.test",
            json!({"state": "unavailable", "attributes": {}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);

        mgr.enter("test", &["light.test"], None, true);
        mgr.ha_mut().calls.clear();
        mgr.exit(true);

        assert!(mgr.ha.calls.is_empty());
    }

    #[test]
    fn get_snapshot_no_overlay() {
        let ha = FakeHA::new(HashMap::new());
        let mgr = OverlayManager::new(ha, 0.8);
        assert!(mgr.get_snapshot("light.test").is_none());
    }

    #[test]
    fn get_snapshot_unknown_entity() {
        let ha = FakeHA::new(states(vec![(
            "light.a",
            json!({"state": "on", "attributes": {}}),
        )]));
        let mut mgr = OverlayManager::new(ha, 0.8);
        mgr.enter("test", &["light.a"], None, true);

        assert!(mgr.get_snapshot("light.b").is_none());
    }

    // ── Logging tests ──────────────────────────────────────────────

    #[test]
    fn logs_on_enter() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let logs_clone = logs.clone();

        let ha = FakeHA::new(HashMap::new());
        let mut mgr = OverlayManager::new(ha, 0.8)
            .with_log(move |msg| logs_clone.borrow_mut().push(msg.to_string()));

        mgr.enter("brushing", &["light.test"], None, true);

        let logs = logs.borrow();
        assert!(logs.iter().any(|l| l.contains("entered mode=brushing")));
    }

    #[test]
    fn logs_on_exit() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let logs_clone = logs.clone();

        let ha = FakeHA::new(HashMap::new());
        let mut mgr = OverlayManager::new(ha, 0.8)
            .with_log(move |msg| logs_clone.borrow_mut().push(msg.to_string()));

        mgr.enter("brushing", &[], None, true);
        mgr.exit(true);

        let logs = logs.borrow();
        assert!(logs.iter().any(|l| l.contains("exited mode=brushing")));
    }
}
