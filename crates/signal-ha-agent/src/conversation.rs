//! HA Conversation API wrapper.
//!
//! Calls `conversation/process` via the existing `HaClient` WebSocket
//! connection.  This is the same API that Signal Deck's `AnalystSession`
//! uses — any HA Conversation agent (Claude, Ollama, OpenAI, etc.) works.
//!
//! The conversation is stateless per-call on the HA side, so we send the
//! full message history concatenated as text on every turn.  The LLM's
//! conversation entity manages its own context window.

use anyhow::{Context, Result};
use serde_json::json;
use signal_ha::HaClient;
use std::time::Duration;
use tracing::{debug, info};

/// Maximum number of history messages to keep (excluding system prompt).
/// Keep small — the agent's memory file carries long-term context,
/// so old turns within a session add little value and waste tokens.
const MAX_HISTORY_MESSAGES: usize = 4;

/// A message in the conversation history.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Manages a multi-turn conversation via HA `conversation/process`.
pub struct Conversation {
    client: HaClient,
    agent_id: Option<String>,
    messages: Vec<Message>,
    /// Unique ID for this conversation session.
    /// Ensures HA creates a fresh context and doesn't reuse/accumulate
    /// history from previous sessions on the same conversation entity.
    conversation_id: String,
}

impl Conversation {
    /// Create a new conversation.
    ///
    /// `agent_id` is the HA conversation entity ID (e.g.
    /// `"conversation.claude_conversation"`).  Pass `None` to use the
    /// default HA conversation agent.
    pub fn new(client: HaClient, agent_id: Option<String>) -> Self {
        // Generate a unique conversation ID so HA doesn't reuse/accumulate
        // context from previous sessions on this conversation entity.
        let conversation_id = format!(
            "signal-ha-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_millis(),
        );
        Self {
            client,
            agent_id,
            messages: Vec::new(),
            conversation_id,
        }
    }

    /// Initialize with a system prompt.
    pub fn set_system_prompt(&mut self, prompt: String) {
        // Remove any existing system message
        self.messages.retain(|m| m.role != Role::System);
        self.messages.insert(
            0,
            Message {
                role: Role::System,
                content: prompt,
            },
        );
    }

    /// Add a user message and call the LLM.  Returns the assistant's response.
    pub async fn send(&mut self, user_message: String) -> Result<String> {
        self.messages.push(Message {
            role: Role::User,
            content: user_message,
        });

        // Trim history if too long (keep system prompt + last N)
        self.trim_history();

        let response = self.call_llm().await?;

        self.messages.push(Message {
            role: Role::Assistant,
            content: response.clone(),
        });

        Ok(response)
    }

    // ── Private ────────────────────────────────────────────────

    /// Build the full text payload and call conversation/process.
    ///
    /// HA conversation/process is stateless — we send the full history
    /// concatenated as text.  This mirrors Signal Deck's `_callLLM()`.
    async fn call_llm(&self) -> Result<String> {
        // Build conversation text (excluding system prompt from "User:"/"Assistant:" blocks)
        let conversation_text: String = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| {
                let prefix = match m.role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::System => unreachable!(),
                };
                format!("{prefix}: {}", m.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        // Prepend system prompt if present
        let full_text = if let Some(system) = self.messages.first() {
            if system.role == Role::System {
                format!("{}\n\n---\n\n{}", system.content, conversation_text)
            } else {
                conversation_text
            }
        } else {
            conversation_text
        };

        // Use configured agent
        let agent_id = &self.agent_id;

        info!(
            agent_id = agent_id.as_deref().unwrap_or("default"),
            messages = self.messages.len(),
            text_len = full_text.len(),
            "Calling conversation/process"
        );

        let mut msg = json!({
            "type": "conversation/process",
            "text": full_text,
            "conversation_id": self.conversation_id,
        });

        if let Some(ref agent) = agent_id {
            msg["agent_id"] = json!(agent);
        }

        let resp = self
            .client
            .send_raw_timeout(msg, Duration::from_secs(120))
            .await
            .context("conversation/process call failed (120s timeout)")?;

        // Extract speech from response
        let speech = resp["result"]["response"]["speech"]["plain"]["speech"]
            .as_str()
            .unwrap_or("(no response)");

        debug!(
            response_len = speech.len(),
            "conversation/process response"
        );

        // Log first chunk of the LLM response for observability
        let preview: String = speech.chars().take(500).collect();
        info!(
            response_len = speech.len(),
            preview = %preview,
            "LLM response"
        );

        Ok(speech.to_string())
    }

    /// Trim conversation history to keep within context limits.
    fn trim_history(&mut self) {
        trim_messages(&mut self.messages, MAX_HISTORY_MESSAGES);
    }
}

/// Trim a message list to keep the system prompt (if present) plus
/// at most `max` non-system messages.
fn trim_messages(messages: &mut Vec<Message>, max: usize) {
    if messages.len() <= max + 1 {
        return;
    }

    let has_system = messages
        .first()
        .map(|m| m.role == Role::System)
        .unwrap_or(false);

    if has_system {
        let system = messages.remove(0);
        let keep_from = messages.len().saturating_sub(max);
        *messages = std::iter::once(system)
            .chain(messages.drain(keep_from..))
            .collect();
    } else {
        let keep_from = messages.len().saturating_sub(max);
        *messages = messages.drain(keep_from..).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_system_prompt() {
        let mut messages = vec![Message {
            role: Role::System,
            content: "system".into(),
        }];

        // Add more messages than the limit
        for i in 0..50 {
            messages.push(Message {
                role: if i % 2 == 0 { Role::User } else { Role::Assistant },
                content: format!("msg {i}"),
            });
        }

        trim_messages(&mut messages, MAX_HISTORY_MESSAGES);

        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].content, "system");
        // Should have system + MAX_HISTORY_MESSAGES
        assert!(messages.len() <= MAX_HISTORY_MESSAGES + 1);
    }
}
