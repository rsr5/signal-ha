//! Stable signature computation for change detection.
//!
//! A deterministic JSON string of normalised targets, used by the
//! [`Actuator`](super::actuator::Actuator) to skip redundant updates.
//!
//! Direct port of `appdaemon_lighting.signatures`.

use crate::target::LightTarget;
use serde_json::{json, Value};

/// Compute a deterministic signature for change detection.
///
/// Normalises each target to a canonical form, sorts by entity_id,
/// and returns compact JSON.
///
/// If `include_metadata` is true, zone and layer are included in the
/// signature (useful for target publishers that need to detect layer changes).
pub fn stable_signature(targets: &[LightTarget], include_metadata: bool) -> String {
    let mut norm: Vec<Value> = targets
        .iter()
        .map(|t| {
            let bri = t.brightness;
            // Python rule: brightness ≤ 0 forces on→false
            let on = if bri <= 0 { false } else { t.on };

            let mut entry = json!({
                "brightness": bri,
                "ct_mired": t.ct_mired,
                "entity_id": t.entity_id,
                "on": on,
                "transition": t.transition,
            });

            if include_metadata {
                entry["layer"] = json!(t.layer);
                entry["zone"] = json!(t.zone);
            }

            entry
        })
        .collect();

    // Sort for determinism — matches Python sort key
    if include_metadata {
        norm.sort_by(|a, b| {
            let eid = a["entity_id"]
                .as_str()
                .unwrap()
                .cmp(b["entity_id"].as_str().unwrap());
            let layer = a["layer"]
                .as_str()
                .unwrap()
                .cmp(b["layer"].as_str().unwrap());
            let zone = a["zone"]
                .as_str()
                .unwrap()
                .cmp(b["zone"].as_str().unwrap());
            eid.then(layer).then(zone)
        });
    } else {
        norm.sort_by(|a, b| {
            a["entity_id"]
                .as_str()
                .unwrap()
                .cmp(b["entity_id"].as_str().unwrap())
        });
    }

    // Compact JSON with sorted keys — matches Python separators=(",",":")
    // serde_json::to_string already uses compact form without spaces.
    // We need sort_keys though, which serde_json does not guarantee for
    // `json!({})` — but we built our objects with keys in sorted order
    // already (brightness, ct_mired, entity_id, on, transition).
    //
    // Actually, serde_json::Value (Map) preserves insertion order, not
    // sorted. We need to explicitly sort. Let's serialise via BTreeMap.
    use std::collections::BTreeMap;

    let sorted: Vec<BTreeMap<String, Value>> = norm
        .into_iter()
        .map(|v| {
            if let Value::Object(map) = v {
                map.into_iter().collect::<BTreeMap<_, _>>()
            } else {
                BTreeMap::new()
            }
        })
        .collect();

    serde_json::to_string(&sorted).unwrap()
}

// ─────────────────────────────────────────────────────────────────────
// Tests — ported from test_signatures.py::TestStableSignature
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::LightTarget;

    #[test]
    fn empty_list() {
        let sig = stable_signature(&[], false);
        assert_eq!(sig, "[]");
    }

    #[test]
    fn single_target() {
        let targets = vec![LightTarget::new("light.kitchen", true)
            .brightness(200)
            .ct_mired(350)];
        let sig = stable_signature(&targets, false);
        assert!(sig.contains("light.kitchen"));
        assert!(sig.contains(r#""on":true"#));
        assert!(sig.contains(r#""brightness":200"#));
    }

    #[test]
    fn deterministic_ordering() {
        // Same targets in different order should produce same signature
        let a = vec![
            LightTarget::new("light.b", true).brightness(100),
            LightTarget::new("light.a", true).brightness(200),
        ];
        let b = vec![
            LightTarget::new("light.a", true).brightness(200),
            LightTarget::new("light.b", true).brightness(100),
        ];
        assert_eq!(stable_signature(&a, false), stable_signature(&b, false));
    }

    #[test]
    fn brightness_zero_means_off() {
        let targets = vec![LightTarget::new("light.kitchen", true).brightness(0)];
        let sig = stable_signature(&targets, false);
        assert!(sig.contains(r#""on":false"#));
    }

    #[test]
    fn include_metadata_false() {
        let targets = vec![LightTarget::new("light.kitchen", true)
            .brightness(200)
            .zone("ceiling")
            .layer("base")];
        let sig = stable_signature(&targets, false);
        assert!(!sig.contains("zone"));
        assert!(!sig.contains("layer"));
    }

    #[test]
    fn include_metadata_true() {
        let targets = vec![LightTarget::new("light.kitchen", true)
            .brightness(200)
            .zone("ceiling")
            .layer("base")];
        let sig = stable_signature(&targets, true);
        assert!(sig.contains(r#""zone":"ceiling""#));
        assert!(sig.contains(r#""layer":"base""#));
    }

    #[test]
    fn metadata_affects_ordering() {
        let targets = vec![
            LightTarget::new("light.a", true)
                .brightness(100)
                .zone("z2")
                .layer("base"),
            LightTarget::new("light.a", true)
                .brightness(100)
                .zone("z1")
                .layer("overlay"),
        ];
        let reversed: Vec<_> = targets.iter().rev().cloned().collect();
        assert_eq!(
            stable_signature(&targets, true),
            stable_signature(&reversed, true)
        );
    }
}
