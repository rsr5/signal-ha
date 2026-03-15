//! Markdown parser for the agent loop.
//!
//! Ported from Signal Deck's `parser.ts`.  Follows the markdown-agent
//! pattern:
//!
//! - LLM writes ```tool fenced code blocks for tools it wants executed
//! - After execution, a ```result block is appended right after
//! - On subsequent turns the LLM sees both its tool calls *and* the output
//! - Hallucinated ```result blocks from the LLM are stripped before parsing

use std::fmt;

/// A single fenced code block extracted from a markdown document.
#[derive(Debug, Clone)]
pub struct CodeBlock {
    /// Language tag (e.g. "tool", "signal-deck").
    pub language: String,
    /// The code/content inside the fences.
    pub content: String,
    /// 0-indexed line where the opening fence is.
    pub start_line: usize,
    /// 0-indexed line where the closing fence is.
    pub end_line: usize,
    /// True if a ```result block already follows this block.
    pub has_result: bool,
}

/// A parsed markdown document with extracted code blocks.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    pub lines: Vec<String>,
    pub blocks: Vec<CodeBlock>,
}

impl fmt::Display for ParsedDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.lines.join("\n"))
    }
}

/// Languages we treat as executable code blocks.
///
/// - "signal-deck" is the Python runtime (same as Signal Deck's WASM engine)
/// - "tool" is kept for backwards compatibility with existing prompts
const EXECUTABLE_LANGUAGES: &[&str] = &["signal-deck", "tool"];

/// Language tag for injected results.
const RESULT_TAG: &str = "result";

/// Get executable blocks — those in executable languages without existing results.
pub fn get_executable_blocks(doc: &ParsedDocument) -> Vec<&CodeBlock> {
    doc.blocks
        .iter()
        .filter(|b| EXECUTABLE_LANGUAGES.contains(&b.language.as_str()) && !b.has_result)
        .collect()
}

/// Strip any ```result ... ``` blocks from markdown.
///
/// This is the key defense against hallucinated results — the LLM might
/// write its own ```result blocks, but we throw them away and only inject
/// real execution output.
pub fn strip_result_blocks(markdown: &str) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut cleaned: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        if let Some(lang) = parse_fence_open(lines[i]) {
            if lang == RESULT_TAG {
                // Skip this entire result block (opening fence → closing fence)
                let mut j = i + 1;
                while j < lines.len() {
                    if is_fence_close(lines[j]) {
                        break;
                    }
                    j += 1;
                }
                // Jump past the closing fence (or end of file if unclosed)
                i = j + 1;
                continue;
            }
        }
        cleaned.push(lines[i]);
        i += 1;
    }

    cleaned.join("\n")
}

/// Parse a markdown string into a ParsedDocument with extracted code blocks.
///
/// If `sanitize` is true (default), any ```result blocks in the input are
/// stripped first — this removes hallucinated results from LLM output.
pub fn parse(markdown: &str, sanitize: bool) -> ParsedDocument {
    let markdown = if sanitize {
        strip_result_blocks(markdown)
    } else {
        markdown.to_string()
    };

    let lines: Vec<String> = markdown.lines().map(String::from).collect();
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        if let Some(lang) = parse_fence_open(&lines[i]) {
            let start = i;

            // Scan forward for the closing fence
            let mut j = i + 1;
            while j < lines.len() {
                if is_fence_close(&lines[j]) {
                    break;
                }
                j += 1;
            }

            if j >= lines.len() {
                // No closing fence found — skip this opening fence
                i += 1;
                continue;
            }

            let end = j;
            let content = lines[start + 1..end].join("\n");

            // Check if a result block immediately follows
            let has_result = if end + 1 < lines.len() {
                parse_fence_open(&lines[end + 1])
                    .map(|l| l == RESULT_TAG)
                    .unwrap_or(false)
            } else {
                false
            };

            blocks.push(CodeBlock {
                language: lang.to_string(),
                content,
                start_line: start,
                end_line: end,
                has_result,
            });

            i = end + 1;
        } else {
            i += 1;
        }
    }

    ParsedDocument { lines, blocks }
}

/// Inject an execution result immediately after a code block.
///
/// Returns a new ParsedDocument with the result block inserted and
/// all line references updated (via re-parse).
pub fn inject_result(doc: &ParsedDocument, block: &CodeBlock, result: &str) -> ParsedDocument {
    let result_lines = vec![
        format!("```{RESULT_TAG}"),
        result.trim_end_matches('\n').to_string(),
        "```".to_string(),
    ];

    // Insert after the closing fence of the code block
    let insert_at = block.end_line + 1;
    let mut new_lines = Vec::with_capacity(doc.lines.len() + result_lines.len());
    new_lines.extend_from_slice(&doc.lines[..insert_at]);
    new_lines.extend(result_lines);
    if insert_at < doc.lines.len() {
        new_lines.extend_from_slice(&doc.lines[insert_at..]);
    }

    // Re-parse to get correct line numbers (don't sanitize — these are real results)
    parse(&new_lines.join("\n"), false)
}

// ── Internal helpers ───────────────────────────────────────────

/// Match an opening code fence: ```lang
fn parse_fence_open(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("```") {
        return None;
    }
    let after = trimmed.trim_start_matches('`');
    let lang = after.trim();
    if lang.is_empty() || lang.contains(' ') || lang.contains('`') {
        return None;
    }
    // Must be an alphanumeric language tag
    if lang.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        Some(lang)
    } else {
        None
    }
}

/// Match a closing code fence: ```
fn is_fence_close(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "```"
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_tool_block() {
        let md = r#"Some text

```tool
get_state("sensor.temp")
```

More text"#;

        let doc = parse(md, true);
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(doc.blocks[0].language, "tool");
        assert_eq!(doc.blocks[0].content, r#"get_state("sensor.temp")"#);
        assert!(!doc.blocks[0].has_result);
    }

    #[test]
    fn executable_blocks_skips_result() {
        let md = r#"```tool
get_state("sensor.temp")
```
```result
{"state": "22.5"}
```"#;

        let doc = parse(md, false);
        assert_eq!(doc.blocks.len(), 2);
        assert!(doc.blocks[0].has_result);

        let executable = get_executable_blocks(&doc);
        assert_eq!(executable.len(), 0);
    }

    #[test]
    fn strip_hallucinated_results() {
        let md = r#"I'll check the temperature.

```tool
get_state("sensor.temp")
```
```result
This is hallucinated
```"#;

        let doc = parse(md, true); // sanitize=true strips result blocks
        let executable = get_executable_blocks(&doc);
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].content, r#"get_state("sensor.temp")"#);
    }

    #[test]
    fn inject_result_into_document() {
        let md = r#"```tool
get_state("sensor.temp")
```

More text"#;

        let doc = parse(md, true);
        let block = &doc.blocks[0];
        let new_doc = inject_result(&doc, block, r#"{"state": "22.5"}"#);

        let text = new_doc.to_string();
        assert!(text.contains("```result"));
        assert!(text.contains(r#"{"state": "22.5"}"#));
        assert!(text.contains("More text"));

        // The tool block should now have a result
        assert!(new_doc.blocks[0].has_result);
        assert_eq!(get_executable_blocks(&new_doc).len(), 0);
    }

    #[test]
    fn multiple_tool_blocks() {
        let md = r#"```tool
get_state("sensor.a")
```

```tool
get_state("sensor.b")
```"#;

        let doc = parse(md, true);
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(get_executable_blocks(&doc).len(), 2);
    }

    #[test]
    fn unclosed_fence_ignored() {
        let md = "```tool\nsome content\nno closing fence";
        let doc = parse(md, true);
        assert_eq!(doc.blocks.len(), 0);
    }

    #[test]
    fn non_tool_blocks_ignored() {
        let md = r#"```python
print("hello")
```

```json
{"key": "value"}
```"#;

        let doc = parse(md, true);
        assert_eq!(doc.blocks.len(), 2);
        // Neither python nor json is in EXECUTABLE_LANGUAGES
        assert_eq!(get_executable_blocks(&doc).len(), 0);
    }
}
