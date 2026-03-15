//! Agent shell engine — Python execution via signal-ha-shell.
//!
//! This is the headless equivalent of Signal Deck's `ShellEngine`.
//! Instead of rendering to a card UI, it returns text output that
//! gets injected as ```result blocks in the markdown agent loop.
//!
//! ## Execution flow
//!
//! 1. `eval_python(code)` tries `feed_snippet()` first (borrows REPL).
//! 2. If the snippet calls an external function, feed() fails with
//!    "not implemented with standard execution" — retry with `start_snippet()`.
//! 3. If `start_snippet()` suspends at a host call, we fulfill it
//!    via `HaHost` and resume the snapshot.  This can chain (a single
//!    snippet can make multiple host calls).
//! 4. The final output (print + expression value) is returned as text.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use signal_ha_shell::{
    self as shell,
    convert::{monty_obj_to_json, json_to_entity_state, json_to_entity_state_list},
    host_call::map_ext_call_to_host_call,
    repl::{self, ReplEvalResult},
};
use monty::MontyObject;
use monty::MontyRepl;
use monty::NoLimitTracker;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::ha_host::HaHost;
use crate::memory::Memory;

/// Maximum number of chained host calls per snippet.
/// Prevents infinite loops if the LLM writes pathological code.
const MAX_HOST_CALLS_PER_SNIPPET: u32 = 20;

/// The agent's Python execution engine.
///
/// Owns the Monty REPL session state and fulfills host calls
/// via `HaHost`.
pub struct AgentEngine {
    /// The Monty REPL — None if init failed or consumed by start().
    repl: Option<MontyRepl<NoLimitTracker>>,
    /// HA host call fulfillment.
    ha_host: Arc<HaHost>,
    /// Disallowed service calls (side-effect gating).
    disallowed_calls: Vec<String>,
    /// Extra external function names (beyond HA_EXTERNAL_FUNCTIONS).
    extra_functions: Vec<String>,
    /// Persistent agent memory (shared with session).
    memory: Arc<Mutex<Memory>>,
}

impl AgentEngine {
    /// Create a new engine and initialise the Python REPL.
    ///
    /// `init_code` runs once to set up variables, imports, etc.
    /// `extra_functions` registers additional external functions
    /// beyond the standard HA set (e.g. `write_log`, `get_status_page`).
    pub fn new(
        ha_host: Arc<HaHost>,
        init_code: &str,
        extra_functions: &[&str],
        disallowed_calls: Vec<String>,
        memory: Arc<Mutex<Memory>>,
    ) -> Result<Self> {
        let repl = if extra_functions.is_empty() {
            repl::init_repl(init_code)
        } else {
            repl::init_repl_with_functions(init_code, extra_functions)
        }
        .map_err(|e| anyhow!("Failed to init Python REPL: {e}"))?;

        let extra_fns: Vec<String> = extra_functions.iter().map(|s| s.to_string()).collect();

        Ok(Self {
            repl: Some(repl),
            ha_host,
            disallowed_calls,
            extra_functions: extra_fns,
            memory,
        })
    }

    /// Execute a Python snippet and return the text output.
    ///
    /// This is the main entry point called by the session loop
    /// for each ```signal-deck code block.
    pub async fn eval_python(&mut self, code: &str) -> String {
        // If the code references any known external function, skip feed()
        // and go straight to start().  feed() cannot handle external calls
        // and partially mutates REPL state before failing — then start()
        // re-executes the entire snippet on the already-mutated state,
        // causing double-execution of early statements.
        if repl::code_references_external_fn(code, &self.extra_functions) {
            let repl = match self.take_repl() {
                Some(r) => r,
                None => match repl::init_repl("") {
                    Ok(r) => r,
                    Err(e) => return format!("Error: REPL init failed: {e}"),
                },
            };
            let result = repl::start_snippet_with_extras(repl, code, &self.extra_functions);
            return self.handle_eval_result("", result).await;
        }

        // Phase 1: try feed() — borrows the REPL (no external calls expected)
        let feed_result = {
            let repl = match self.repl.as_mut() {
                Some(r) => r,
                None => {
                    // REPL not available — try to re-init
                    match repl::init_repl("") {
                        Ok(r) => {
                            self.repl = Some(r);
                            self.repl.as_mut().unwrap()
                        }
                        Err(e) => return format!("Error: REPL init failed: {e}"),
                    }
                }
            };
            repl::feed_snippet(repl, code)
        };

        match feed_result {
            Ok((output, value)) => {
                format_output(&output, value.as_ref())
            }
            Err(err_msg) => {
                if repl::is_name_error_for_external_fn_with_extras(&err_msg, &self.extra_functions) {
                    // Phase 2: retry with start() — consumes the REPL
                    let repl = match self.take_repl() {
                        Some(r) => r,
                        None => match repl::init_repl("") {
                            Ok(r) => r,
                            Err(e) => return format!("Error: REPL init failed: {e}"),
                        },
                    };
                    let result = repl::start_snippet_with_extras(repl, code, &self.extra_functions);
                    self.handle_eval_result("", result).await
                } else {
                    // Genuine error (syntax, runtime) — REPL still alive (feed borrows)
                    format!("Error: {err_msg}")
                }
            }
        }
    }

    /// Handle a ReplEvalResult — may chain through multiple host calls.
    async fn handle_eval_result(
        &mut self,
        prefix_output: &str,
        result: ReplEvalResult,
    ) -> String {
        let mut current_result = result;
        let mut output_so_far = prefix_output.to_string();
        let mut call_count = 0u32;

        loop {
            match current_result {
                ReplEvalResult::Complete { repl, output, value } => {
                    self.repl = Some(repl);
                    let combined = combine_output(&output_so_far, &output);
                    return format_output(&combined, value.as_ref());
                }
                ReplEvalResult::HostCallNeeded {
                    output,
                    function_name,
                    args,
                    call,
                } => {
                    call_count += 1;
                    if call_count > MAX_HOST_CALLS_PER_SNIPPET {
                        return format!(
                            "{}Error: Too many host calls in one snippet ({MAX_HOST_CALLS_PER_SNIPPET} max)",
                            combine_output(&output_so_far, &output),
                        );
                    }

                    let combined = combine_output(&output_so_far, &output);

                    // Handle locally-resolved functions
                    if function_name == "show" {
                        // show() in headless mode: format the value as text
                        let show_text = if let Some(first_arg) = args.first() {
                            let json = monty_obj_to_json(first_arg);
                            match serde_json::to_string_pretty(&json) {
                                Ok(s) => s,
                                Err(_) => format!("{json}"),
                            }
                        } else {
                            "None".to_string()
                        };
                        output_so_far = if combined.is_empty() {
                            show_text
                        } else {
                            format!("{combined}\n{show_text}")
                        };
                        // Resume with None
                        current_result = repl::resume_call_with_extras(
                            call,
                            MontyObject::None,
                            &self.extra_functions,
                        );
                        continue;
                    }

                    if matches!(
                        function_name.as_str(),
                        "plot_line" | "plot_bar" | "plot_pie" | "plot_series"
                    ) {
                        // Charts not supported headless — resume with None
                        output_so_far = combine_output(&combined, "(chart not rendered in headless mode)");
                        current_result = repl::resume_call_with_extras(
                            call,
                            MontyObject::None,
                            &self.extra_functions,
                        );
                        continue;
                    }

                    if function_name == "ago" {
                        let result_obj = shell::host_call::parse_ago(&args);
                        output_so_far = combined;
                        current_result = repl::resume_call_with_extras(
                            call,
                            result_obj,
                            &self.extra_functions,
                        );
                        continue;
                    }

                    if function_name == "get_datetime" {
                        // Handle locally — no network needed
                        let now = chrono::Utc::now();
                        let weekday = now.format("%A").to_string();
                        let dt_json = serde_json::json!({
                            "iso": now.to_rfc3339(),
                            "date": now.format("%Y-%m-%d").to_string(),
                            "time": now.format("%H:%M:%S").to_string(),
                            "weekday": &weekday,
                            "day_of_week": &weekday,
                            "timestamp": now.timestamp(),
                        });
                        let monty_val = shell::convert::json_to_monty_obj(&dt_json);
                        output_so_far = combined;
                        current_result = repl::resume_call_with_extras(
                            call,
                            monty_val,
                            &self.extra_functions,
                        );
                        continue;
                    }

                    // ── Agent-specific builtins ──────────────────────

                    if function_name == "write_log" {
                        // Emit a structured log message via tracing.
                        // write_log("message") or write_log("message", "level")
                        let message = args
                            .first()
                            .map(|a| {
                                let j = monty_obj_to_json(a);
                                j.as_str().unwrap_or("").to_string()
                            })
                            .unwrap_or_default();
                        let level = args
                            .get(1)
                            .map(|a| {
                                let j = monty_obj_to_json(a);
                                j.as_str().unwrap_or("info").to_string()
                            })
                            .unwrap_or_else(|| "info".to_string());

                        match level.as_str() {
                            "warn" | "warning" => warn!(agent_log = true, "{message}"),
                            "error" => tracing::error!(agent_log = true, "{message}"),
                            "debug" => tracing::debug!(agent_log = true, "{message}"),
                            _ => info!(agent_log = true, "{message}"),
                        }

                        output_so_far = combined;
                        current_result = repl::resume_call_with_extras(
                            call,
                            MontyObject::None,
                            &self.extra_functions,
                        );
                        continue;
                    }

                    if function_name == "get_status_page" {
                        // Delegate to ha_host which has the status_page_url.
                        let params = serde_json::json!({});
                        match self.ha_host.fulfill("get_status_page", &params).await {
                            Ok(json_response) => {
                                let monty_val =
                                    shell::convert::json_to_monty_obj(&json_response);
                                output_so_far = combined;
                                current_result = repl::resume_call_with_extras(
                                    call,
                                    monty_val,
                                    &self.extra_functions,
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "get_status_page failed");
                                output_so_far = combined;
                                current_result = repl::resume_call_with_extras(
                                    call,
                                    MontyObject::String(format!("Error: {e}")),
                                    &self.extra_functions,
                                );
                            }
                        }
                        continue;
                    }

                    if function_name == "schedule_next_session" {
                        // Stub: log the requested interval, return None.
                        // Full implementation will adjust the automation's
                        // tokio timer from within the session loop.
                        let hours = args
                            .first()
                            .map(|a| {
                                let j = monty_obj_to_json(a);
                                j.as_f64().unwrap_or(24.0)
                            })
                            .unwrap_or(24.0);
                        info!(
                            hours,
                            "Agent requested next session (stub — using default interval)"
                        );
                        output_so_far = combined;
                        current_result = repl::resume_call_with_extras(
                            call,
                            MontyObject::None,
                            &self.extra_functions,
                        );
                        continue;
                    }

                    if function_name == "get_agent_memory" {
                        let mem = self.memory.lock().await;
                        let content = mem
                            .content()
                            .unwrap_or("(no previous memory)")
                            .to_string();
                        drop(mem);
                        output_so_far = combined;
                        current_result = repl::resume_call_with_extras(
                            call,
                            MontyObject::String(content),
                            &self.extra_functions,
                        );
                        continue;
                    }

                    if function_name == "set_agent_memory" {
                        // set_agent_memory("content text") or
                        // set_agent_memory({"content": "text"})
                        let content = args
                            .first()
                            .map(|a| {
                                let j = monty_obj_to_json(a);
                                if let Some(s) = j.as_str() {
                                    s.to_string()
                                } else if let Some(s) = j.get("content").and_then(|v| v.as_str()) {
                                    s.to_string()
                                } else {
                                    serde_json::to_string_pretty(&j).unwrap_or_default()
                                }
                            })
                            .unwrap_or_default();

                        if content.is_empty() {
                            output_so_far = combined;
                            current_result = repl::resume_call_with_extras(
                                call,
                                MontyObject::String("Error: empty content".to_string()),
                                &self.extra_functions,
                            );
                            continue;
                        }

                        let mut mem = self.memory.lock().await;
                        match mem.save(&content).await {
                            Ok(()) => {
                                info!(content_len = content.len(), "Agent memory saved");
                                output_so_far = combined;
                                current_result = repl::resume_call_with_extras(
                                    call,
                                    MontyObject::String("Memory saved.".to_string()),
                                    &self.extra_functions,
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to save agent memory");
                                output_so_far = combined;
                                current_result = repl::resume_call_with_extras(
                                    call,
                                    MontyObject::String(format!("Error saving memory: {e}")),
                                    &self.extra_functions,
                                );
                            }
                        }
                        continue;
                    }

                    // Map to host call method + params
                    match map_ext_call_to_host_call(&function_name, &args) {
                        Some((method, params)) => {
                            // Check side-effect gating
                            if method == "call_service"
                                && self.disallowed_calls.iter().any(|d| {
                                    let parts: Vec<&str> = d.splitn(2, '.').collect();
                                    parts.len() == 2
                                        && params.get("domain").and_then(|v| v.as_str())
                                            == Some(parts[0])
                                        && params.get("service").and_then(|v| v.as_str())
                                            == Some(parts[1])
                                })
                            {
                                output_so_far = combined;
                                let err_msg = format!(
                                    "Service call blocked by side-effect gate: {}",
                                    params
                                );
                                warn!("{err_msg}");
                                current_result = repl::resume_call_with_extras(
                                    call,
                                    MontyObject::None,
                                    &self.extra_functions,
                                );
                                continue;
                            }

                            // Fulfill via HA
                            match self.ha_host.fulfill(method, &params).await {
                                Ok(json_response) => {
                                    // Convert JSON to MontyObject using typed conversion
                                    let monty_val = match method {
                                        "get_state" => json_to_entity_state(&json_response),
                                        "get_states" | "get_area_entities" => {
                                            if let Some(entities) = json_response.get("entities") {
                                                json_to_entity_state_list(entities)
                                            } else if json_response.is_array() {
                                                json_to_entity_state_list(&json_response)
                                            } else {
                                                shell::convert::json_to_monty_obj(&json_response)
                                            }
                                        }
                                        _ => shell::convert::json_to_monty_obj(&json_response),
                                    };

                                    output_so_far = combined;
                                    current_result = repl::resume_call_with_extras(
                                        call,
                                        monty_val,
                                        &self.extra_functions,
                                    );
                                }
                                Err(e) => {
                                    // Host call failed — resume with error string
                                    warn!(method, error = %e, "Host call failed");
                                    output_so_far = combined;
                                    let error_obj = MontyObject::String(format!("Error: {e}"));
                                    current_result = repl::resume_call_with_extras(
                                        call,
                                        error_obj,
                                        &self.extra_functions,
                                    );
                                }
                            }
                        }
                        None => {
                            return format!(
                                "{combined}Error: Unknown external function '{function_name}'"
                            );
                        }
                    }
                }
                ReplEvalResult::Error { message, repl } => {
                    if let Some(r) = repl {
                        self.repl = Some(r);
                    }
                    let combined = combine_output(&output_so_far, "");
                    return if combined.is_empty() {
                        format!("Error: {message}")
                    } else {
                        format!("{combined}\nError: {message}")
                    };
                }
            }
        }
    }

    /// Take the REPL out (for start() which consumes it).
    fn take_repl(&mut self) -> Option<MontyRepl<NoLimitTracker>> {
        self.repl.take()
    }
}

/// Combine prefix output with new output.
fn combine_output(prefix: &str, new: &str) -> String {
    match (prefix.is_empty(), new.is_empty()) {
        (true, true) => String::new(),
        (true, false) => new.to_string(),
        (false, true) => prefix.to_string(),
        (false, false) => format!("{prefix}\n{new}"),
    }
}

/// Format output + optional expression value for injection into
/// the ```result block.
fn format_output(output: &str, value: Option<&MontyObject>) -> String {
    let val_str = match value {
        Some(obj) => {
            let json = monty_obj_to_json(obj);
            match &json {
                Value::Null => None,
                Value::String(s) if s.is_empty() => None,
                _ => Some(
                    serde_json::to_string_pretty(&json).unwrap_or_else(|_| format!("{json}"))
                ),
            }
        }
        None => None,
    };

    match (output.is_empty(), val_str) {
        (true, None) => "(ok)".to_string(),
        (true, Some(v)) => v,
        (false, None) => output.to_string(),
        (false, Some(v)) => format!("{output}\n{v}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_combine_output() {
        assert_eq!(combine_output("", ""), "");
        assert_eq!(combine_output("a", ""), "a");
        assert_eq!(combine_output("", "b"), "b");
        assert_eq!(combine_output("a", "b"), "a\nb");
    }

    #[test]
    fn test_format_output_empty() {
        assert_eq!(format_output("", None), "(ok)");
    }

    #[test]
    fn test_format_output_with_text() {
        assert_eq!(format_output("hello world", None), "hello world");
    }
}
