//! REPL lifecycle — init, feed, start, resume.
//!
//! Wraps Monty's `MontyRepl` with proper error handling, external
//! function suspension, and session state preservation.
//!
//! ## Two execution paths
//!
//! - **`feed_snippet()`** borrows `&mut MontyRepl`.  The REPL is never
//!   consumed, so it survives even on runtime errors.  Returns expression
//!   values directly.  **Cannot** handle external function calls — they
//!   hit `NameError` because `feed()` has no host to resolve names.
//!
//! - **`start_snippet()`** consumes the REPL and returns `ReplEvalResult`,
//!   which can suspend at external calls or report runtime errors — both
//!   variants return the REPL so session state is preserved.
//!
//! The recommended pattern: try `feed_snippet()` first.  If the snippet
//! calls an external function, `feed()` raises `NameError` for the
//! function name — the caller detects this and retries with
//! `start_snippet()`.

use monty::{
    ExtFunctionResult, MontyException, MontyObject, MontyRepl, NameLookupResult,
    NoLimitTracker, PrintWriter, ReplFunctionCall, ReplProgress,
};

use crate::ext_functions::HA_EXTERNAL_FUNCTIONS;

// ── Result type ────────────────────────────────────────────────

/// Outcome of a REPL snippet evaluation or snapshot resume.
#[derive(Debug)]
pub enum ReplEvalResult {
    /// Snippet completed — value and captured print output.
    /// The REPL is returned so it can be stored back in the session.
    Complete {
        repl: MontyRepl<NoLimitTracker>,
        output: String,
        value: Option<MontyObject>,
    },
    /// Snippet suspended at an external function call.
    HostCallNeeded {
        output: String,
        function_name: String,
        args: Vec<MontyObject>,
        kwargs: Vec<(MontyObject, MontyObject)>,
        call: ReplFunctionCall<NoLimitTracker>,
    },
    /// Snippet failed with an error.
    /// The REPL is always returned for runtime errors (via
    /// `ReplStartError`).  `repl: None` only occurs on
    /// syntax/compile errors during `start()` (before execution began).
    Error {
        message: String,
        repl: Option<MontyRepl<NoLimitTracker>>,
    },
}

// ── Known external function names ──────────────────────────────

/// Combined list of known external function names.
/// Used to resolve `NameLookup` during `start()` execution.
static KNOWN_FUNCTIONS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    HA_EXTERNAL_FUNCTIONS.iter().map(|s| s.to_string()).collect()
});

/// Check whether a name is a known external function (HA built-in
/// or registered extra function).
fn is_known_function(name: &str, extra_functions: &[String]) -> bool {
    KNOWN_FUNCTIONS.iter().any(|n| n == name) || extra_functions.iter().any(|n| n == name)
}

/// Check whether a code snippet references any known external function.
///
/// Used to skip the `feed()` fast-path and go straight to `start()`
/// when external calls are present.  This avoids the double-execution
/// issue where `feed()` partially mutates REPL state before failing
/// at the external call, then `start()` re-executes the entire
/// snippet on the already-mutated state.
pub fn code_references_external_fn(code: &str, extra_functions: &[String]) -> bool {
    for name in KNOWN_FUNCTIONS.iter().chain(extra_functions.iter()) {
        // Simple word-boundary check: the function name must appear
        // as a standalone identifier (not as part of a longer word).
        // Check that the char before and after the match is not alphanumeric/underscore.
        let name_bytes = name.as_bytes();
        let code_bytes = code.as_bytes();
        let name_len = name_bytes.len();

        let mut start = 0;
        while let Some(pos) = code[start..].find(name.as_str()) {
            let abs_pos = start + pos;
            let before_ok = abs_pos == 0
                || !code_bytes[abs_pos - 1].is_ascii_alphanumeric()
                    && code_bytes[abs_pos - 1] != b'_';
            let after_pos = abs_pos + name_len;
            let after_ok = after_pos >= code_bytes.len()
                || !code_bytes[after_pos].is_ascii_alphanumeric()
                    && code_bytes[after_pos] != b'_';
            if before_ok && after_ok {
                return true;
            }
            start = abs_pos + 1;
        }
    }
    false
}

// ── REPL lifecycle ─────────────────────────────────────────────

/// Initialise a fresh Monty REPL session.
///
/// `init_code` is compiled and executed once to set up initial state.
/// Pass an empty string for a blank session.
pub fn init_repl(init_code: &str) -> Result<MontyRepl<NoLimitTracker>, String> {
    init_repl_inner(init_code)
}

/// Initialise a REPL with the standard HA functions plus additional
/// custom external functions registered for name lookup.
///
/// Use this when the consumer (e.g. the agent) needs extra builtins
/// like `write_log()` or `schedule_next_session()` that aren't part
/// of the shared API.  The extra names are stored separately and
/// checked during `NameLookup` resolution.
pub fn init_repl_with_functions(
    init_code: &str,
    _extra_functions: &[&str],
) -> Result<MontyRepl<NoLimitTracker>, String> {
    // Extra functions are now handled at NameLookup resolution time
    // rather than at REPL creation.  The caller stores the extra
    // names and passes them to start_snippet_with_extras().
    init_repl_inner(init_code)
}

fn init_repl_inner(
    init_code: &str,
) -> Result<MontyRepl<NoLimitTracker>, String> {
    let mut print = PrintWriter::Collect(String::new());
    let (repl, _init_value) = MontyRepl::new(
        init_code.to_owned(),
        "<signal-deck>",
        vec![],          // no input names
        vec![],          // no input values
        NoLimitTracker,
        &mut print,
    )
    .map_err(|e| format_monty_error(&e))?;
    Ok(repl)
}

/// Execute a snippet using `feed()` — borrows the REPL.
///
/// The REPL is **never lost**, even on runtime errors.  Returns the
/// expression value directly.
///
/// Cannot handle external function calls — `feed()` converts unresolved
/// names to `NameError`.  If the snippet calls `state()`, `show()`, etc.,
/// returns an error containing "NameError".  The caller should detect this
/// (via `is_name_error_for_external_fn()`) and retry with `start_snippet()`.
pub fn feed_snippet(
    repl: &mut MontyRepl<NoLimitTracker>,
    code: &str,
) -> Result<(String, Option<MontyObject>), String> {
    let mut print = PrintWriter::Collect(String::new());
    let value = repl
        .feed(code, &mut print)
        .map_err(|e| format_monty_error(&e))?;
    let output = print.collected_output().unwrap_or("").to_owned();
    let val = if value == MontyObject::None {
        None
    } else {
        Some(value)
    };
    Ok((output, val))
}

/// Check whether a feed() error indicates it should be retried with
/// `start_snippet()`.  This catches both:
/// - "not implemented with standard execution" (external function call in feed mode)
/// - "NameError" for a known external function name (non-call reference)
pub fn is_name_error_for_external_fn(error_msg: &str) -> bool {
    is_name_error_for_external_fn_with_extras(error_msg, &[])
}

/// Like `is_name_error_for_external_fn` but also checks extra function names.
pub fn is_name_error_for_external_fn_with_extras(error_msg: &str, extras: &[String]) -> bool {
    // Direct external function call in feed mode:
    if error_msg.contains("not implemented with standard execution") {
        return true;
    }
    // Non-call reference to an external function name:
    // NameError format: "NameError: name 'state' is not defined"
    if !error_msg.contains("NameError") {
        return false;
    }
    // Check all known function names
    for name in KNOWN_FUNCTIONS.iter().chain(extras.iter()) {
        if error_msg.contains(&format!("'{name}'")) {
            return true;
        }
    }
    false
}

/// Execute a snippet using `start()` — consumes the REPL.
///
/// Required when the snippet calls external functions (`state()`,
/// `history()`, etc.), because only `start()` can suspend at those calls.
///
/// Returns `ReplEvalResult::Error { repl: None }` only for syntax/compile
/// errors before execution begins.  Runtime errors always return the REPL.
pub fn start_snippet(
    repl: MontyRepl<NoLimitTracker>,
    code: &str,
) -> ReplEvalResult {
    start_snippet_with_extras(repl, code, &[])
}

/// Execute a snippet using `start()` with extra function names for lookup.
pub fn start_snippet_with_extras(
    repl: MontyRepl<NoLimitTracker>,
    code: &str,
    extra_functions: &[String],
) -> ReplEvalResult {
    let mut print = PrintWriter::Collect(String::new());
    let progress = repl.start(code, &mut print);
    let output = print.collected_output().unwrap_or("").to_owned();
    match progress {
        Ok(prog) => finish_repl_progress(prog, output, extra_functions),
        Err(e) => {
            // ReplStartError contains the repl + MontyException.
            // Runtime errors preserve the REPL for continued use.
            ReplEvalResult::Error {
                message: format_monty_error(&e.error),
                repl: Some(e.repl),
            }
        }
    }
}

/// Resume a suspended REPL execution with an external result.
pub fn resume_call(
    call: ReplFunctionCall<NoLimitTracker>,
    result: impl Into<ExtFunctionResult>,
) -> ReplEvalResult {
    resume_call_with_extras(call, result, &[])
}

/// Resume a suspended REPL execution with extra function names for lookup.
pub fn resume_call_with_extras(
    call: ReplFunctionCall<NoLimitTracker>,
    result: impl Into<ExtFunctionResult>,
    extra_functions: &[String],
) -> ReplEvalResult {
    let mut print = PrintWriter::Collect(String::new());
    let progress = call.resume(result, &mut print);
    let output = print.collected_output().unwrap_or("").to_owned();
    match progress {
        Ok(prog) => finish_repl_progress(prog, output, extra_functions),
        Err(e) => {
            // Runtime error during resume — REPL is preserved.
            ReplEvalResult::Error {
                message: format_monty_error(&e.error),
                repl: Some(e.repl),
            }
        }
    }
}

// ── Internal helpers ───────────────────────────────────────────

fn finish_repl_progress(
    mut progress: ReplProgress<NoLimitTracker>,
    output: String,
    extra_functions: &[String],
) -> ReplEvalResult {
    // Resolve any NameLookup yields before returning to the caller.
    // Known external function names → MontyObject::Function, unknown → Undefined.
    loop {
        match progress {
            ReplProgress::Complete { repl, value } => {
                let val = if value == MontyObject::None {
                    None
                } else {
                    Some(value)
                };
                return ReplEvalResult::Complete { repl, output, value: val };
            }
            ReplProgress::FunctionCall(call) => {
                let function_name = call.function_name.clone();
                let args = call.args.clone();
                let kwargs = call.kwargs.clone();
                return ReplEvalResult::HostCallNeeded {
                    output,
                    function_name,
                    args,
                    kwargs,
                    call,
                };
            }
            ReplProgress::NameLookup(lookup) => {
                // Resolve known external functions as Function objects.
                // Unknown names become NameError (Undefined).
                let result = if is_known_function(&lookup.name, extra_functions) {
                    NameLookupResult::Value(MontyObject::Function {
                        name: lookup.name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };
                let mut print = PrintWriter::Collect(String::new());
                match lookup.resume(result, &mut print) {
                    Ok(next) => {
                        progress = next;
                        // Continue loop to handle next progress variant.
                    }
                    Err(e) => {
                        return ReplEvalResult::Error {
                            message: format_monty_error(&e.error),
                            repl: Some(e.repl),
                        };
                    }
                }
            }
            ReplProgress::OsCall(_) => {
                return ReplEvalResult::Error {
                    message: "OS calls are not supported.".to_string(),
                    repl: None,
                };
            }
            ReplProgress::ResolveFutures(_) => {
                return ReplEvalResult::Error {
                    message: "Async futures are not supported.".to_string(),
                    repl: None,
                };
            }
        }
    }
}

/// Format a MontyException into a user-friendly error string.
pub fn format_monty_error(err: &MontyException) -> String {
    err.to_string()
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_repl_empty() {
        let repl = init_repl("");
        assert!(repl.is_ok());
    }

    #[test]
    fn test_init_repl_with_code() {
        let repl = init_repl("x = 42");
        assert!(repl.is_ok());
    }

    #[test]
    fn test_init_repl_syntax_error() {
        let result = init_repl("def");
        assert!(result.is_err());
    }

    #[test]
    fn test_init_repl_with_extra_functions() {
        let repl = init_repl_with_functions("", &["write_log", "schedule_next_session"]);
        assert!(repl.is_ok());
    }

    #[test]
    fn test_feed_simple_expression() {
        let mut repl = init_repl("").unwrap();
        let result = feed_snippet(&mut repl, "1 + 2");
        assert!(result.is_ok());
        let (output, value) = result.unwrap();
        assert!(output.is_empty());
        assert_eq!(value, Some(MontyObject::Int(3)));
    }

    #[test]
    fn test_feed_print() {
        let mut repl = init_repl("").unwrap();
        let result = feed_snippet(&mut repl, "print('hello')");
        assert!(result.is_ok());
        let (output, _value) = result.unwrap();
        assert_eq!(output.trim(), "hello");
    }

    #[test]
    fn test_feed_external_function_raises_name_error() {
        // feed() has no host for name resolution — external function calls
        // produce "not implemented with standard execution".  The caller
        // should detect this with is_name_error_for_external_fn() and
        // retry with start_snippet().
        let mut repl = init_repl("").unwrap();
        let result = feed_snippet(&mut repl, "state('sensor.temp')");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("not implemented with standard execution") || err.contains("NameError"),
            "Expected external function error, got: {err}"
        );
        assert!(is_name_error_for_external_fn(&err));
    }

    #[test]
    fn test_is_name_error_for_external_fn() {
        assert!(is_name_error_for_external_fn(
            "NameError: name 'state' is not defined"
        ));
        assert!(is_name_error_for_external_fn(
            "NameError: name 'history' is not defined"
        ));
        // Unknown names are NOT external functions
        assert!(!is_name_error_for_external_fn(
            "NameError: name 'foobar' is not defined"
        ));
        // Non-NameError errors are not external function errors
        assert!(!is_name_error_for_external_fn("ZeroDivisionError: division by zero"));
    }

    #[test]
    fn test_is_name_error_with_extras() {
        assert!(is_name_error_for_external_fn_with_extras(
            "NameError: name 'write_log' is not defined",
            &["write_log".to_string()],
        ));
        assert!(!is_name_error_for_external_fn_with_extras(
            "NameError: name 'write_log' is not defined",
            &[],
        ));
    }

    #[test]
    fn test_start_simple_expression() {
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "1 + 2");
        match result {
            ReplEvalResult::Complete { value, .. } => {
                assert_eq!(value, Some(MontyObject::Int(3)));
            }
            _ => panic!("Expected Complete"),
        }
    }

    #[test]
    fn test_start_external_call_suspends() {
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "state('sensor.temp')");
        match result {
            ReplEvalResult::HostCallNeeded {
                function_name,
                args,
                ..
            } => {
                assert_eq!(function_name, "state");
                assert_eq!(
                    args,
                    vec![MontyObject::String("sensor.temp".to_string())]
                );
            }
            _ => panic!("Expected HostCallNeeded"),
        }
    }

    #[test]
    fn test_start_syntax_error() {
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "if");
        match result {
            ReplEvalResult::Error { message, repl } => {
                assert!(!message.is_empty());
                // ReplStartError preserves the REPL so we can continue
                assert!(repl.is_some());
            }
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn test_resume_completes() {
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "state('sensor.temp')");
        let call = match result {
            ReplEvalResult::HostCallNeeded { call, .. } => call,
            _ => panic!("Expected HostCallNeeded"),
        };

        let fake_value = MontyObject::String("21.5".to_string());
        let resumed = resume_call(call, fake_value);
        match resumed {
            ReplEvalResult::Complete { value, repl, .. } => {
                assert_eq!(value, Some(MontyObject::String("21.5".to_string())));
                // REPL is recoverable
                assert!(matches!(
                    start_snippet(repl, "1"),
                    ReplEvalResult::Complete { .. }
                ));
            }
            _ => panic!("Expected Complete after resume"),
        }
    }

    #[test]
    fn test_variable_persists_across_snippets() {
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "x = 42");
        let repl = match result {
            ReplEvalResult::Complete { repl, .. } => repl,
            _ => panic!("Expected Complete"),
        };
        let result = start_snippet(repl, "x + 1");
        match result {
            ReplEvalResult::Complete { value, .. } => {
                assert_eq!(value, Some(MontyObject::Int(43)));
            }
            _ => panic!("Expected Complete"),
        }
    }

    #[test]
    fn test_error_does_not_corrupt_context() {
        let mut repl = init_repl("").unwrap();
        // Successful assignment via feed
        let _ = feed_snippet(&mut repl, "x = 10");
        // Error — should not corrupt
        let err = feed_snippet(&mut repl, "y = 1/0");
        assert!(err.is_err());
        // x should still be accessible
        let result = feed_snippet(&mut repl, "print(x)");
        assert!(result.is_ok());
        let (output, _) = result.unwrap();
        assert!(output.contains("10"));
    }

    #[test]
    fn test_extra_function_suspends() {
        let repl = init_repl_with_functions("", &["write_log"]).unwrap();
        let extras = vec!["write_log".to_string()];
        let result = start_snippet_with_extras(repl, "write_log('hello')", &extras);
        match result {
            ReplEvalResult::HostCallNeeded { function_name, .. } => {
                assert_eq!(function_name, "write_log");
            }
            _ => panic!("Expected HostCallNeeded for custom function"),
        }
    }

    #[test]
    fn test_unknown_name_in_non_call_raises_error() {
        // Names that are NOT known external functions and NOT in call
        // position should produce NameError via NameLookup → Undefined.
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "x = totally_unknown");
        match result {
            ReplEvalResult::Error { message, .. } => {
                assert!(message.contains("NameError"), "Expected NameError, got: {message}");
            }
            _ => panic!("Expected Error for unknown non-call name"),
        }
    }

    #[test]
    fn test_unknown_function_call_yields_host_call() {
        // In call position, unknown names go directly to FunctionCall
        // (via LoadGlobalCallable) — the engine/caller decides what to do.
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "totally_unknown()");
        match result {
            ReplEvalResult::HostCallNeeded { function_name, .. } => {
                assert_eq!(function_name, "totally_unknown");
            }
            other => panic!("Expected HostCallNeeded, got: {other:?}"),
        }
    }

    #[test]
    fn test_name_lookup_resolves_known_function() {
        // Non-call usage of a known external function should resolve
        // (e.g. assigning to a variable).  The resolved function can
        // then be called.
        let repl = init_repl("").unwrap();
        let result = start_snippet(repl, "f = state\nf('sensor.temp')");
        match result {
            ReplEvalResult::HostCallNeeded {
                function_name,
                args,
                ..
            } => {
                assert_eq!(function_name, "state");
                assert_eq!(
                    args,
                    vec![MontyObject::String("sensor.temp".to_string())]
                );
            }
            other => panic!("Expected HostCallNeeded, got: {other:?}"),
        }
    }

    #[test]
    fn test_code_references_external_fn_basic() {
        let extras = vec![];
        assert!(code_references_external_fn("state('sensor.temp')", &extras));
        assert!(code_references_external_fn("x = get_area_entities('garage')", &extras));
        assert!(code_references_external_fn("history('light.x', hours=24)", &extras));
    }

    #[test]
    fn test_code_references_external_fn_no_false_positives() {
        let extras = vec![];
        // Should not match substrings
        assert!(!code_references_external_fn("my_state = 42", &extras));
        assert!(!code_references_external_fn("x = get_states_count", &extras));
        assert!(!code_references_external_fn("for_state = True", &extras));
    }

    #[test]
    fn test_code_references_external_fn_extras() {
        let extras = vec!["write_log".to_string()];
        assert!(code_references_external_fn("write_log('hello')", &extras));
        assert!(!code_references_external_fn("x = 1 + 2", &extras));
    }

    #[test]
    fn test_code_references_external_fn_pure_code() {
        let extras = vec![];
        // Pure arithmetic / string code should not trigger
        assert!(!code_references_external_fn("x = 1 + 2\nprint(x)", &extras));
        assert!(!code_references_external_fn("for i in range(10):\n    print(i)", &extras));
    }
}
