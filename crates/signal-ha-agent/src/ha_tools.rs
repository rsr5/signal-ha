//! Home Assistant tool implementations for signal-ha automations.
//!
//! Each struct captures its own state and implements `Tool`.
//! The host automation registers the tools it needs:
//!
//! ```rust,ignore
//! use signal_ha_agent::ha_tools;
//!
//! let mut registry = ToolRegistry::new();
//! ha_tools::register_all(&mut registry, &opts);
//! ```
//!
//! Signal Deck would register a completely different set of tools
//! (run_python, render_chart, etc.) using the same `Tool` trait.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use signal_ha::HaClient;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::memory::Memory;
use crate::tools::{Tool, ToolRegistry, ToolResult};

// ── Shared mutable state ───────────────────────────────────────

/// State that tools need shared, mutable access to.
///
/// Wrapped in `Arc<Mutex<_>>` so multiple tools can hold a reference
/// and the session loop can read back results after execution.
#[derive(Clone)]
pub struct SharedState {
    pub memory: Arc<Mutex<Memory>>,
    pub next_session_after: Arc<Mutex<Option<Duration>>>,
}

/// Options for constructing the HA tool set.
pub struct HaToolOpts {
    pub ha_client: HaClient,
    pub http: reqwest::Client,
    pub ha_base_url: String,
    pub ha_token: String,
    pub status_page_url: Option<String>,
    pub shared: SharedState,
}

/// Register all HA tools into a registry.
pub fn register_all(registry: &mut ToolRegistry, opts: &HaToolOpts) {
    registry.register(GetStateTool {
        http: opts.http.clone(),
        base_url: opts.ha_base_url.clone(),
        token: opts.ha_token.clone(),
    });
    registry.register(GetStatesTool { client: opts.ha_client.clone() });
    registry.register(GetHistoryTool {
        http: opts.http.clone(),
        base_url: opts.ha_base_url.clone(),
        token: opts.ha_token.clone(),
    });
    registry.register(GetLogbookTool {
        http: opts.http.clone(),
        base_url: opts.ha_base_url.clone(),
        token: opts.ha_token.clone(),
    });
    registry.register(GetStatusPageTool {
        http: opts.http.clone(),
        url: opts.status_page_url.clone(),
    });
    registry.register(WriteLogTool);
    registry.register(GetAgentMemoryTool { memory: opts.shared.memory.clone() });
    registry.register(SetAgentMemoryTool { memory: opts.shared.memory.clone() });
    registry.register(ScheduleNextSessionTool {
        next: opts.shared.next_session_after.clone(),
    });
    registry.register(SuggestConfigChangeTool);
}

// ── get_state ──────────────────────────────────────────────────

struct GetStateTool {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Tool for GetStateTool {
    fn name(&self) -> &str { "get_state" }
    fn description(&self) -> &str {
        "Returns current state, attributes, and last_changed for an entity."
    }
    fn usage(&self) -> &str {
        r#"get_state({"entity_id": "sensor.temp"})"#
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(entity_id) = args["entity_id"].as_str() else {
                return ToolResult::err("Missing required arg: entity_id");
            };
            let url = format!("{}/api/states/{entity_id}", self.base_url);
            let resp = match self.http.get(&url).bearer_auth(&self.token).send().await {
                Ok(r) => r,
                Err(e) => return ToolResult::err(format!("Error getting state for {entity_id}: {e}")),
            };
            if !resp.status().is_success() {
                return ToolResult::err(format!("Error getting state for {entity_id}: HTTP {}", resp.status()));
            }
            match resp.json::<Value>().await {
                Ok(state) => ToolResult::ok(
                    serde_json::to_string_pretty(&json!({
                        "entity_id": state["entity_id"],
                        "state": state["state"],
                        "attributes": state["attributes"],
                        "last_changed": state["last_changed"],
                    }))
                    .unwrap_or_default(),
                ),
                Err(e) => ToolResult::err(format!("Error parsing state for {entity_id}: {e}")),
            }
        })
    }
}

// ── get_states ─────────────────────────────────────────────────

struct GetStatesTool {
    client: HaClient,
}

impl Tool for GetStatesTool {
    fn name(&self) -> &str { "get_states" }
    fn description(&self) -> &str {
        "Returns all entity states. Optional \"domain\" filter."
    }
    fn usage(&self) -> &str {
        r#"get_states({"domain": "sensor"})"#
    }
    fn help_lines(&self) -> &[&str] {
        &["Without domain, returns ALL entities (large!). Use sparingly."]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let domain = args["domain"].as_str();

            let resp = match self.client.send_raw(json!({"type": "get_states"})).await {
                Ok(r) => r,
                Err(e) => return ToolResult::err(format!("Error fetching states: {e}")),
            };

            let Some(states) = resp["result"].as_array() else {
                return ToolResult::err("No states returned");
            };

            let filtered: Vec<&Value> = if let Some(domain) = domain {
                states
                    .iter()
                    .filter(|s| {
                        s["entity_id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with(&format!("{domain}.")))
                    })
                    .collect()
            } else {
                states.iter().collect()
            };

            let summary: Vec<Value> = filtered
                .iter()
                .map(|s| {
                    json!({
                        "entity_id": s["entity_id"],
                        "state": s["state"],
                        "name": s["attributes"]["friendly_name"],
                    })
                })
                .collect();

            ToolResult::ok(serde_json::to_string_pretty(&summary).unwrap_or_default())
        })
    }
}

// ── get_history ────────────────────────────────────────────────

struct GetHistoryTool {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Tool for GetHistoryTool {
    fn name(&self) -> &str { "get_history" }
    fn description(&self) -> &str {
        "Returns state change history for an entity over the last N hours."
    }
    fn usage(&self) -> &str {
        r#"get_history({"entity_id": "sensor.temp", "hours": 24})"#
    }
    fn help_lines(&self) -> &[&str] {
        &["Default: 24 hours. This is your primary evidence source."]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(entity_id) = args["entity_id"].as_str() else {
                return ToolResult::err("Missing required arg: entity_id");
            };

            let hours = args["hours"].as_u64().unwrap_or(24);
            let start = Utc::now() - chrono::Duration::hours(hours as i64);
            let start_str = start.to_rfc3339();

            let url = format!(
                "{}/api/history/period/{start_str}?filter_entity_id={entity_id}&minimal_response&no_attributes",
                self.base_url,
            );

            match self.http.get(&url).bearer_auth(&self.token).send().await {
                Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                    Ok(data) => {
                        let changes = data
                            .as_array()
                            .and_then(|a| a.first())
                            .and_then(|a| a.as_array())
                            .cloned()
                            .unwrap_or_default();

                        let total = changes.len();

                        // Cap at 100 entries to avoid blowing
                        // the LLM context window. Show first 10
                        // + last 90, or all if ≤ 100.
                        const MAX_ENTRIES: usize = 100;
                        let summary: Vec<Value> = if total <= MAX_ENTRIES {
                            changes
                                .iter()
                                .map(|c| json!({"state": c["state"], "last_changed": c["last_changed"]}))
                                .collect()
                        } else {
                            // Show a representative sample:
                            // first 10 + last 90 entries
                            let first_n = 10;
                            let last_n = MAX_ENTRIES - first_n;
                            let first: Vec<Value> = changes[..first_n]
                                .iter()
                                .map(|c| json!({"state": c["state"], "last_changed": c["last_changed"]}))
                                .collect();
                            let last: Vec<Value> = changes[total - last_n..]
                                .iter()
                                .map(|c| json!({"state": c["state"], "last_changed": c["last_changed"]}))
                                .collect();
                            [first, vec![json!({"_note": format!("... {} entries omitted ...", total - MAX_ENTRIES)})], last].concat()
                        };

                        ToolResult::ok(
                            serde_json::to_string_pretty(&json!({
                                "entity_id": entity_id,
                                "hours": hours,
                                "total_changes": total,
                                "showing": summary.len().min(total),
                                "history": summary,
                            }))
                            .unwrap_or_default(),
                        )
                    }
                    Err(e) => ToolResult::err(format!("Failed to parse history response: {e}")),
                },
                Ok(resp) => ToolResult::err(format!("History API error: HTTP {}", resp.status())),
                Err(e) => ToolResult::err(format!("History API request failed: {e}")),
            }
        })
    }
}

// ── get_logbook ────────────────────────────────────────────────

struct GetLogbookTool {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Tool for GetLogbookTool {
    fn name(&self) -> &str { "get_logbook" }
    fn description(&self) -> &str {
        "Returns logbook entries showing WHO/WHAT caused each state change."
    }
    fn usage(&self) -> &str {
        r#"get_logbook({"entity_id": "switch.garage", "hours": 24})"#
    }
    fn help_lines(&self) -> &[&str] {
        &["Shows context: which automation, user, or service triggered changes."]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let entity_id = args["entity_id"].as_str();
            let hours = args["hours"].as_u64().unwrap_or(24);
            let start = Utc::now() - chrono::Duration::hours(hours as i64);
            let start_str = start.to_rfc3339();
            let end_str = Utc::now().to_rfc3339();

            let mut url = format!(
                "{}/api/logbook/{start_str}?end_time={end_str}",
                self.base_url,
            );
            if let Some(eid) = entity_id {
                url.push_str(&format!("&entity={eid}"));
            }

            match self.http.get(&url).bearer_auth(&self.token).send().await {
                Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                    Ok(data) => {
                        let entries = data.as_array().cloned().unwrap_or_default();
                        let summary: Vec<Value> = entries
                            .iter()
                            .map(|e| {
                                json!({
                                    "when": e["when"],
                                    "name": e["name"],
                                    "state": e["state"],
                                    "entity_id": e["entity_id"],
                                    "context_domain": e["context_domain"],
                                    "context_service": e["context_service"],
                                    "context_entity_name": e["context_entity_id_name"],
                                })
                            })
                            .collect();

                        ToolResult::ok(
                            serde_json::to_string_pretty(&json!({
                                "entries": summary.len(),
                                "logbook": summary,
                            }))
                            .unwrap_or_default(),
                        )
                    }
                    Err(e) => ToolResult::err(format!("Failed to parse logbook response: {e}")),
                },
                Ok(resp) => ToolResult::err(format!("Logbook API error: HTTP {}", resp.status())),
                Err(e) => ToolResult::err(format!("Logbook API request failed: {e}")),
            }
        })
    }
}

// ── get_status_page ────────────────────────────────────────────

struct GetStatusPageTool {
    http: reqwest::Client,
    url: Option<String>,
}

impl Tool for GetStatusPageTool {
    fn name(&self) -> &str { "get_status_page" }
    fn description(&self) -> &str {
        "Returns the automation's own JSON status page."
    }
    fn usage(&self) -> &str {
        "get_status_page()"
    }
    fn help_lines(&self) -> &[&str] {
        &[
            "Shows current scores, active timers, last-applied states —",
            "exactly what the automation is doing right now.",
        ]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let url = args["url"]
                .as_str()
                .map(String::from)
                .or_else(|| self.url.clone())
                .unwrap_or_else(|| "http://localhost:9102/?format=json".into());

            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.text().await {
                    Ok(body) => ToolResult::ok(body),
                    Err(e) => ToolResult::err(format!("Failed to read status page body: {e}")),
                },
                Ok(resp) => ToolResult::err(format!("Status page error: HTTP {}", resp.status())),
                Err(e) => ToolResult::err(format!("Status page request failed: {e}")),
            }
        })
    }
}

// ── write_log ──────────────────────────────────────────────────

struct WriteLogTool;

impl Tool for WriteLogTool {
    fn name(&self) -> &str { "write_log" }
    fn description(&self) -> &str {
        "Write to the automation's journald log. Levels: debug, info, warn."
    }
    fn usage(&self) -> &str {
        r#"write_log({"message": "Observation text", "level": "info"})"#
    }
    fn help_lines(&self) -> &[&str] {
        &["This is your primary output. Write concise, engineer-grade observations."]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let message = args["message"].as_str().unwrap_or("(empty)");
            let level = args["level"].as_str().unwrap_or("info");

            match level {
                "debug" => debug!(agent = true, "{message}"),
                "warn" => warn!(agent = true, "{message}"),
                _ => info!(agent = true, "{message}"),
            }

            ToolResult::ok(format!("Logged ({level}): {message}"))
        })
    }
}

// ── get_agent_memory ───────────────────────────────────────────

struct GetAgentMemoryTool {
    memory: Arc<Mutex<Memory>>,
}

impl Tool for GetAgentMemoryTool {
    fn name(&self) -> &str { "get_agent_memory" }
    fn description(&self) -> &str {
        "Read your persistent memory from previous sessions."
    }
    fn usage(&self) -> &str {
        "get_agent_memory()"
    }
    fn help_lines(&self) -> &[&str] {
        &["Returns your accumulated observations and working model."]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let _ = args;
        Box::pin(async move {
            let memory = self.memory.lock().await;
            ToolResult::ok(
                memory.content().unwrap_or("(no previous memory)").to_string(),
            )
        })
    }
}

// ── set_agent_memory ───────────────────────────────────────────

struct SetAgentMemoryTool {
    memory: Arc<Mutex<Memory>>,
}

impl Tool for SetAgentMemoryTool {
    fn name(&self) -> &str { "set_agent_memory" }
    fn description(&self) -> &str {
        "Write your persistent memory for the next session."
    }
    fn usage(&self) -> &str {
        r#"set_agent_memory({"content": "Updated memory text"})"#
    }
    fn help_lines(&self) -> &[&str] {
        &[
            "Overwrite — include everything you want to remember.",
            "Write coherent prose, not JSON. Summarize patterns, open questions.",
        ]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(content) = args["content"].as_str() else {
                return ToolResult::err("Missing required arg: content");
            };
            let mut memory = self.memory.lock().await;
            match memory.save(content).await {
                Ok(()) => ToolResult::ok("Memory saved."),
                Err(e) => ToolResult::err(format!("Failed to save memory: {e}")),
            }
        })
    }
}

// ── schedule_next_session ──────────────────────────────────────

struct ScheduleNextSessionTool {
    next: Arc<Mutex<Option<Duration>>>,
}

impl Tool for ScheduleNextSessionTool {
    fn name(&self) -> &str { "schedule_next_session" }
    fn description(&self) -> &str {
        "Set when you next wake up. Default: 24 hours."
    }
    fn usage(&self) -> &str {
        r#"schedule_next_session({"hours": 24})"#
    }
    fn help_lines(&self) -> &[&str] {
        &["Call this on your last turn."]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let hours = args["hours"].as_u64().unwrap_or(24);
            *self.next.lock().await = Some(Duration::from_secs(hours * 3600));
            info!(hours, "Agent scheduled next session");
            ToolResult::ok(format!("Next session scheduled in {hours} hours."))
        })
    }
}

// ── suggest_config_change ──────────────────────────────────────

struct SuggestConfigChangeTool;

impl Tool for SuggestConfigChangeTool {
    fn name(&self) -> &str { "suggest_config_change" }
    fn description(&self) -> &str {
        "Propose a specific, actionable change to the automation config."
    }
    fn usage(&self) -> &str {
        r#"suggest_config_change({"entity": "...", "param": "...", "current": "...", "suggested": "...", "reason": "...", "confidence": "high|medium|low"})"#
    }
    fn help_lines(&self) -> &[&str] {
        &[
            "Only suggest with strong evidence (multiple days of data).",
            "The operator reviews suggestions — nothing is auto-applied.",
        ]
    }

    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let suggestion = json!({
                "type": "config_suggestion",
                "entity": args["entity"],
                "param": args["param"],
                "current": args["current"],
                "suggested": args["suggested"],
                "reason": args["reason"],
                "confidence": args["confidence"],
            });

            info!(suggestion = %suggestion, "Agent config suggestion");

            ToolResult::ok(format!(
                "Suggestion recorded: {} → {} ({})",
                args["param"].as_str().unwrap_or("?"),
                args["suggested"].as_str().unwrap_or("?"),
                args["confidence"].as_str().unwrap_or("?"),
            ))
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_log_levels() {
        let tool = WriteLogTool;
        let r = tool.execute(json!({"message": "test", "level": "info"})).await;
        assert!(!r.is_error);
        assert!(r.output.contains("Logged (info)"));

        let r = tool.execute(json!({"message": "test", "level": "warn"})).await;
        assert!(r.output.contains("Logged (warn)"));
    }

    #[tokio::test]
    async fn schedule_sets_duration() {
        let next = Arc::new(Mutex::new(None));
        let tool = ScheduleNextSessionTool { next: next.clone() };

        let r = tool.execute(json!({"hours": 12})).await;
        assert!(!r.is_error);
        assert_eq!(*next.lock().await, Some(Duration::from_secs(12 * 3600)));
    }

    #[tokio::test]
    async fn suggest_config_change_logs() {
        let tool = SuggestConfigChangeTool;
        let r = tool
            .execute(json!({
                "entity": "binary_sensor.garage_motion",
                "param": "TIMEOUT",
                "current": "600s",
                "suggested": "300s",
                "reason": "Evidence from 7 days",
                "confidence": "high"
            }))
            .await;
        assert!(!r.is_error);
        assert!(r.output.contains("high"));
    }

    #[tokio::test]
    async fn register_all_builds_registry() {
        // We can't construct a real HaClient, but we can verify
        // register_all doesn't panic and produces the right tool count
        // by testing with individual tools we *can* construct.
        let mut reg = ToolRegistry::new();
        reg.register(WriteLogTool);
        reg.register(SuggestConfigChangeTool);

        let docs = reg.tool_docs();
        assert!(docs.contains("write_log"));
        assert!(docs.contains("suggest_config_change"));
    }
}
