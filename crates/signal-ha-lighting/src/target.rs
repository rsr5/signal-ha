//! Per-entity lighting target.
//!
//! Direct port of `appdaemon_lighting.types.LightTarget`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Universal per-entity lighting target.
///
/// The common interface between *planners* (which decide what should happen)
/// and *actuators* (which make it happen safely).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LightTarget {
    pub entity_id: String,
    pub on: bool,
    #[serde(default)]
    pub brightness: i32,
    #[serde(default)]
    pub ct_mired: i32,
    #[serde(default)]
    pub transition: i32,
    #[serde(default)]
    pub zone: String,
    #[serde(default)]
    pub layer: String,
    #[serde(default)]
    pub reason: String,
}

impl LightTarget {
    /// Shorthand constructor for the common case.
    pub fn new(entity_id: impl Into<String>, on: bool) -> Self {
        Self {
            entity_id: entity_id.into(),
            on,
            brightness: 0,
            ct_mired: 0,
            transition: 0,
            zone: String::new(),
            layer: String::new(),
            reason: String::new(),
        }
    }

    /// Builder — set brightness.
    pub fn brightness(mut self, b: i32) -> Self {
        self.brightness = b;
        self
    }

    /// Builder — set colour temperature in mireds.
    pub fn ct_mired(mut self, ct: i32) -> Self {
        self.ct_mired = ct;
        self
    }

    /// Builder — set transition time in seconds.
    pub fn transition(mut self, t: i32) -> Self {
        self.transition = t;
        self
    }

    /// Builder — set zone.
    pub fn zone(mut self, z: impl Into<String>) -> Self {
        self.zone = z.into();
        self
    }

    /// Builder — set layer.
    pub fn layer(mut self, l: impl Into<String>) -> Self {
        self.layer = l.into();
        self
    }

    /// Builder — set reason.
    pub fn reason(mut self, r: impl Into<String>) -> Self {
        self.reason = r.into();
        self
    }

    /// Convert to a `HashMap` for JSON serialisation / HA attributes.
    ///
    /// Matches the Python `LightTarget.to_dict()`.
    pub fn to_map(&self) -> HashMap<String, serde_json::Value> {
        use serde_json::Value;
        let mut m = HashMap::new();
        m.insert("entity_id".into(), Value::String(self.entity_id.clone()));
        m.insert("on".into(), Value::Bool(self.on));
        m.insert("brightness".into(), Value::Number(self.brightness.into()));
        m.insert("ct_mired".into(), Value::Number(self.ct_mired.into()));
        m.insert("transition".into(), Value::Number(self.transition.into()));
        m.insert("zone".into(), Value::String(self.zone.clone()));
        m.insert("layer".into(), Value::String(self.layer.clone()));
        m.insert("reason".into(), Value::String(self.reason.clone()));
        m
    }

    /// Create from a JSON map. Mirrors Python `LightTarget.from_dict()`.
    pub fn from_map(d: &HashMap<String, serde_json::Value>) -> Self {
        use serde_json::Value;

        let str_field = |key: &str| -> String {
            match d.get(key) {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            }
        };
        let int_field = |key: &str| -> i32 {
            match d.get(key) {
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0) as i32,
                _ => 0,
            }
        };
        let bool_field = |key: &str| -> bool {
            match d.get(key) {
                Some(Value::Bool(b)) => *b,
                _ => false,
            }
        };

        Self {
            entity_id: str_field("entity_id"),
            on: bool_field("on"),
            brightness: int_field("brightness"),
            ct_mired: int_field("ct_mired"),
            transition: int_field("transition"),
            zone: str_field("zone"),
            layer: str_field("layer"),
            reason: str_field("reason"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests — ported from test_signatures.py::TestLightTarget
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation() {
        let t = LightTarget::new("light.kitchen", true)
            .brightness(200)
            .ct_mired(350);
        assert_eq!(t.entity_id, "light.kitchen");
        assert!(t.on);
        assert_eq!(t.brightness, 200);
        assert_eq!(t.ct_mired, 350);
        assert_eq!(t.transition, 0);
        assert_eq!(t.zone, "");
        assert_eq!(t.layer, "");
    }

    #[test]
    fn to_map_roundtrip() {
        let original = LightTarget::new("light.kitchen", true)
            .brightness(200)
            .ct_mired(350)
            .zone("kitchen_ceiling")
            .layer("base");
        let d = original.to_map();
        assert_eq!(
            d["entity_id"],
            serde_json::Value::String("light.kitchen".into())
        );
        assert_eq!(d["on"], serde_json::Value::Bool(true));
        assert_eq!(d["brightness"], serde_json::json!(200));
        assert_eq!(d["ct_mired"], serde_json::json!(350));
        assert_eq!(d["zone"], serde_json::Value::String("kitchen_ceiling".into()));
        assert_eq!(d["layer"], serde_json::Value::String("base".into()));
    }

    #[test]
    fn from_map_basic() {
        let mut d = HashMap::new();
        d.insert(
            "entity_id".into(),
            serde_json::Value::String("light.kitchen".into()),
        );
        d.insert("on".into(), serde_json::Value::Bool(true));
        d.insert("brightness".into(), serde_json::json!(200));
        d.insert("ct_mired".into(), serde_json::json!(350));
        d.insert(
            "zone".into(),
            serde_json::Value::String("kitchen_ceiling".into()),
        );
        d.insert("layer".into(), serde_json::Value::String("base".into()));

        let t = LightTarget::from_map(&d);
        assert_eq!(t.entity_id, "light.kitchen");
        assert!(t.on);
        assert_eq!(t.brightness, 200);
        assert_eq!(t.ct_mired, 350);
        assert_eq!(t.zone, "kitchen_ceiling");
        assert_eq!(t.layer, "base");
    }

    #[test]
    fn from_map_missing_fields() {
        let mut d = HashMap::new();
        d.insert(
            "entity_id".into(),
            serde_json::Value::String("light.kitchen".into()),
        );
        d.insert("on".into(), serde_json::Value::Bool(true));

        let t = LightTarget::from_map(&d);
        assert_eq!(t.entity_id, "light.kitchen");
        assert!(t.on);
        assert_eq!(t.brightness, 0);
        assert_eq!(t.ct_mired, 0);
    }

    #[test]
    fn roundtrip_map() {
        let original = LightTarget::new("light.kitchen", true)
            .brightness(200)
            .ct_mired(350)
            .transition(2)
            .zone("kitchen_ceiling")
            .layer("cooking")
            .reason("cooking active");
        let restored = LightTarget::from_map(&original.to_map());
        assert_eq!(original, restored);
    }

    #[test]
    fn serde_roundtrip() {
        let original = LightTarget::new("light.kitchen", true)
            .brightness(200)
            .ct_mired(350);
        let json = serde_json::to_string(&original).unwrap();
        let restored: LightTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }
}
