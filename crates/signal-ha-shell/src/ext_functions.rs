//! External function registry — names registered with Monty at REPL init.
//!
//! When user Python code calls one of these, Monty suspends execution
//! and returns a `ReplProgress::FunctionCall`.  The caller maps it to
//! a host call via [`map_ext_call_to_host_call`](crate::map_ext_call_to_host_call).

/// Names of all external functions available to user Python code.
///
/// These are registered with Monty at REPL init time.  Both short
/// aliases (`state`) and long names (`get_state`) are registered so
/// the LLM can use whichever feels natural.
pub const HA_EXTERNAL_FUNCTIONS: &[&str] = &[
    // ── State ──────────────────────────────────────────────────
    "state",          // short alias
    "states",         // short alias
    "get_state",      // long name
    "get_states",     // long name

    // ── History & statistics ───────────────────────────────────
    "history",        // short alias
    "statistics",     // short alias
    "get_history",    // long name
    "get_statistics", // long name

    // ── Calendar events ────────────────────────────────────────
    "events",
    "get_events",

    // ── Services ───────────────────────────────────────────────
    "call_service",
    "get_services",

    // ── Areas / rooms ──────────────────────────────────────────
    "get_areas",
    "get_area_entities",

    // ── Time ───────────────────────────────────────────────────
    "ago",
    "get_datetime",

    // ── Display ────────────────────────────────────────────────
    "show",

    // ── Logbook ────────────────────────────────────────────────
    "get_logbook",

    // ── Automation traces ──────────────────────────────────────
    "get_trace",
    "list_traces",

    // ── Charting ───────────────────────────────────────────────
    "plot_line",
    "plot_bar",
    "plot_pie",
    "plot_series",

    // ── Semantic layer (annotations) ───────────────────────────
    "annotate",
    "annotations",
    "note",
    "notes",
    "tags",
    "del_annotation",

    // ── House agent (cross-agent access) ───────────────────────
    "read_agent_memory",
    "read_transcript",
    "read_status_page",
    "board_get_all_posts",
];

/// User-facing aliases for the Python API.
///
/// These exist so we can document a clean API (`room("garage")`)
/// while the underlying external function is `get_area_entities`.
/// The aliases are NOT in HA_EXTERNAL_FUNCTIONS — they're mapped
/// at the host call layer.
pub const FUNCTION_ALIASES: &[(&str, &str)] = &[
    ("room", "get_area_entities"),
    ("rooms", "get_areas"),
    ("logbook", "get_logbook"),
    ("now", "get_datetime"),
];
