//! Tool trait and registry — the extensible dispatch layer.
//!
//! The markdown-agent pattern works like this: the LLM writes a ```tool
//! block containing `tool_name(json_args)`, the host parses it and
//! dispatches to the matching tool, and the result is injected as a
//! ```result block.
//!
//! This module defines the **shared interface** that both Signal Deck
//! and signal-ha use.  Each host registers its own tools at startup:
//!
//! ```rust,ignore
//! let mut registry = ToolRegistry::new();
//! registry.register(GetStateTool::new(ha_client.clone()));
//! registry.register(WriteLogTool);
//! // Signal Deck would register different tools:
//! // registry.register(RunPythonTool::new(shell));
//! // registry.register(RenderChartTool::new(card));
//! ```

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};
use tracing::debug;

/// Result of executing a tool call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// The output text (typically JSON).
    pub output: String,
    /// Whether this result represents an error.
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: false }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: true }
    }
}

/// A single tool that the LLM can invoke.
///
/// Implementors capture their own state (HA client, HTTP client, memory
/// handle, etc.) in the struct.  The `execute` method receives only the
/// parsed JSON args from the LLM's tool call.
///
/// # Object safety
///
/// This trait is object-safe (`Box<dyn Tool>`) by returning a pinned
/// boxed future instead of using `async fn`.
pub trait Tool: Send + Sync {
    /// The tool name as the LLM writes it (e.g. "get_state").
    fn name(&self) -> &str;

    /// One-line description for the system prompt.
    fn description(&self) -> &str;

    /// Usage example for the system prompt.
    /// Should look like: `get_state({"entity_id": "sensor.temp"})`
    fn usage(&self) -> &str;

    /// Extended help lines (each prefixed with →) for the system prompt.
    /// Return empty slice if the one-line description is enough.
    fn help_lines(&self) -> &[&str] {
        &[]
    }

    /// Execute the tool with the given JSON args.
    fn execute<'a>(
        &'a self,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
}

/// A collection of registered tools.
///
/// The session loop uses this to dispatch tool calls and generate
/// the tool documentation section of the system prompt.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool.  Panics on duplicate names (programming error).
    pub fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name().to_string();
        assert!(
            !self.tools.iter().any(|t| t.name() == name),
            "Duplicate tool name: {name}"
        );
        self.tools.push(Box::new(tool));
    }

    /// Parse a tool call string and dispatch to the matching tool.
    ///
    /// Call format: `tool_name({"arg": "value"})` or `tool_name()`
    pub async fn dispatch(&self, content: &str) -> ToolResult {
        let content = content.trim();

        let Some(paren_pos) = content.find('(') else {
            return ToolResult::err(format!(
                "Invalid tool call syntax (no opening paren): {content}"
            ));
        };

        let tool_name = content[..paren_pos].trim();

        let args_str = content[paren_pos + 1..]
            .strip_suffix(')')
            .unwrap_or(&content[paren_pos + 1..])
            .trim();

        let args: Value = if args_str.is_empty() {
            json!({})
        } else {
            match serde_json::from_str(args_str) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult::err(format!(
                        "Failed to parse tool args as JSON: {e}\nArgs: {args_str}"
                    ));
                }
            }
        };

        debug!(tool = tool_name, "Dispatching tool call");

        match self.tools.iter().find(|t| t.name() == tool_name) {
            Some(tool) => tool.execute(args).await,
            None => {
                let available: Vec<&str> = self.tools.iter().map(|t| t.name()).collect();
                ToolResult::err(format!(
                    "Unknown tool: {tool_name}\nAvailable: {}",
                    available.join(", ")
                ))
            }
        }
    }

    /// Generate the TOOLS section for the system prompt.
    ///
    /// Each tool gets its usage line + description + optional help lines,
    /// all formatted consistently for the LLM.
    pub fn tool_docs(&self) -> String {
        let mut doc = String::from(
            "TOOLS:\n\
             You interact with the system by writing tool calls in ```tool blocks.\n\
             Each block contains exactly ONE call in the format: tool_name({\"arg\": \"value\"})\n\n\
             Available tools:\n",
        );

        for tool in &self.tools {
            doc.push_str(&format!("\n  {}\n", tool.usage()));
            doc.push_str(&format!("    → {}\n", tool.description()));
            for line in tool.help_lines() {
                doc.push_str(&format!("    → {line}\n"));
            }
        }

        doc
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial test tool.
    struct EchoTool;

    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn description(&self) -> &str { "Echoes the input back." }
        fn usage(&self) -> &str { r#"echo({"text": "hello"})"# }
        fn execute<'a>(
            &'a self,
            args: Value,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async move {
                let text = args["text"].as_str().unwrap_or("(empty)");
                ToolResult::ok(text.to_string())
            })
        }
    }

    #[test]
    fn parse_tool_call_syntax() {
        let content = r#"get_state({"entity_id": "sensor.temp"})"#;
        let paren_pos = content.find('(').unwrap();
        let tool_name = &content[..paren_pos];
        assert_eq!(tool_name, "get_state");

        let args_str = &content[paren_pos + 1..];
        let args_str = args_str.strip_suffix(')').unwrap().trim();
        let args: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(args["entity_id"], "sensor.temp");
    }

    #[test]
    fn parse_no_args() {
        let content = "get_status_page()";
        let paren_pos = content.find('(').unwrap();
        let tool_name = &content[..paren_pos];
        assert_eq!(tool_name, "get_status_page");

        let args_str = content[paren_pos + 1..]
            .strip_suffix(')')
            .unwrap()
            .trim();
        assert!(args_str.is_empty());
    }

    #[tokio::test]
    async fn registry_dispatch() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);

        let result = reg.dispatch(r#"echo({"text": "hello"})"#).await;
        assert!(!result.is_error);
        assert_eq!(result.output, "hello");
    }

    #[tokio::test]
    async fn registry_unknown_tool() {
        let reg = ToolRegistry::new();
        let result = reg.dispatch("nope()").await;
        assert!(result.is_error);
        assert!(result.output.contains("Unknown tool"));
    }

    #[test]
    fn tool_docs_generated() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        let docs = reg.tool_docs();
        assert!(docs.contains("echo"));
        assert!(docs.contains("Echoes the input back"));
    }

    #[test]
    #[should_panic(expected = "Duplicate tool name")]
    fn duplicate_tool_panics() {
        let mut reg = ToolRegistry::new();
        reg.register(EchoTool);
        reg.register(EchoTool);
    }
}
