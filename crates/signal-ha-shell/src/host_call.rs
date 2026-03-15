//! Host call mapping — Python function call → method + JSON params.
//!
//! When Monty suspends at an external function call, this module maps
//! the `(function_name, args)` pair to a `(method, params)` pair that
//! the host can fulfil via HA WebSocket, REST, or mock.
//!
//! Returns `None` for functions handled locally (show, ago, charts).

use monty::MontyObject;

use crate::convert::monty_obj_to_json;

/// Map an external function call to a host call method + params.
///
/// Returns `None` for functions that are handled locally by the engine
/// (show, ago, plot_*) rather than sent to the host.
pub fn map_ext_call_to_host_call(
    function_name: &str,
    args: &[MontyObject],
) -> Option<(&'static str, serde_json::Value)> {
    match function_name {
        // ── State ──────────────────────────────────────────────
        "state" | "get_state" => {
            let entity_id = extract_string(args, 0)?;
            Some(("get_state", serde_json::json!({ "entity_id": entity_id })))
        }
        "states" | "get_states" => {
            let domain = extract_string_opt(args, 0);
            let params = match domain {
                Some(d) => serde_json::json!({ "domain": d }),
                None => serde_json::json!({}),
            };
            Some(("get_states", params))
        }

        // ── History ────────────────────────────────────────────
        "history" | "get_history" => {
            let entity_id = extract_string(args, 0)?;
            let hours_param = match args.get(1) {
                Some(MontyObject::String(s)) => {
                    serde_json::json!({ "entity_id": entity_id, "start_time": s })
                }
                Some(MontyObject::Int(n)) => {
                    serde_json::json!({ "entity_id": entity_id, "hours": *n as f64 })
                }
                Some(MontyObject::Float(f)) => {
                    serde_json::json!({ "entity_id": entity_id, "hours": f })
                }
                _ => {
                    serde_json::json!({ "entity_id": entity_id, "hours": 6.0 })
                }
            };
            Some(("get_history", hours_param))
        }

        // ── Statistics ─────────────────────────────────────────
        "statistics" | "get_statistics" => {
            let entity_id = extract_string(args, 0)?;
            let period = extract_string_opt(args, 1).unwrap_or("hour".to_string());
            let hours = extract_number(args, 2).unwrap_or(24.0);
            Some((
                "get_statistics",
                serde_json::json!({ "entity_id": entity_id, "period": period, "hours": hours }),
            ))
        }

        // ── Calendar events ────────────────────────────────────
        "events" | "get_events" => {
            let entity_id = extract_string(args, 0)?;
            let hours = extract_number(args, 1).unwrap_or(24.0 * 14.0);
            Some((
                "get_events",
                serde_json::json!({ "entity_id": entity_id, "hours": hours }),
            ))
        }

        // ── Services ───────────────────────────────────────────
        "call_service" => {
            let domain = extract_string(args, 0)?;
            let service = extract_string(args, 1)?;
            let data = args
                .get(2)
                .map(monty_obj_to_json)
                .unwrap_or(serde_json::json!({}));
            Some((
                "call_service",
                serde_json::json!({
                    "domain": domain,
                    "service": service,
                    "service_data": data,
                }),
            ))
        }
        "get_services" => {
            let domain = extract_string(args, 0)?;
            Some(("get_services", serde_json::json!({ "domain": domain })))
        }

        // ── Areas / rooms ──────────────────────────────────────
        "get_areas" | "rooms" => Some(("get_areas", serde_json::json!({}))),
        "get_area_entities" | "room" => {
            let area_id = extract_string(args, 0)?;
            Some((
                "get_area_entities",
                serde_json::json!({ "area_id": area_id }),
            ))
        }

        // ── Time ───────────────────────────────────────────────
        "get_datetime" | "now" => Some(("get_datetime", serde_json::json!({}))),

        // ── Logbook ────────────────────────────────────────────
        "get_logbook" | "logbook" => {
            let entity_id = extract_string(args, 0)?;
            let hours = extract_number(args, 1).unwrap_or(24.0);
            Some(("get_logbook", serde_json::json!({ "entity_id": entity_id, "hours": hours })))
        }

        // ── Traces ─────────────────────────────────────────────
        "get_trace" => {
            let automation_id = extract_string(args, 0)?;
            let run_id = extract_string_opt(args, 1);
            let mut params = serde_json::json!({ "automation_id": automation_id });
            if let Some(rid) = run_id {
                params["run_id"] = serde_json::json!(rid);
            }
            Some(("get_trace", params))
        }
        "list_traces" => {
            let domain = extract_string_opt(args, 0).unwrap_or("automation".to_string());
            Some(("list_traces", serde_json::json!({ "domain": domain })))
        }

        // ── Semantic layer ─────────────────────────────────────
        "annotate" => {
            let entity_id = extract_string(args, 0)?;
            let notes = extract_string_opt(args, 1);
            let tags = extract_string_list(args, 2);
            let mut params = serde_json::json!({ "entity_id": entity_id });
            if let Some(n) = notes {
                params["notes"] = serde_json::json!(n);
            }
            if let Some(t) = tags {
                params["tags"] = serde_json::json!(t);
            }
            Some(("set_annotation", params))
        }
        "annotations" => {
            let entity_id = extract_string_opt(args, 0);
            let params = match entity_id {
                Some(eid) => serde_json::json!({ "entity_id": eid }),
                None => serde_json::json!({}),
            };
            Some(("get_annotations", params))
        }
        "note" => {
            let text = extract_string(args, 0)?;
            Some((
                "set_global_note",
                serde_json::json!({ "text": text }),
            ))
        }
        "notes" => Some(("get_global_notes", serde_json::json!({}))),
        "tags" => {
            match args.len() {
                0 => Some(("get_tags", serde_json::json!({}))),
                1 => {
                    let arg = extract_string(args, 0)?;
                    if arg.contains('.') {
                        // entity_id → get annotations for it
                        Some(("get_annotations", serde_json::json!({ "entity_id": arg })))
                    } else {
                        // tag name → search by tag
                        Some(("get_tags", serde_json::json!({ "tag": arg })))
                    }
                }
                _ => {
                    // tags(entity_id, [tag_list]) → set tags
                    let entity_id = extract_string(args, 0)?;
                    let tag_list = extract_string_list(args, 1)?;
                    Some((
                        "set_tags",
                        serde_json::json!({ "entity_id": entity_id, "tags": tag_list }),
                    ))
                }
            }
        }
        "del_annotation" => {
            let entity_id = extract_string(args, 0)?;
            Some((
                "delete_annotation",
                serde_json::json!({ "entity_id": entity_id }),
            ))
        }

        // ── Board (findings API) ──────────────────────────────
        "board_get_posts" => {
            // board_get_posts() — returns open posts for this agent
            // Agent name is injected by the engine, not passed from Python.
            Some(("board_get_posts", serde_json::json!({})))
        }
        "board_create_post" => {
            // board_create_post("body text")
            let body = extract_string(args, 0)?;
            Some(("board_create_post", serde_json::json!({ "body": body })))
        }
        "board_reply" => {
            // board_reply(post_id, "reply body")
            let post_id = extract_number(args, 0)? as i64;
            let body = extract_string(args, 1)?;
            Some(("board_reply", serde_json::json!({ "post_id": post_id, "body": body })))
        }
        "board_close_post" => {
            // board_close_post(post_id)
            let post_id = extract_number(args, 0)? as i64;
            Some(("board_close_post", serde_json::json!({ "post_id": post_id })))
        }

        // ── Dashboards ─────────────────────────────────────────
        "list_dashboards" => {
            Some(("list_dashboards", serde_json::json!({})))
        }
        "get_dashboard" => {
            // get_dashboard("signal-porch-lights") → full config for a dashboard
            let url_path = extract_string(args, 0)?;
            Some(("get_dashboard", serde_json::json!({ "url_path": url_path })))
        }

        // ── House agent (cross-agent functions) ────────────────
        "read_agent_memory" => {
            // read_agent_memory("porch-agent")
            let agent_name = extract_string(args, 0)?;
            Some(("read_agent_memory", serde_json::json!({ "agent_name": agent_name })))
        }
        "read_transcript" => {
            // read_transcript("porch-agent") or read_transcript("porch-agent", 0)
            let agent_name = extract_string(args, 0)?;
            let nth = extract_number(args, 1).unwrap_or(0.0) as i64;
            Some(("read_transcript", serde_json::json!({ "agent_name": agent_name, "nth_latest": nth })))
        }
        "read_status_page" => {
            // read_status_page("http://127.0.0.1:9100")
            let url = extract_string(args, 0)?;
            Some(("read_status_page", serde_json::json!({ "url": url })))
        }
        "board_get_all_posts" => {
            // board_get_all_posts() or board_get_all_posts(True/False)
            let active_only = match args.first() {
                Some(MontyObject::Bool(b)) => *b,
                _ => true,
            };
            Some(("board_get_all_posts", serde_json::json!({ "active_only": active_only })))
        }

        // show, ago, plot_* are handled locally by the engine — not host calls.
        _ => None,
    }
}

/// Parse an `ago()` argument like "6h", "30m", "2d" and return hours as i64.
///
/// Supported suffixes: m (minutes), h (hours), d (days), w (weeks).
/// Returns the value in hours (rounded). Falls back to 6 for unparseable input.
pub fn parse_ago(args: &[MontyObject]) -> MontyObject {
    let input = match args.first() {
        Some(MontyObject::String(s)) => s.clone(),
        Some(MontyObject::Int(n)) => return MontyObject::Int(*n),
        Some(MontyObject::Float(f)) => return MontyObject::Int(*f as i64),
        _ => return MontyObject::Int(6),
    };

    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        return MontyObject::Int(6);
    }

    let (num_str, suffix) =
        if trimmed.chars().last().map(|c| c.is_alphabetic()).unwrap_or(false) {
            let split = trimmed.len() - 1;
            (&trimmed[..split], &trimmed[split..])
        } else {
            (trimmed.as_str(), "h")
        };

    let num: f64 = match num_str.parse() {
        Ok(n) => n,
        Err(_) => return MontyObject::Int(6),
    };

    let hours = match suffix {
        "m" => (num / 60.0).max(1.0),
        "h" => num,
        "d" => num * 24.0,
        "w" => num * 168.0,
        _ => num,
    };

    MontyObject::Int(hours.round() as i64)
}

// ── Argument extraction helpers ────────────────────────────────

fn extract_string(args: &[MontyObject], idx: usize) -> Option<String> {
    match args.get(idx) {
        Some(MontyObject::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn extract_string_opt(args: &[MontyObject], idx: usize) -> Option<String> {
    extract_string(args, idx)
}

fn extract_number(args: &[MontyObject], idx: usize) -> Option<f64> {
    match args.get(idx) {
        Some(MontyObject::Int(n)) => Some(*n as f64),
        Some(MontyObject::Float(f)) => Some(*f),
        _ => None,
    }
}

fn extract_string_list(args: &[MontyObject], idx: usize) -> Option<Vec<String>> {
    match args.get(idx) {
        Some(MontyObject::List(items)) => {
            let strs: Vec<String> = items
                .iter()
                .filter_map(|i| {
                    if let MontyObject::String(s) = i {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();
            Some(strs)
        }
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_state() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, params) = map_ext_call_to_host_call("state", &args).unwrap();
        assert_eq!(method, "get_state");
        assert_eq!(params["entity_id"], "sensor.temp");
    }

    #[test]
    fn test_get_state_long_name() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, _) = map_ext_call_to_host_call("get_state", &args).unwrap();
        assert_eq!(method, "get_state");
    }

    #[test]
    fn test_get_states_no_domain() {
        let (method, _) = map_ext_call_to_host_call("states", &[]).unwrap();
        assert_eq!(method, "get_states");
    }

    #[test]
    fn test_get_states_with_domain() {
        let args = vec![MontyObject::String("light".into())];
        let (method, params) = map_ext_call_to_host_call("get_states", &args).unwrap();
        assert_eq!(method, "get_states");
        assert_eq!(params["domain"], "light");
    }

    #[test]
    fn test_history_default_hours() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, params) = map_ext_call_to_host_call("history", &args).unwrap();
        assert_eq!(method, "get_history");
        assert_eq!(params["hours"], 6.0);
    }

    #[test]
    fn test_history_with_hours() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::Int(12),
        ];
        let (method, params) = map_ext_call_to_host_call("get_history", &args).unwrap();
        assert_eq!(method, "get_history");
        assert_eq!(params["hours"], 12.0);
    }

    #[test]
    fn test_statistics() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, params) = map_ext_call_to_host_call("statistics", &args).unwrap();
        assert_eq!(method, "get_statistics");
        assert_eq!(params["period"], "hour");
        assert_eq!(params["hours"], 24.0);
    }

    #[test]
    fn test_statistics_with_period() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::String("day".into()),
        ];
        let (method, params) = map_ext_call_to_host_call("get_statistics", &args).unwrap();
        assert_eq!(method, "get_statistics");
        assert_eq!(params["period"], "day");
        assert_eq!(params["hours"], 24.0);
    }

    #[test]
    fn test_statistics_with_period_and_hours() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::String("5minute".into()),
            MontyObject::Int(6),
        ];
        let (method, params) = map_ext_call_to_host_call("statistics", &args).unwrap();
        assert_eq!(method, "get_statistics");
        assert_eq!(params["period"], "5minute");
        assert_eq!(params["hours"], 6.0);
    }

    #[test]
    fn test_call_service() {
        let args = vec![
            MontyObject::String("light".into()),
            MontyObject::String("turn_on".into()),
            MontyObject::Dict(
                vec![(
                    MontyObject::String("entity_id".into()),
                    MontyObject::String("light.kitchen".into()),
                )]
                .into(),
            ),
        ];
        let (method, params) = map_ext_call_to_host_call("call_service", &args).unwrap();
        assert_eq!(method, "call_service");
        assert_eq!(params["domain"], "light");
        assert_eq!(params["service"], "turn_on");
    }

    #[test]
    fn test_get_areas() {
        let (method, _) = map_ext_call_to_host_call("get_areas", &[]).unwrap();
        assert_eq!(method, "get_areas");
    }

    #[test]
    fn test_rooms_alias() {
        let (method, _) = map_ext_call_to_host_call("rooms", &[]).unwrap();
        assert_eq!(method, "get_areas");
    }

    #[test]
    fn test_room_alias() {
        let args = vec![MontyObject::String("garage".into())];
        let (method, params) = map_ext_call_to_host_call("room", &args).unwrap();
        assert_eq!(method, "get_area_entities");
        assert_eq!(params["area_id"], "garage");
    }

    #[test]
    fn test_get_area_entities() {
        let args = vec![MontyObject::String("kitchen".into())];
        let (method, params) = map_ext_call_to_host_call("get_area_entities", &args).unwrap();
        assert_eq!(method, "get_area_entities");
        assert_eq!(params["area_id"], "kitchen");
    }

    #[test]
    fn test_logbook_default_hours() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, params) = map_ext_call_to_host_call("logbook", &args).unwrap();
        assert_eq!(method, "get_logbook");
        assert_eq!(params["hours"], 24.0);
    }

    #[test]
    fn test_get_logbook_with_hours() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::Int(6),
        ];
        let (method, params) = map_ext_call_to_host_call("get_logbook", &args).unwrap();
        assert_eq!(method, "get_logbook");
        assert_eq!(params["hours"], 6.0);
    }

    #[test]
    fn test_annotate() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::String("Living room".into()),
        ];
        let (method, params) = map_ext_call_to_host_call("annotate", &args).unwrap();
        assert_eq!(method, "set_annotation");
        assert_eq!(params["entity_id"], "sensor.temp");
        assert_eq!(params["notes"], "Living room");
    }

    #[test]
    fn test_annotate_with_tags() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::String("note".into()),
            MontyObject::List(vec![
                MontyObject::String("climate".into()),
                MontyObject::String("kitchen".into()),
            ]),
        ];
        let (method, params) = map_ext_call_to_host_call("annotate", &args).unwrap();
        assert_eq!(method, "set_annotation");
        assert_eq!(params["tags"], serde_json::json!(["climate", "kitchen"]));
    }

    #[test]
    fn test_annotations_all() {
        let (method, _) = map_ext_call_to_host_call("annotations", &[]).unwrap();
        assert_eq!(method, "get_annotations");
    }

    #[test]
    fn test_note() {
        let args = vec![MontyObject::String("Three floors".into())];
        let (method, params) = map_ext_call_to_host_call("note", &args).unwrap();
        assert_eq!(method, "set_global_note");
        assert_eq!(params["text"], "Three floors");
    }

    #[test]
    fn test_notes() {
        let (method, _) = map_ext_call_to_host_call("notes", &[]).unwrap();
        assert_eq!(method, "get_global_notes");
    }

    #[test]
    fn test_tags_all() {
        let (method, _) = map_ext_call_to_host_call("tags", &[]).unwrap();
        assert_eq!(method, "get_tags");
    }

    #[test]
    fn test_tags_by_name() {
        let args = vec![MontyObject::String("kitchen".into())];
        let (method, params) = map_ext_call_to_host_call("tags", &args).unwrap();
        assert_eq!(method, "get_tags");
        assert_eq!(params["tag"], "kitchen");
    }

    #[test]
    fn test_tags_by_entity_id() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, params) = map_ext_call_to_host_call("tags", &args).unwrap();
        assert_eq!(method, "get_annotations");
        assert_eq!(params["entity_id"], "sensor.temp");
    }

    #[test]
    fn test_tags_set() {
        let args = vec![
            MontyObject::String("sensor.temp".into()),
            MontyObject::List(vec![MontyObject::String("kitchen".into())]),
        ];
        let (method, params) = map_ext_call_to_host_call("tags", &args).unwrap();
        assert_eq!(method, "set_tags");
        assert_eq!(params["tags"], serde_json::json!(["kitchen"]));
    }

    #[test]
    fn test_del_annotation() {
        let args = vec![MontyObject::String("sensor.temp".into())];
        let (method, _) = map_ext_call_to_host_call("del_annotation", &args).unwrap();
        assert_eq!(method, "delete_annotation");
    }

    #[test]
    fn test_show_returns_none() {
        let args = vec![MontyObject::Int(42)];
        assert!(map_ext_call_to_host_call("show", &args).is_none());
    }

    #[test]
    fn test_ago_returns_none() {
        let args = vec![MontyObject::String("6h".into())];
        assert!(map_ext_call_to_host_call("ago", &args).is_none());
    }

    #[test]
    fn test_unknown_returns_none() {
        assert!(map_ext_call_to_host_call("not_a_function", &[]).is_none());
    }

    // ── parse_ago tests ────────────────────────────────────────

    #[test]
    fn test_parse_ago_hours() {
        let args = vec![MontyObject::String("6h".into())];
        assert_eq!(parse_ago(&args), MontyObject::Int(6));
    }

    #[test]
    fn test_parse_ago_minutes() {
        let args = vec![MontyObject::String("30m".into())];
        assert_eq!(parse_ago(&args), MontyObject::Int(1)); // 30m → 1h (min 1)
    }

    #[test]
    fn test_parse_ago_days() {
        let args = vec![MontyObject::String("2d".into())];
        assert_eq!(parse_ago(&args), MontyObject::Int(48));
    }

    #[test]
    fn test_parse_ago_weeks() {
        let args = vec![MontyObject::String("1w".into())];
        assert_eq!(parse_ago(&args), MontyObject::Int(168));
    }

    #[test]
    fn test_parse_ago_bare_number() {
        let args = vec![MontyObject::String("12".into())];
        assert_eq!(parse_ago(&args), MontyObject::Int(12));
    }

    #[test]
    fn test_parse_ago_int_passthrough() {
        let args = vec![MontyObject::Int(24)];
        assert_eq!(parse_ago(&args), MontyObject::Int(24));
    }
}
