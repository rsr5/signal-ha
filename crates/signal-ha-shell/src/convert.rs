//! Data conversion — MontyObject ↔ JSON, HA state → EntityState dataclass.

use monty::MontyObject;

// ── MontyObject ↔ JSON ─────────────────────────────────────────

/// Convert a MontyObject to a serde_json::Value.
pub fn monty_obj_to_json(obj: &MontyObject) -> serde_json::Value {
    match obj {
        MontyObject::None => serde_json::Value::Null,
        MontyObject::Bool(b) => serde_json::Value::Bool(*b),
        MontyObject::Int(n) => serde_json::json!(n),
        MontyObject::Float(f) => serde_json::json!(f),
        MontyObject::String(s) => serde_json::Value::String(s.clone()),
        MontyObject::List(items) => {
            serde_json::Value::Array(items.iter().map(monty_obj_to_json).collect())
        }
        MontyObject::Tuple(items) => {
            serde_json::Value::Array(items.iter().map(monty_obj_to_json).collect())
        }
        MontyObject::Dict(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => format!("{other}"),
                };
                map.insert(key, monty_obj_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        MontyObject::Set(items) => {
            serde_json::Value::Array(items.iter().map(monty_obj_to_json).collect())
        }
        MontyObject::FrozenSet(items) => {
            serde_json::Value::Array(items.iter().map(monty_obj_to_json).collect())
        }
        MontyObject::Bytes(b) => {
            serde_json::Value::String(format!("b\"{}\"", String::from_utf8_lossy(b)))
        }
        MontyObject::Dataclass { name, attrs, .. } => {
            let mut map = serde_json::Map::new();
            map.insert("__type__".to_string(), serde_json::json!(name));
            for (k, v) in attrs {
                let key = match k {
                    MontyObject::String(s) => s.clone(),
                    other => format!("{other}"),
                };
                map.insert(key, monty_obj_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        // Catch-all for new variants (Ellipsis, BigInt, NamedTuple, Exception, Type, etc.)
        other => serde_json::Value::String(format!("{other}")),
    }
}

/// Convert a JSON value to a MontyObject.
pub fn json_to_monty_obj(value: &serde_json::Value) -> MontyObject {
    match value {
        serde_json::Value::Null => MontyObject::None,
        serde_json::Value::Bool(b) => MontyObject::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::Int(i)
            } else if let Some(f) = n.as_f64() {
                MontyObject::Float(f)
            } else {
                MontyObject::None
            }
        }
        serde_json::Value::String(s) => MontyObject::String(s.clone()),
        serde_json::Value::Array(arr) => {
            MontyObject::List(arr.iter().map(json_to_monty_obj).collect())
        }
        serde_json::Value::Object(map) => {
            let pairs: Vec<(MontyObject, MontyObject)> = map
                .iter()
                .map(|(k, v)| (MontyObject::String(k.clone()), json_to_monty_obj(v)))
                .collect();
            MontyObject::Dict(pairs.into())
        }
    }
}

// ── HA state → EntityState dataclass ───────────────────────────

/// Convert a HA state JSON object to an EntityState dataclass.
///
/// This is the canonical construction shared by both shell-engine
/// (WASM) and signal-ha-agent (native).
pub fn json_to_entity_state(value: &serde_json::Value) -> MontyObject {
    let entity_id = value
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state = value
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let last_changed = value
        .get("last_changed")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let last_updated = value
        .get("last_updated")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let domain = entity_id.split('.').next().unwrap_or("").to_string();

    let friendly_name = value
        .get("attributes")
        .and_then(|a| a.get("friendly_name"))
        .and_then(|v| v.as_str())
        .unwrap_or(&entity_id)
        .to_string();

    let is_on = matches!(
        state.as_str(),
        "on" | "home" | "open" | "playing" | "active"
    );

    let attributes = value
        .get("attributes")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let attrs_monty = json_to_monty_obj(&attributes);

    let labels_monty = match value.get("labels") {
        Some(serde_json::Value::Array(arr)) => MontyObject::List(
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| MontyObject::String(s.to_string())))
                .collect(),
        ),
        _ => MontyObject::List(vec![]),
    };

    MontyObject::Dataclass {
        name: "EntityState".to_string(),
        type_id: 0,
        field_names: vec![
            "entity_id".into(),
            "state".into(),
            "domain".into(),
            "name".into(),
            "last_changed".into(),
            "last_updated".into(),
            "is_on".into(),
            "attributes".into(),
            "labels".into(),
        ],
        attrs: vec![
            (
                MontyObject::String("entity_id".into()),
                MontyObject::String(entity_id),
            ),
            (
                MontyObject::String("state".into()),
                MontyObject::String(state),
            ),
            (
                MontyObject::String("domain".into()),
                MontyObject::String(domain),
            ),
            (
                MontyObject::String("name".into()),
                MontyObject::String(friendly_name),
            ),
            (
                MontyObject::String("last_changed".into()),
                MontyObject::String(last_changed),
            ),
            (
                MontyObject::String("last_updated".into()),
                MontyObject::String(last_updated),
            ),
            (
                MontyObject::String("is_on".into()),
                MontyObject::Bool(is_on),
            ),
            (
                MontyObject::String("attributes".into()),
                attrs_monty,
            ),
            (
                MontyObject::String("labels".into()),
                labels_monty,
            ),
        ]
        .into(),
        frozen: false,
    }
}

/// Convert a JSON array of HA state objects to a list of EntityState.
pub fn json_to_entity_state_list(value: &serde_json::Value) -> MontyObject {
    match value {
        serde_json::Value::Array(arr) => {
            MontyObject::List(arr.iter().map(json_to_entity_state).collect())
        }
        _ => json_to_entity_state(value),
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monty_to_json_primitives() {
        assert_eq!(monty_obj_to_json(&MontyObject::None), serde_json::Value::Null);
        assert_eq!(monty_obj_to_json(&MontyObject::Bool(true)), serde_json::json!(true));
        assert_eq!(monty_obj_to_json(&MontyObject::Int(42)), serde_json::json!(42));
        assert_eq!(
            monty_obj_to_json(&MontyObject::String("hello".into())),
            serde_json::json!("hello")
        );
    }

    #[test]
    fn test_monty_to_json_list() {
        let list = MontyObject::List(vec![MontyObject::Int(1), MontyObject::Int(2)]);
        assert_eq!(monty_obj_to_json(&list), serde_json::json!([1, 2]));
    }

    #[test]
    fn test_monty_to_json_dict() {
        let dict = MontyObject::Dict(
            vec![
                (MontyObject::String("a".into()), MontyObject::Int(1)),
                (MontyObject::String("b".into()), MontyObject::Int(2)),
            ]
            .into(),
        );
        let json = monty_obj_to_json(&dict);
        assert_eq!(json["a"], 1);
        assert_eq!(json["b"], 2);
    }

    #[test]
    fn test_json_to_monty_primitives() {
        assert_eq!(json_to_monty_obj(&serde_json::Value::Null), MontyObject::None);
        assert_eq!(json_to_monty_obj(&serde_json::json!(true)), MontyObject::Bool(true));
        assert_eq!(json_to_monty_obj(&serde_json::json!(42)), MontyObject::Int(42));
        assert_eq!(
            json_to_monty_obj(&serde_json::json!("hello")),
            MontyObject::String("hello".into())
        );
    }

    #[test]
    fn test_json_to_entity_state() {
        let json = serde_json::json!({
            "entity_id": "sensor.temp",
            "state": "21.5",
            "last_changed": "2026-01-01T00:00:00Z",
            "last_updated": "2026-01-01T00:00:00Z",
            "attributes": {
                "friendly_name": "Temperature",
                "unit_of_measurement": "°C",
            }
        });
        let result = json_to_entity_state(&json);
        if let MontyObject::Dataclass { name, .. } = &result {
            assert_eq!(name, "EntityState");
            let as_json = monty_obj_to_json(&result);
            assert_eq!(as_json["entity_id"], "sensor.temp");
            assert_eq!(as_json["state"], "21.5");
            assert_eq!(as_json["name"], "Temperature");
        } else {
            panic!("Expected Dataclass");
        }
    }

    #[test]
    fn test_json_to_entity_state_list() {
        let json = serde_json::json!([
            { "entity_id": "sensor.a", "state": "1", "attributes": {} },
            { "entity_id": "sensor.b", "state": "2", "attributes": {} }
        ]);
        let result = json_to_entity_state_list(&json);
        if let MontyObject::List(items) = &result {
            assert_eq!(items.len(), 2);
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_roundtrip_dict() {
        let dict = MontyObject::Dict(
            vec![
                (MontyObject::String("x".into()), MontyObject::Int(10)),
                (MontyObject::String("y".into()), MontyObject::Bool(false)),
            ]
            .into(),
        );
        let json = monty_obj_to_json(&dict);
        let back = json_to_monty_obj(&json);
        // Dict → JSON → Dict should preserve structure.
        if let MontyObject::Dict(pairs) = back {
            assert_eq!(pairs.into_iter().count(), 2);
        } else {
            panic!("Expected Dict");
        }
    }
}
