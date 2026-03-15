//! HA-specific host call fulfillment.
//!
//! When the Python runtime suspends at an external function call
//! (e.g. `state("sensor.temp")`), this module fulfills it by calling
//! the appropriate HA API (WebSocket or REST) and returning the result
//! as JSON.
//!
//! This is the native equivalent of Signal Deck's `host-functions.ts`.

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::{json, Value};
use signal_ha::{HaClient, HaError};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Everything needed to fulfill HA host calls.
pub struct HaHost {
    /// The shared HA WebSocket client — behind RwLock so we can
    /// replace it after a reconnect.
    client: RwLock<HaClient>,
    /// HTTP client for REST-only APIs (history).
    pub http: reqwest::Client,
    /// HA base URL (e.g. "http://homeassistant.local:8123").
    pub ha_base_url: String,
    /// HA WebSocket URL (e.g. "ws://homeassistant.local:8123/api/websocket").
    pub ha_ws_url: String,
    /// Long-lived HA access token.
    pub ha_token: String,
    /// Status page URL for this automation (optional).
    pub status_page_url: Option<String>,
    /// Board REST API base URL (e.g. "http://127.0.0.1:9200").
    /// If None, board functions return an error.
    pub board_url: Option<String>,
    /// Agent name — auto-injected into board posts/replies.
    pub agent_name: String,
    /// Directory for agent memory files (house-agent reads other agents').
    pub memory_dir: Option<String>,
    /// Directory for session transcripts (house-agent reads other agents').
    pub transcript_dir: Option<String>,
}

impl HaHost {
    /// Construct a new HaHost.
    pub fn new(
        client: HaClient,
        ha_base_url: String,
        ha_ws_url: String,
        ha_token: String,
        status_page_url: Option<String>,
    ) -> Self {
        Self {
            client: RwLock::new(client),
            http: reqwest::Client::new(),
            ha_base_url,
            ha_ws_url,
            ha_token,
            status_page_url,
            board_url: None,
            agent_name: String::new(),
            memory_dir: None,
            transcript_dir: None,
        }
    }

    /// Configure the board (findings API) connection.
    pub fn with_board(mut self, url: String, agent_name: String) -> Self {
        self.board_url = Some(url);
        self.agent_name = agent_name;
        self
    }

    /// Configure cross-agent access directories (for house-agent).
    pub fn with_cross_agent_access(mut self, memory_dir: String, transcript_dir: String) -> Self {
        self.memory_dir = Some(memory_dir);
        self.transcript_dir = Some(transcript_dir);
        self
    }

    /// Get a clone of the current HaClient (for passing to Conversation, etc.)
    pub async fn client(&self) -> HaClient {
        self.client.read().await.clone()
    }

    /// Reconnect the WebSocket client.
    ///
    /// Called automatically when a host call fails due to a dead connection.
    /// Note: this does NOT re-establish subscriptions — only the main
    /// automation loop handles those.  The agent only uses request/response
    /// patterns (send_raw), which work fine on a fresh connection.
    async fn reconnect(&self) -> Result<()> {
        info!("Reconnecting to Home Assistant WebSocket");
        let new_client = HaClient::connect(&self.ha_ws_url, &self.ha_token)
            .await
            .map_err(|e| anyhow!("Reconnect failed: {e}"))?;
        *self.client.write().await = new_client;
        info!("Reconnected successfully");
        Ok(())
    }

    /// Fulfill a host call by method name and params.
    ///
    /// Returns JSON that will be converted to a MontyObject and
    /// resumed into the Python execution.
    ///
    /// If the WebSocket connection is dead, attempts to reconnect once.
    /// If the response was too large, returns an actionable error
    /// message (not a panic).
    pub async fn fulfill(&self, method: &str, params: &Value) -> Result<Value> {
        debug!(method, "Fulfilling host call");
        match self.fulfill_inner(method, params).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // Try to downcast to HaError for precise matching
                if let Some(ha_err) = e.downcast_ref::<HaError>() {
                    match ha_err {
                        HaError::ResponseTooLarge(detail) => {
                            return Err(anyhow!(
                                "RESPONSE TOO LARGE: The API call '{method}' returned more data \
                                 than the 16 MB WebSocket limit allows. The connection has been \
                                 reset. Try a more targeted query — use specific entity_id \
                                 parameters, add time filters (hours=1), or query fewer entities. \
                                 Detail: {detail}"
                            ));
                        }
                        HaError::ConnectionClosed | HaError::Timeout => {
                            warn!(method, error = %e, "Connection lost, attempting reconnect");
                            if let Err(re) = self.reconnect().await {
                                return Err(anyhow!(
                                    "Host call '{method}' failed and reconnect also failed: {re}. \
                                     Original: {e}"
                                ));
                            }
                            // Retry once after reconnect
                            return self.fulfill_inner(method, params).await;
                        }
                        _ => {}
                    }
                }
                // Fallback: string-match for errors wrapped by the `?` operator
                let err_str = format!("{e}");
                if err_str.contains("Connection closed")
                    || err_str.contains("channel closed")
                    || err_str.contains("WebSocket error")
                {
                    warn!(method, error = %err_str, "Connection lost (string match), attempting reconnect");
                    if let Err(re) = self.reconnect().await {
                        return Err(anyhow!(
                            "Host call '{method}' failed and reconnect also failed: {re}. \
                             Original: {err_str}"
                        ));
                    }
                    self.fulfill_inner(method, params).await
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Send a raw WS message through the (possibly reconnected) client.
    async fn send_raw(&self, msg: Value) -> Result<Value> {
        Ok(self.client.read().await.send_raw(msg).await?)
    }

    /// Call an HA service through the (possibly reconnected) client.
    async fn ws_call_service(&self, domain: &str, service: &str, data: Value) -> Result<()> {
        Ok(self.client.read().await.call_service(domain, service, data).await?)
    }

    /// Inner fulfillment logic — called by fulfill() with retry/reconnect wrapping.
    async fn fulfill_inner(&self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "get_state" => self.get_state(params).await,
            "get_states" => self.get_states(params).await,
            "get_history" => self.get_history(params).await,
            "get_statistics" => self.get_statistics(params).await,
            "get_logbook" => self.get_logbook(params).await,
            "get_events" => self.get_events(params).await,
            "get_services" => self.get_services(params).await,
            "get_areas" => self.get_areas(params).await,
            "get_area_entities" => self.get_area_entities(params).await,
            "get_datetime" => self.get_datetime(params).await,
            "call_service" => self.call_service(params).await,
            "get_trace" => self.get_trace(params).await,
            "list_traces" => self.list_traces(params).await,
            "get_status_page" => self.get_status_page(params).await,
            // Annotation / semantic layer — pass through to HA
            "annotate" | "annotations" | "note" | "notes" | "tags" | "del_annotation" => {
                // These are handled by Signal Deck's semantic layer.
                // For the headless agent, return a stub.
                Ok(json!({"error": format!("Semantic layer method '{method}' not yet implemented in headless agent")}))
            }
            // ── Board (findings API) ────────────────────────────
            "board_get_posts" => self.board_get_posts(params).await,
            "board_get_post" => self.board_get_post(params).await,
            "board_create_post" => self.board_create_post(params).await,
            "board_reply" => self.board_reply(params).await,
            "board_close_post" => self.board_close_post(params).await,
            // ── House agent (cross-agent access) ────────────────
            "board_get_all_posts" => self.board_get_all_posts(params).await,
            "read_agent_memory" => self.read_agent_memory(params).await,
            "read_transcript" => self.read_transcript(params).await,
            "read_status_page" => self.read_status_page(params).await,
            _ => Err(anyhow!("Unknown host call method: {method}")),
        }
    }

    // ── Individual method implementations ─────────────────────

    async fn get_state(&self, params: &Value) -> Result<Value> {
        let entity_id = params["entity_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_state: missing entity_id"))?;

        // Use REST API for single-entity lookup — avoids the 54 MB
        // get_states WebSocket response that exceeds the 16 MB frame limit.
        let url = format!("{}/api/states/{entity_id}", self.ha_base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.ha_token)
            .send()
            .await?;

        if resp.status().as_u16() == 404 {
            return Err(anyhow!("Entity not found: {entity_id}"));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("get_state: HTTP {}", resp.status()));
        }

        let state: Value = resp.json().await?;
        Ok(state)
    }

    async fn get_states(&self, params: &Value) -> Result<Value> {
        let resp = self
            .send_raw(json!({"type": "get_states"}))
            .await?;

        let states = resp["result"]
            .as_array()
            .ok_or_else(|| anyhow!("get_states: no states array"))?;

        // Optional domain filter
        if let Some(domain) = params.get("domain").and_then(|d| d.as_str()) {
            let filtered: Vec<&Value> = states
                .iter()
                .filter(|s| {
                    s["entity_id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with(&format!("{domain}.")))
                })
                .collect();
            Ok(Value::Array(filtered.into_iter().cloned().collect()))
        } else {
            Ok(Value::Array(states.clone()))
        }
    }

    async fn get_history(&self, params: &Value) -> Result<Value> {
        let entity_id = params["entity_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_history: missing entity_id"))?;

        // Determine start time
        let start_str = if let Some(start) = params.get("start_time").and_then(|s| s.as_str()) {
            start.to_string()
        } else {
            let hours = params
                .get("hours")
                .and_then(|h| h.as_f64())
                .unwrap_or(24.0);
            let start = Utc::now() - chrono::Duration::seconds((hours * 3600.0) as i64);
            start.to_rfc3339()
        };

        let url = format!(
            "{}/api/history/period/{start_str}?filter_entity_id={entity_id}&minimal_response&no_attributes",
            self.ha_base_url,
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.ha_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!("History API: HTTP {}", resp.status()));
        }

        let data: Value = resp.json().await?;

        // HA returns [[{state, last_changed}, ...]], flatten the outer array.
        // With minimal_response, entries after the first use short keys:
        //   "s" → state, "lu" → last_updated (float epoch).
        // We normalise everything to full key names so the agent gets
        // consistent dicts with "state" and "last_changed".
        let raw_changes = data
            .as_array()
            .and_then(|a| a.first())
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default();

        let changes: Vec<Value> = raw_changes
            .into_iter()
            .map(|entry| {
                if entry.get("state").is_some() {
                    // Full-format entry (first entry or non-minimal)
                    entry
                } else if let Some(s) = entry.get("s") {
                    // Minimal-format entry: expand short keys
                    let state = s.clone();
                    let last_changed = entry
                        .get("lu")
                        .and_then(|lu| lu.as_f64())
                        .map(|epoch| {
                            let dt = chrono::DateTime::from_timestamp(
                                epoch as i64,
                                ((epoch.fract()) * 1_000_000_000.0) as u32,
                            );
                            dt.map(|d| Value::String(d.to_rfc3339()))
                                .unwrap_or(Value::Null)
                        })
                        .unwrap_or(Value::Null);
                    json!({
                        "state": state,
                        "last_changed": last_changed,
                    })
                } else {
                    entry
                }
            })
            .collect();

        // Cap at 200 entries to avoid blowing LLM context.
        // The Python runtime lets the LLM filter/aggregate in code,
        // so we can be more generous than the old tool (100 entries).
        const MAX_ENTRIES: usize = 200;
        let total = changes.len();
        let entries = if total <= MAX_ENTRIES {
            changes
        } else {
            let first = &changes[..20];
            let last = &changes[total - (MAX_ENTRIES - 20)..];
            let mut v = first.to_vec();
            v.push(json!({"_note": format!("... {} entries omitted ...", total - MAX_ENTRIES)}));
            v.extend_from_slice(last);
            v
        };

        Ok(json!({
            "entity_id": entity_id,
            "total_changes": total,
            "history": entries,
        }))
    }

    async fn get_statistics(&self, params: &Value) -> Result<Value> {
        let entity_id = params["entity_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_statistics: missing entity_id"))?;

        let period = params
            .get("period")
            .and_then(|p| p.as_str())
            .unwrap_or("hour");

        let hours = params
            .get("hours")
            .and_then(|h| h.as_f64())
            .unwrap_or(24.0);
        let start = Utc::now() - chrono::Duration::seconds((hours * 3600.0) as i64);

        let resp = self
            .send_raw(json!({
                "type": "recorder/statistics_during_period",
                "start_time": start.to_rfc3339(),
                "statistic_ids": [entity_id],
                "period": period,
            }))
            .await?;

        // HA returns {"entity_id": [{start, end, mean, min, max, ...}, ...]}.
        // Flatten into a simple dict with aggregated min/max/mean plus the
        // raw entries array, so the agent can just check `stats['mean']`.
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        if let Some(entries) = result.get(entity_id).and_then(|v| v.as_array()) {
            if entries.is_empty() {
                return Ok(json!({ "entries": [], "count": 0 }));
            }
            let mut min_val = f64::INFINITY;
            let mut max_val = f64::NEG_INFINITY;
            let mut sum = 0.0_f64;
            let mut n = 0_u64;
            for entry in entries {
                if let Some(mean) = entry.get("mean").and_then(|v| v.as_f64()) {
                    sum += mean;
                    n += 1;
                }
                if let Some(mi) = entry.get("min").and_then(|v| v.as_f64()) {
                    min_val = min_val.min(mi);
                }
                if let Some(ma) = entry.get("max").and_then(|v| v.as_f64()) {
                    max_val = max_val.max(ma);
                }
            }
            let mean = if n > 0 { sum / n as f64 } else { 0.0 };
            Ok(json!({
                "mean": mean,
                "min": if min_val.is_finite() { json!(min_val) } else { Value::Null },
                "max": if max_val.is_finite() { json!(max_val) } else { Value::Null },
                "count": n,
                "entries": entries,
            }))
        } else {
            // No data for this entity — return empty result
            Ok(json!({ "entries": [], "count": 0 }))
        }
    }

    async fn get_logbook(&self, params: &Value) -> Result<Value> {
        let entity_id = params["entity_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_logbook requires an 'entity_id' parameter"))?;
        let hours = params
            .get("hours")
            .and_then(|h| h.as_f64())
            .unwrap_or(24.0);
        let start = Utc::now() - chrono::Duration::seconds((hours * 3600.0) as i64);

        // Use WS logbook/get_events for better performance
        let resp = self
            .send_raw(json!({
                "type": "logbook/get_events",
                "start_time": start.to_rfc3339(),
                "end_time": Utc::now().to_rfc3339(),
                "entity_ids": [entity_id],
            }))
            .await?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn get_events(&self, params: &Value) -> Result<Value> {
        let entity_id = params["entity_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_events: missing entity_id"))?;

        let resp = self
            .send_raw(json!({
                "type": "calendars/list_events",
                "entity_id": entity_id,
                "start_time": Utc::now().to_rfc3339(),
                "end_time": (Utc::now() + chrono::Duration::days(7)).to_rfc3339(),
            }))
            .await?;

        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn get_services(&self, params: &Value) -> Result<Value> {
        let domain = params["domain"]
            .as_str()
            .ok_or_else(|| anyhow!("get_services requires a 'domain' parameter"))?;

        // REST endpoint returns an array; each element has "domain" + "services".
        let url = format!("{}/api/services", self.ha_base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.ha_token)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!("get_services: HTTP {}", resp.status()));
        }

        let data: Value = resp.json().await?;
        // Filter to the requested domain only.
        let filtered: Vec<&Value> = data
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|s| s["domain"].as_str() == Some(domain))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Value::Array(filtered.into_iter().cloned().collect()))
    }

    async fn get_areas(&self, _params: &Value) -> Result<Value> {
        let resp = self
            .send_raw(json!({
                "type": "config/area_registry/list",
            }))
            .await?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn get_area_entities(&self, params: &Value) -> Result<Value> {
        let area_id = params["area_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_area_entities: missing area_id"))?;

        // Get entity registry
        let entities_resp = self
            .send_raw(json!({
                "type": "config/entity_registry/list",
            }))
            .await?;

        // Get device registry
        let devices_resp = self
            .send_raw(json!({
                "type": "config/device_registry/list",
            }))
            .await?;

        let entities = entities_resp["result"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let devices = devices_resp["result"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        // Build device_id → area_id map
        let device_area: std::collections::HashMap<String, String> = devices
            .iter()
            .filter_map(|d| {
                let did = d["id"].as_str()?;
                let aid = d["area_id"].as_str()?;
                Some((did.to_string(), aid.to_string()))
            })
            .collect();

        // Find entities in the target area (direct or via device)
        let area_entities: Vec<String> = entities
            .iter()
            .filter_map(|e| {
                let eid = e["entity_id"].as_str()?;
                let entity_area = e["area_id"].as_str();
                let device_id = e["device_id"].as_str();

                if entity_area == Some(area_id) {
                    return Some(eid.to_string());
                }
                if let Some(did) = device_id {
                    if device_area.get(did).map(|a| a.as_str()) == Some(area_id) {
                        return Some(eid.to_string());
                    }
                }
                None
            })
            .collect();

        // Get states for those entities via REST (one call each).
        // This avoids the 54 MB get_states WebSocket response.
        let mut entity_states: Vec<Value> = Vec::with_capacity(area_entities.len());
        for eid in &area_entities {
            let url = format!("{}/api/states/{eid}", self.ha_base_url);
            match self.http.get(&url).bearer_auth(&self.ha_token).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(state) = resp.json::<Value>().await {
                        entity_states.push(state);
                    }
                }
                _ => {} // skip unavailable entities
            }
        }

        Ok(json!({
            "area_id": area_id,
            "entities": entity_states,
        }))
    }

    async fn get_datetime(&self, _params: &Value) -> Result<Value> {
        let now = Utc::now();
        let weekday = now.format("%A").to_string();
        Ok(json!({
            "iso": now.to_rfc3339(),
            "date": now.format("%Y-%m-%d").to_string(),
            "time": now.format("%H:%M:%S").to_string(),
            "weekday": &weekday,
            "day_of_week": &weekday,
            "timestamp": now.timestamp(),
        }))
    }

    async fn call_service(&self, params: &Value) -> Result<Value> {
        let domain = params["domain"]
            .as_str()
            .ok_or_else(|| anyhow!("call_service: missing domain"))?;
        let service = params["service"]
            .as_str()
            .ok_or_else(|| anyhow!("call_service: missing service"))?;
        let data = params.get("data").cloned().unwrap_or(json!({}));

        warn!(domain, service, "Agent calling service (side-effect)");

        self.ws_call_service(domain, service, data)
            .await?;

        Ok(json!({"ok": true, "domain": domain, "service": service}))
    }

    async fn get_trace(&self, params: &Value) -> Result<Value> {
        let domain = params
            .get("domain")
            .and_then(|d| d.as_str())
            .unwrap_or("automation");
        let item_id = params["item_id"]
            .as_str()
            .ok_or_else(|| anyhow!("get_trace: missing item_id"))?;

        let mut msg = json!({
            "type": "trace/list",
            "domain": domain,
            "item_id": item_id,
        });

        // If a specific run_id is provided, get the full trace
        if let Some(run_id) = params.get("run_id").and_then(|r| r.as_str()) {
            msg = json!({
                "type": "trace/get",
                "domain": domain,
                "item_id": item_id,
                "run_id": run_id,
            });
        }

        let resp = self.send_raw(msg).await?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn list_traces(&self, params: &Value) -> Result<Value> {
        let domain = params
            .get("domain")
            .and_then(|d| d.as_str())
            .unwrap_or("automation");

        let resp = self
            .send_raw(json!({
                "type": "trace/list",
                "domain": domain,
            }))
            .await?;

        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn get_status_page(&self, _params: &Value) -> Result<Value> {
        let url = self
            .status_page_url
            .as_deref()
            .unwrap_or("http://localhost:9102/?format=json");

        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("Status page: HTTP {}", resp.status()));
        }

        let body = resp.text().await?;
        // Try to parse as JSON; if it fails, wrap as string
        match serde_json::from_str::<Value>(&body) {
            Ok(v) => Ok(v),
            Err(_) => Ok(json!({"text": body})),
        }
    }

    // ── Board (findings API) ─────────────────────────────────

    fn board_base(&self) -> Result<&str> {
        self.board_url
            .as_deref()
            .ok_or_else(|| anyhow!("Board URL not configured"))
    }

    async fn board_get_posts(&self, _params: &Value) -> Result<Value> {
        let base = self.board_base()?;
        // Agent names are simple ASCII (e.g. "porch-lights") — no encoding needed.
        let url = format!("{base}/posts?agent={}&active=true", &self.agent_name);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("board_get_posts: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    async fn board_create_post(&self, params: &Value) -> Result<Value> {
        let base = self.board_base()?;
        let body = params["body"]
            .as_str()
            .ok_or_else(|| anyhow!("board_create_post: missing body"))?;
        let resp = self
            .http
            .post(format!("{base}/posts"))
            .json(&json!({ "agent": &self.agent_name, "body": body }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("board_create_post: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    async fn board_reply(&self, params: &Value) -> Result<Value> {
        let base = self.board_base()?;
        let post_id = params["post_id"]
            .as_i64()
            .ok_or_else(|| anyhow!("board_reply: missing post_id"))?;
        let body = params["body"]
            .as_str()
            .ok_or_else(|| anyhow!("board_reply: missing body"))?;
        let resp = self
            .http
            .post(format!("{base}/posts/{post_id}/replies"))
            .json(&json!({ "author": &self.agent_name, "body": body }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("board_reply: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    async fn board_close_post(&self, params: &Value) -> Result<Value> {
        let base = self.board_base()?;
        let post_id = params["post_id"]
            .as_i64()
            .ok_or_else(|| anyhow!("board_close_post: missing post_id"))?;
        let resp = self
            .http
            .patch(format!("{base}/posts/{post_id}"))
            .json(&json!({ "active": false }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("board_close_post: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    pub async fn board_get_post(&self, params: &Value) -> Result<Value> {
        let base = self.board_base()?;
        let post_id = params["post_id"]
            .as_i64()
            .ok_or_else(|| anyhow!("board_get_post: missing post_id"))?;
        let resp = self.http.get(format!("{base}/posts/{post_id}")).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("board_get_post: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    // ── House agent: cross-agent access ───────────────────────

    async fn board_get_all_posts(&self, params: &Value) -> Result<Value> {
        let base = self.board_base()?;
        let active_only = params["active_only"].as_bool().unwrap_or(true);
        let url = if active_only {
            format!("{base}/posts?active=true")
        } else {
            format!("{base}/posts")
        };
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("board_get_all_posts: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    async fn read_agent_memory(&self, params: &Value) -> Result<Value> {
        let agent_name = params["agent_name"]
            .as_str()
            .ok_or_else(|| anyhow!("read_agent_memory: missing agent_name"))?;

        // Validate agent name — only allow alphanumeric + hyphens
        if !agent_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(anyhow!("read_agent_memory: invalid agent name"));
        }

        let memory_dir = self.memory_dir.as_deref().unwrap_or("/var/lib/signal-ha/agent-memory");
        let path = format!("{memory_dir}/{agent_name}.json");
        let data = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow!("read_agent_memory({agent_name}): {e}"))?;
        let parsed: Value = serde_json::from_str(&data)
            .map_err(|e| anyhow!("read_agent_memory({agent_name}): invalid JSON: {e}"))?;
        // Return just the content string + metadata
        Ok(json!({
            "agent": agent_name,
            "content": parsed.get("content").and_then(|v| v.as_str()).unwrap_or(""),
            "session_count": parsed.get("session_count").and_then(|v| v.as_i64()).unwrap_or(0),
            "updated": parsed.get("updated").and_then(|v| v.as_str()).unwrap_or(""),
        }))
    }

    async fn read_transcript(&self, params: &Value) -> Result<Value> {
        let agent_name = params["agent_name"]
            .as_str()
            .ok_or_else(|| anyhow!("read_transcript: missing agent_name"))?;
        let nth = params["nth_latest"].as_i64().unwrap_or(0) as usize;

        // Validate agent name
        if !agent_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(anyhow!("read_transcript: invalid agent name"));
        }

        let transcript_dir = self.transcript_dir.as_deref().unwrap_or("/var/lib/signal-ha/transcripts");
        let prefix = format!("{agent_name}-");

        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(transcript_dir)
            .await
            .map_err(|e| anyhow!("read_transcript: can't read dir: {e}"))?;
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".md") {
                entries.push(entry.path());
            }
        }
        entries.sort();

        if entries.is_empty() {
            return Err(anyhow!("read_transcript: no transcripts for {agent_name}"));
        }

        let idx = if nth < entries.len() {
            entries.len() - 1 - nth
        } else {
            0
        };

        let content = tokio::fs::read_to_string(&entries[idx])
            .await
            .map_err(|e| anyhow!("read_transcript: {e}"))?;

        // Truncate to 50K chars to avoid context explosion
        let truncated = if content.len() > 50_000 {
            format!("{}\n\n[TRUNCATED — showing first 50K of {}K]", &content[..50_000], content.len() / 1000)
        } else {
            content
        };

        Ok(json!({
            "agent": agent_name,
            "file": entries[idx].file_name().and_then(|n| n.to_str()).unwrap_or(""),
            "content": truncated,
        }))
    }

    async fn read_status_page(&self, params: &Value) -> Result<Value> {
        let url = params["url"]
            .as_str()
            .ok_or_else(|| anyhow!("read_status_page: missing url"))?;

        // SSRF protection: only allow localhost status page ports
        if !url.starts_with("http://127.0.0.1:9") && !url.starts_with("http://localhost:9") {
            return Err(anyhow!(
                "read_status_page: only http://127.0.0.1:9xxx URLs are allowed"
            ));
        }

        let resp = self.http.get(url).send().await
            .map_err(|e| anyhow!("read_status_page({url}): {e}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("read_status_page: HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }
}
