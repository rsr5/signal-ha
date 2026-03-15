//! Agent session — the markdown-agent loop with Python runtime.
//!
//! This is the equivalent of Signal Deck's `AnalystSession.run()`,
//! but running headless in a systemd service instead of in-browser.
//!
//! Loop:
//! 1. Build system prompt with automation description + entity states
//! 2. Send to HA Conversation entity via `conversation/process`
//! 3. Parse LLM response for ```signal-deck (Python) blocks
//! 4. Execute each block in the Monty Python runtime
//! 5. Inject ```result blocks with output
//! 6. Send updated document back for next turn
//! 7. Loop until no executable blocks remain or budget exhausted

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use signal_ha::HaClient;
use tracing::{error, info, warn};

use crate::conversation::Conversation;
use crate::engine::AgentEngine;
use crate::ha_host::HaHost;
use crate::memory::Memory;
use crate::parser;

/// Configuration for an embedded agent.
///
/// The host automation constructs this and passes it to
/// `AgentHandle::spawn()`.
pub struct AgentConfig {
    /// Agent name (used for logging, memory file naming).
    pub name: String,

    /// The agent's specialist role / identity.
    /// Injected at the top of the system prompt to ground the LLM's
    /// perspective (e.g. "You are the garage lighting specialist.").
    pub role: String,

    /// The shared HA WebSocket client (for conversation/process + WS APIs).
    pub ha_client: HaClient,

    /// Force a specific conversation entity.
    /// If None, auto-detects (prefers Claude/Anthropic).
    pub conversation_entity: Option<String>,

    /// Plain-English description of what this automation does.
    /// This is the agent's ground truth — write it carefully.
    pub description: String,

    /// Entities this automation directly controls or reads.
    pub primary_entities: Vec<String>,

    /// HA area this automation lives in.
    /// The agent will use `get_area_entities()` at runtime to discover
    /// everything in this area, rather than relying on a hardcoded list.
    pub area: Option<String>,

    /// Maximum LLM turns within a single agent session.
    /// Default: 8.
    pub max_iterations: u32,

    /// How long to wait before starting the next session.
    /// Default: 24 hours.
    pub default_interval: Duration,

    /// HA host call configuration.
    pub ha_host: Arc<HaHost>,

    /// Path to the persistent memory file.
    pub memory_path: String,

    /// Service calls that are blocked (side-effect gating).
    /// Format: "domain.service" (e.g. "switch.turn_on").
    /// Empty = all calls allowed.
    pub disallowed_calls: Vec<String>,

    /// Directory for full session transcripts (markdown files).
    /// If set, each session writes a timestamped `.md` file with
    /// the complete system prompt + conversation.  Essential for
    /// tuning in early phases.
    pub transcript_dir: Option<String>,

    /// Inject current date/time into the system prompt.
    /// Gives the agent grounded temporal awareness so it can reason
    /// about "how long ago" events occurred. Default: true.
    pub inject_current_time: bool,

    /// URL path of the Lovelace dashboard this automation manages.
    /// If set, the agent is told about the dashboard and gets tools
    /// to inspect it (`list_dashboards()`, `get_dashboard(url_path)`).
    pub dashboard_url_path: Option<String>,
}

/// Handle to a running agent background task.
///
/// The agent runs as a tokio task within the automation's process.
/// Drop the handle to stop the agent.
pub struct AgentHandle {
    cancel: tokio::sync::watch::Sender<bool>,
    trigger: Arc<tokio::sync::Notify>,
}

impl AgentHandle {
    /// Spawn the agent as a background task.
    ///
    /// The agent waits for an external trigger (SIGUSR1) or its scheduled
    /// interval before running its first session.  It does NOT run on startup.
    pub fn spawn(config: AgentConfig) -> Self {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let trigger = Arc::new(tokio::sync::Notify::new());
        let trigger_clone = trigger.clone();

        tokio::spawn(async move {
            if let Err(e) = agent_loop(config, cancel_rx, trigger_clone).await {
                error!(error = %e, "Agent loop exited with error");
            }
        });

        Self { cancel: cancel_tx, trigger }
    }

    /// Trigger an immediate session (interrupts the sleep timer).
    ///
    /// If a session is already running, the trigger is queued and fires
    /// as soon as the current session finishes.
    pub fn trigger_now(&self) {
        info!("Agent session triggered manually");
        self.trigger.notify_one();
    }

    /// Get a clone of the trigger `Notify` for use from signal handlers
    /// or other tasks.
    pub fn trigger(&self) -> Arc<tokio::sync::Notify> {
        self.trigger.clone()
    }

    /// Stop the agent.
    pub fn stop(&self) {
        let _ = self.cancel.send(true);
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The main agent loop — runs sessions on a schedule.
async fn agent_loop(
    config: AgentConfig,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    trigger: Arc<tokio::sync::Notify>,
) -> Result<()> {
    info!(name = %config.name, "Agent starting");

    let mut interval = config.default_interval;

    let ctx = SessionCtx {
        name: config.name,
        role: config.role,
        conversation_entity: config.conversation_entity,
        description: config.description,
        primary_entities: config.primary_entities,
        area: config.area,
        max_iterations: config.max_iterations,
        default_interval: config.default_interval,
        ha_host: config.ha_host,
        memory_path: config.memory_path,
        disallowed_calls: config.disallowed_calls,
        transcript_dir: config.transcript_dir,
        inject_current_time: config.inject_current_time,
        dashboard_url_path: config.dashboard_url_path,
    };

    loop {
        if *cancel.borrow() {
            info!(name = %ctx.name, "Agent cancelled");
            return Ok(());
        }

        // Wait for scheduled interval or external trigger before running.
        // On first iteration this means the agent does NOT run on startup —
        // it waits for a SIGUSR1 (or schedule_next_session from a prior run).
        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = trigger.notified() => {
                info!(name = %ctx.name, "Agent triggered early");
            }
            _ = cancel.changed() => {
                info!(name = %ctx.name, "Agent cancelled during sleep");
                return Ok(());
            }
        }

        match run_session(&ctx).await {
            Ok(next_interval) => {
                interval = next_interval.unwrap_or(ctx.default_interval);
                info!(
                    name = %ctx.name,
                    next_in_hours = interval.as_secs() / 3600,
                    "Session complete"
                );
            }
            Err(e) => {
                error!(
                    name = %ctx.name,
                    error = %e,
                    "Session failed — will retry on schedule"
                );
            }
        }
    }
}

/// Internal session context.
struct SessionCtx {
    name: String,
    role: String,
    conversation_entity: Option<String>,
    description: String,
    primary_entities: Vec<String>,
    area: Option<String>,
    max_iterations: u32,
    default_interval: Duration,
    ha_host: Arc<HaHost>,
    memory_path: String,
    disallowed_calls: Vec<String>,
    transcript_dir: Option<String>,
    inject_current_time: bool,
    dashboard_url_path: Option<String>,
}

/// Run a single agent session — one multi-turn conversation.
///
/// Returns the interval for the next session (None = use default).
async fn run_session(ctx: &SessionCtx) -> Result<Option<Duration>> {
    info!(name = %ctx.name, "Starting agent session");

    let session_start = chrono::Utc::now();

    // Load memory
    let memory = Memory::load(&ctx.memory_path).await.unwrap_or_else(|e| {
        warn!(name = %ctx.name, error = %e, "Failed to load memory");
        Memory::empty(&ctx.memory_path)
    });
    let memory_text = memory
        .content()
        .unwrap_or("(none — this is your first session)")
        .to_string();

    // Build system prompt
    let system_prompt = build_system_prompt(ctx, &memory_text).await?;

    // Transcript collection
    let mut transcript = Vec::new();
    transcript.push(format!(
        "# {} — Session Transcript\n\n**Date:** {}\n**Agent:** {}\n\n---\n",
        ctx.name,
        session_start.format("%Y-%m-%d %H:%M UTC"),
        ctx.name,
    ));
    transcript.push(format!(
        "## System Prompt\n\n{}\n\n---\n",
        system_prompt,
    ));

    // Create conversation — get a fresh client handle from ha_host
    // (if the WS reconnects mid-session, conversation uses the old handle
    // but that's OK — the next session will get a new one).
    let fresh_client = ctx.ha_host.client().await;
    let mut conversation =
        Conversation::new(fresh_client, ctx.conversation_entity.clone());
    conversation.set_system_prompt(system_prompt);

    // Initialise the Python engine for this session
    let memory_handle = Arc::new(tokio::sync::Mutex::new(memory));
    let mut engine = AgentEngine::new(
        ctx.ha_host.clone(),
        "", // no init code needed
        &[
            "write_log",
            "get_status_page",
            "schedule_next_session",
            "get_agent_memory",
            "set_agent_memory",
            "board_get_posts",
            "board_create_post",
            "board_reply",
            "board_close_post",
        ],
        ctx.disallowed_calls.clone(),
        memory_handle,
    )?;

    // Initial user message kicks off the agent
    let initial_message = format!(
        "You are the {} agent. Begin your observation session. \
         Check the current entity states using Python, write observations \
         to the log, and remember to call set_agent_memory() before you finish.",
        ctx.name,
    );

    let mut last_doc_text: Option<String> = None;
    let mut prev_code = String::new();
    let mut repeat_count = 0u32;

    for iteration in 1..=ctx.max_iterations {
        let is_last = iteration == ctx.max_iterations;

        let user_message = if iteration == 1 {
            initial_message.clone()
        } else if let Some(ref doc_text) = last_doc_text {
            let nudge = if is_last {
                "\n\n[This is your FINAL turn. Call set_agent_memory() to save what you learned, \
                 then reply with NO code block to finish.]"
            } else {
                "\n\n[Results above. Continue your analysis or reply with \
                 NO code block to end the session.]"
            };
            format!("{doc_text}{nudge}")
        } else {
            break;
        };

        transcript.push(format!(
            "## Turn {iteration}\n\n### User\n\n{user_message}\n",
        ));

        info!(
            name = %ctx.name,
            iteration,
            max = ctx.max_iterations,
            "Agent turn"
        );

        // Call LLM via conversation/process
        let response = match conversation.send(user_message).await {
            Ok(r) => r,
            Err(e) => {
                error!(
                    name = %ctx.name,
                    iteration,
                    error = %e,
                    "LLM call failed"
                );
                transcript.push(format!("### Assistant\n\n**ERROR:** {e}\n"));
                break;
            }
        };

        // Log response for observability
        let response_preview: String = response.chars().take(500).collect();
        info!(
            name = %ctx.name,
            iteration,
            response_len = response.len(),
            response_preview = %response_preview,
            "LLM response received"
        );

        transcript.push(format!(
            "### Assistant\n\n{response}\n",
        ));

        // Parse for executable blocks (sanitize=true strips hallucinated results)
        let doc = parser::parse(&response, true);
        let executable = parser::get_executable_blocks(&doc);

        if executable.is_empty() {
            info!(
                name = %ctx.name,
                iteration,
                "No executable blocks — session done"
            );
            break;
        }

        // Repetition detection
        let current_code: String = executable
            .iter()
            .map(|b| b.content.trim())
            .collect::<Vec<_>>()
            .join("|");
        if current_code == prev_code {
            repeat_count += 1;
            if repeat_count >= 2 {
                warn!(
                    name = %ctx.name,
                    iteration,
                    "Repeated code blocks — forcing finish"
                );
                break;
            }
        } else {
            prev_code = current_code;
            repeat_count = 0;
        }

        // Execute each code block via the Python engine
        let block_contents: Vec<String> = executable
            .iter()
            .map(|b| b.content.clone())
            .collect();

        let mut updated_doc = doc;

        for content in &block_contents {
            info!(
                name = %ctx.name,
                code_len = content.len(),
                "Executing Python block"
            );

            let result = engine.eval_python(content).await;

            // Log a truncated preview of the result for observability
            let result_preview: String = result.chars().take(300).collect();
            info!(
                name = %ctx.name,
                result_len = result.len(),
                result_preview = %result_preview,
                "Python block executed"
            );

            transcript.push(format!(
                "### Python Result\n\n```\n{result}\n```\n",
            ));

            let blocks = parser::get_executable_blocks(&updated_doc);
            if let Some(block) = blocks.into_iter().next() {
                updated_doc = parser::inject_result(&updated_doc, block, &result);
            }
        }

        last_doc_text = Some(updated_doc.to_string());
    }

    info!(name = %ctx.name, "Agent session finished");

    // Write transcript to disk
    if let Some(ref dir) = ctx.transcript_dir {
        let ts = session_start.format("%Y%m%d-%H%M%S");
        let filename = format!("{dir}/{}-{ts}.md", ctx.name);
        let full_transcript = transcript.join("\n");

        if let Err(e) = tokio::fs::create_dir_all(dir).await {
            warn!(name = %ctx.name, error = %e, "Failed to create transcript dir");
        }
        match tokio::fs::write(&filename, &full_transcript).await {
            Ok(()) => info!(name = %ctx.name, path = %filename, "Transcript written"),
            Err(e) => warn!(name = %ctx.name, error = %e, "Failed to write transcript"),
        }
    }

    Ok(None)
}

/// Build the system prompt with Python API documentation.
async fn build_system_prompt(ctx: &SessionCtx, memory_text: &str) -> Result<String> {
    // Fetch current states for primary entities
    let mut primary_states = Vec::new();
    let client = ctx.ha_host.client().await;
    for eid in &ctx.primary_entities {
        match client.get_state(eid).await {
            Ok(state) => {
                primary_states.push(format!(
                    "  {eid}: {} (changed: {})",
                    state.state,
                    state.last_changed.format("%Y-%m-%d %H:%M")
                ));
            }
            Err(_) => {
                primary_states.push(format!("  {eid}: (unavailable)"));
            }
        }
    }

    // Build area instruction
    let area_section = match &ctx.area {
        Some(area) => format!(
            "## Area\n\nThis automation is in the **{area}** area. \
             Use `get_area_entities(\"{area}\")` to discover all entities in this area. \
             Not all entities in the area are related to this automation — \
             use the primary entities above to understand what this automation \
             directly controls, and explore area entities for additional context."
        ),
        None => "## Area\n\n(No area configured — use `get_areas()` to list available areas.)".to_string(),
    };

    let primary_entity = ctx
        .primary_entities
        .first()
        .map(|s| s.as_str())
        .unwrap_or("sensor.example");

    let time_line = if ctx.inject_current_time {
        let now = chrono::Local::now();
        format!("\n**Current date and time: {}**", now.format("%Y-%m-%d %H:%M %Z (%A)"))
    } else {
        String::new()
    };

    // Fetch open board posts for this agent (best-effort), including replies
    let board_section = match ctx.ha_host.fulfill("board_get_posts", &serde_json::json!({})).await {
        Ok(posts) => {
            if let Some(arr) = posts.as_array() {
                if arr.is_empty() {
                    "## Board\n\n(No open posts.)".to_string()
                } else {
                    let mut lines = vec!["## Board\n\nYour open posts from previous sessions. Review each one: if the issue is no longer visible in current data (e.g. dropped off logs, been fixed), close it with `board_close_post(post_id)`. If it's still relevant, leave it open or reply with an update.\n".to_string()];
                    for p in arr {
                        let id = p.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                        // Fetch full post with replies
                        let detail = ctx.ha_host.fulfill("board_get_post", &serde_json::json!({"post_id": id})).await;
                        match detail {
                            Ok(full) => {
                                let body = full.get("body").and_then(|v| v.as_str()).unwrap_or("");
                                let created = full.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                                lines.push(format!("### Post #{id} ({created})\n{body}"));
                                if let Some(replies) = full.get("replies").and_then(|v| v.as_array()) {
                                    for r in replies {
                                        let author = r.get("author").and_then(|v| v.as_str()).unwrap_or("unknown");
                                        let rbody = r.get("body").and_then(|v| v.as_str()).unwrap_or("");
                                        let rat = r.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                                        lines.push(format!("> **{author}** ({rat}): {rbody}"));
                                    }
                                }
                                lines.push(String::new());
                            }
                            Err(_) => {
                                let body = p.get("body").and_then(|v| v.as_str()).unwrap_or("");
                                let created = p.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                                lines.push(format!("- **Post #{id}** ({created}): {body}"));
                            }
                        }
                    }
                    lines.push("Use `board_close_post(post_id)` to close resolved/irrelevant posts. Use `board_reply(post_id, \"...\")` to add updates. Use `board_create_post(\"...\")` for new findings.".to_string());
                    lines.join("\n")
                }
            } else {
                "## Board\n\n(No open posts.)".to_string()
            }
        }
        Err(e) => {
            warn!(error = %e, "Failed to fetch board posts");
            "## Board\n\n(Board unavailable.)".to_string()
        }
    };

    let prompt = format!(
        r#"{role}

You are the embedded monitoring agent for the **{name}** automation service.
{time_line}

## What this automation does

{description}

## Primary entities (directly controlled or read)

{primary}

{area}

## Your role

You have two jobs, in priority order:

1. **Health check.** Quickly verify the automation is working: are the primary entities responsive, do recent events show correct cause→effect, are there sensor faults? This should take 1-2 turns. If everything is healthy, say so and move on — do not exhaustively re-verify what you confirmed last session.

2. **Explore and suggest.** This is the main event. Dig into the area's entities and recent data. Look for:
   - **Underutilised sensors** — data that exists but isn't acted on (e.g. illuminance, temperature, person detection, loft motion).
   - **Patterns and correlations** — usage patterns across time of day, day of week. When does the space get used? What does the data tell you about the occupant's habits?
   - **Improvement ideas** — concrete, specific suggestions for new automations or enhancements. For example: "illuminance is 200 lux when lights turn on at 2pm — consider skipping lights when lux > 150" or "the loft motion sensor never triggers — is it positioned correctly or is the battery dead?"
   - **Anomalies** — anything weird, unexpected, or worth flagging even if it isn't a bug.

Think like an engineer who owns this space and wants to make it smarter over time. Don't just report what you see — interpret it and propose what to do about it.

You may also suggest improvements to the automation's behaviour if the data supports it.

## Python API

Write Python code in ```signal-deck blocks. The runtime is a Python interpreter with built-in functions for querying Home Assistant:

### Entity state
- `state("entity_id")` → EntityState with .entity_id, .state, .domain, .name, .last_changed, .last_updated, .is_on, .attributes, .labels
- `states()` → list of all EntityState objects
- `states("domain")` → filtered by domain (e.g. `states("sensor")`)

### History & statistics
- `history("entity_id")` → dict with `entity_id`, `total_changes`, `history` (list of dicts with `"state"` and `"last_changed"` keys — use `h['state']` not `h.state`)
- `history("entity_id", hours=48)` → custom time range
- `statistics("entity_id")` → recorder long-term stats (default 24h, hourly). Returns dict with `mean`, `min`, `max`, `count`, `entries`. Check `count > 0` before using values.
- `statistics("entity_id", "5minute", 6)` → custom period and hours (positional args, no kwargs)

### Logbook & events
- `get_logbook("entity_id")` → logbook entries for an entity (24h default)
- `get_logbook("entity_id", hours)` → logbook entries with custom time window
- `get_events("calendar.entity_id")` → upcoming calendar events

### Areas
- `get_areas()` → list of HA areas
- `get_area_entities("area_id")` → all entities in an area with states

### Services
- `get_services("domain")` → list of services for a specific domain (e.g. "light", "switch")
- `call_service("domain", "service", {{"entity_id": "..."}})` → call a service

### Automation traces
- `list_traces()` → recent automation traces
- `get_trace("automation.id")` → trace for a specific automation
- `get_trace("automation.id", run_id="...")` → specific trace run

### Utilities
- `get_datetime()` → dict with date, time, weekday (or day_of_week), timestamp
- `ago(hours=N)` → ISO timestamp N hours ago (useful for time ranges)
- `show(obj)` → display a value (logged in headless mode)

### Memory (persistent across sessions)
- `get_agent_memory()` → string of your saved memory from previous sessions
- `set_agent_memory("your updated memory text")` → save memory for next session (overwrites previous)

**You MUST call `set_agent_memory()` on your last turn** to persist your observations, suggestions, and working model. If you don't call it, everything you learned this session is lost.

### Board (persistent findings)
- `board_get_posts()` → list of your open posts (active findings from previous sessions)
- `board_create_post("body text")` → create a new finding/observation/question (returns the post object)
- `board_reply(post_id, "reply text")` → add a reply to an existing post
- `board_close_post(post_id)` → close a post that is no longer relevant

The board is a persistent store for findings that survive across sessions. Use it for things the user should see and act on: faults, anomalies, questions.

**When creating a post**, be brief. State the fault in 1-3 sentences: what went wrong, when, and the key evidence. Do NOT include recommendations, action plans, or root cause speculation — the user will ask if they want more. Think: short bug report, not essay.

**Each session, review your open posts.** Check whether the issue is still visible in current data. If the user replied with a correction or resolution, or the issue no longer appears in logs/state, close it with `board_close_post(post_id)`. Don't let stale posts accumulate.

### Dashboards
- `list_dashboards()` → list of all Lovelace dashboards (url_path, title, icon, mode)
- `get_dashboard("url_path")` → full Lovelace config for a dashboard (views, cards, entities)

Use these to inspect the dashboard this automation manages. Check that all entities shown on the dashboard are working and that the layout makes sense.

### Important notes

- **No stdlib.** `import json`, `import datetime`, etc. will fail. Use the built-in functions above instead.
- **`state()` returns a dataclass** — use dot access: `s.state`, `s.last_changed`
- **`history()` returns a dict of dicts** — use bracket access: `h['history'][0]['state']`
- **`state()` and `states()` do NOT include area information.** Entity states have no `area_id` attribute. To find entities in an area, use `get_area_entities("area_id")`.
- All string processing (split, strip, f-strings, slicing) works normally.

### Example

```signal-deck
s = state("{primary_entity}")
print(f"{{s.entity_id}}: {{s.state}} (changed {{s.last_changed}})")
h = history("{primary_entity}", hours=12)
for entry in h['history']:
    print(f"  {{entry['state']}} at {{entry['last_changed']}}")
```

## Memory

Your memory persists between sessions via `set_agent_memory()`. **Call it before your session ends.**

{memory_section}

{board_section}

{dashboard_section}

## Guidelines

- **Health check first, then explore.** Don't spend all your turns verifying the automation works. Confirm it quickly, then use your remaining turns investigating the area.
- Be direct. Write observations as if a senior engineer will read them.
- Every session should produce at least one **concrete suggestion** — even if it's small. "Everything is fine" is not useful.
- Use `print()` for output that should appear in results.
- Use Python logic to filter, count, and aggregate data before reporting.
- **Use your memory.** Check what you found last session. Don't repeat the same analysis — build on it. If you suggested something last time, check if anything changed.
- **Save your memory on your final turn.** Call `set_agent_memory("...")` with a concise summary of what you found, open questions, and suggestions for next time.
- **All communication goes through the board.** Do not write session summaries or reports in your final message. If you found something worth reporting, it should already be a board post. When done, just reply with NO code block to end the session.
- **House agent.** A weekly overseer agent reviews all rooms' board posts and memories. If you see a reply from `house-agent` on one of your posts, treat it like a colleague's input — act on it, respond, or close the post as appropriate."#,
        name = ctx.name,
        role = ctx.role,
        description = ctx.description,
        primary = if primary_states.is_empty() {
            "  (none configured)".to_string()
        } else {
            primary_states.join("\n")
        },
        area = area_section,
        primary_entity = primary_entity,
        memory_section = if memory_text == "(none — this is your first session)" {
            "PREVIOUS SESSION MEMORY: (none — this is your first session)".to_string()
        } else {
            format!("PREVIOUS SESSION MEMORY:\n{memory_text}")
        },
        board_section = board_section,
        dashboard_section = match &ctx.dashboard_url_path {
            Some(url_path) => format!(
                "## Dashboard\n\n\
                 This automation manages a Lovelace dashboard at **`/{url_path}`**.\n\n\
                 The dashboard config is pushed from git on every deploy — do not edit it in the HA UI.\n\n\
                 As part of your health check, use `get_dashboard(\"{url_path}\")` to inspect the \
                 dashboard config. Verify that the entities shown on the dashboard are responsive \
                 and that the cards display meaningful data. If you spot an entity that is \
                 `unavailable` or `unknown`, flag it on the board."
            ),
            None => "## Dashboard\n\n(No dashboard configured for this automation.)".to_string(),
        },
    );

    Ok(prompt)
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_config_can_be_constructed() {
        // Verify the struct fields exist (compile-time check)
        let _fields = vec![
            "name", "role", "ha_client", "conversation_entity", "description",
            "primary_entities", "area",
            "max_iterations", "default_interval", "ha_host", "ha_ws_url",
            "memory_path", "disallowed_calls",
        ];
    }
}
