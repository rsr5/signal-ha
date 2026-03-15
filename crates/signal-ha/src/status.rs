//! Status page — lightweight HTTP endpoint for automation diagnostics.
//!
//! Each automation binary gets its own status page on a configurable port.
//! Serves HTML (dark theme) by default, JSON with `?format=json` or
//! `Accept: application/json`.
//!
//! # Typed values
//!
//! Use typed setters for richer rendering:
//!
//! - [`set`] — plain text (default)
//! - [`set_bool`] — on / off badge
//! - [`set_score`] — accent-coloured floating-point number
//! - [`set_int`] — plain integer
//! - [`set_enum`] — pill strip showing all variants with the active one highlighted
//! - [`set_countdown`] — progress bar countdown timer (drains over time)
//!
//! # Usage
//!
//! ```rust,no_run
//! use signal_ha::StatusPage;
//!
//! #[tokio::main]
//! async fn main() {
//!     let status = StatusPage::new("office-lights", 9101);
//!     status.set("presence", "reason", "decaying");
//!     status.set_bool("presence", "occupied", true);
//!     status.set_score("presence", "score", 42.0);
//!     status.set_enum("activity", "label", "working",
//!                     &["vacant", "working", "relaxing", "watching_tv"]);
//!     status.spawn(); // background axum task
//! }
//! ```

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::net::TcpListener;
use tracing::{error, info};

// ── Public types ───────────────────────────────────────────────────

/// A typed value stored in the status page.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusValue {
    /// Plain text.
    Text(String),
    /// Boolean on/off.
    Bool(bool),
    /// Floating-point score.
    Score(f64),
    /// Integer.
    Int(i64),
    /// Enum — active value plus the full list of variants.
    Enum {
        value: String,
        variants: Vec<String>,
    },
    /// Countdown timer — renders as a progress bar draining from full to empty.
    ///
    /// `remaining_secs` is the seconds left right now.
    /// `total_secs` is the original timer duration (for computing the progress %).
    ///
    /// The status page auto-refreshes every 5 s (server-side truth), but a
    /// tiny client-side JS ticker updates the bar and time display every
    /// second for smooth visual feedback.
    Countdown {
        remaining_secs: u64,
        total_secs: u64,
    },
}

/// Shared status page state.
///
/// Clone-cheap (inner `Arc`). Call [`set`], [`set_bool`], [`set_score`],
/// [`set_int`], [`set_enum`], [`set_many`] from your evaluation loop,
/// then [`spawn`] once at startup.
#[derive(Clone)]
pub struct StatusPage {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    port: u16,
    started_at: Instant,
    state: RwLock<PageState>,
}

struct PageState {
    eval_count: u64,
    last_eval: Option<Instant>,
    /// section name → (key → typed value)
    sections: BTreeMap<String, BTreeMap<String, StatusValue>>,
    /// Ordered list of section names (preserves insertion order).
    section_order: Vec<String>,
}

impl StatusPage {
    /// Create a new status page for the given automation name and port.
    pub fn new(name: &str, port: u16) -> Self {
        Self {
            inner: Arc::new(Inner {
                name: name.to_string(),
                port,
                started_at: Instant::now(),
                state: RwLock::new(PageState {
                    eval_count: 0,
                    last_eval: None,
                    sections: BTreeMap::new(),
                    section_order: Vec::new(),
                }),
            }),
        }
    }

    // ── Internal helpers ───────────────────────────────────────────

    fn ensure_section(state: &mut PageState, section: &str) {
        if !state.sections.contains_key(section) {
            state.section_order.push(section.to_string());
            state.sections.insert(section.to_string(), BTreeMap::new());
        }
    }

    fn put(&self, section: &str, key: &str, value: StatusValue) {
        let mut state = self.inner.state.write().unwrap();
        Self::ensure_section(&mut state, section);
        state
            .sections
            .get_mut(section)
            .unwrap()
            .insert(key.to_string(), value);
    }

    // ── Typed setters ──────────────────────────────────────────────

    /// Set a plain text value.
    pub fn set(&self, section: &str, key: &str, value: &str) {
        self.put(section, key, StatusValue::Text(value.to_string()));
    }

    /// Set a boolean value (renders as on/off badge).
    pub fn set_bool(&self, section: &str, key: &str, value: bool) {
        self.put(section, key, StatusValue::Bool(value));
    }

    /// Set a floating-point score (renders in accent colour).
    pub fn set_score(&self, section: &str, key: &str, value: f64) {
        self.put(section, key, StatusValue::Score(value));
    }

    /// Set an integer value.
    pub fn set_int(&self, section: &str, key: &str, value: i64) {
        self.put(section, key, StatusValue::Int(value));
    }

    /// Set an enum value — renders all variants as a pill strip with the
    /// active variant highlighted.
    pub fn set_enum(&self, section: &str, key: &str, value: &str, variants: &[&str]) {
        self.put(
            section,
            key,
            StatusValue::Enum {
                value: value.to_string(),
                variants: variants.iter().map(|s| s.to_string()).collect(),
            },
        );
    }

    /// Set a countdown timer — renders as a progress bar that drains over time.
    ///
    /// `remaining_secs` is how many seconds are left right now.
    /// `total_secs` is the original full duration (for the progress bar %).
    ///
    /// Call this on every status update to keep the server-side value fresh.
    /// The client-side JS ticker smooths animation between 5 s refreshes.
    pub fn set_countdown(&self, section: &str, key: &str, remaining_secs: u64, total_secs: u64) {
        self.put(
            section,
            key,
            StatusValue::Countdown {
                remaining_secs,
                total_secs,
            },
        );
    }

    /// Clear a countdown (timer expired / inactive) — shows "—" as plain text.
    pub fn clear_countdown(&self, section: &str, key: &str) {
        self.put(section, key, StatusValue::Text("—".to_string()));
    }

    /// Set multiple key-value pairs (plain text) in a named section at once.
    pub fn set_many(&self, section: &str, pairs: &[(&str, String)]) {
        let mut state = self.inner.state.write().unwrap();
        Self::ensure_section(&mut state, section);
        let sec = state.sections.get_mut(section).unwrap();
        for (k, v) in pairs {
            sec.insert(k.to_string(), StatusValue::Text(v.clone()));
        }
    }

    /// Record that an evaluation cycle completed.
    pub fn tick(&self) {
        let mut state = self.inner.state.write().unwrap();
        state.eval_count += 1;
        state.last_eval = Some(Instant::now());
    }

    /// Spawn the status HTTP server as a background tokio task.
    pub fn spawn(&self) {
        let page = self.clone();
        let port = self.inner.port;
        let name = self.inner.name.clone();
        tokio::spawn(async move {
            let app = Router::new()
                .route("/", get(handle_status))
                .route("/status", get(handle_status))
                .with_state(page);

            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    info!(name = %name, port, "Status page listening");
                    if let Err(e) = axum::serve(listener, app).await {
                        error!(?e, "Status server error");
                    }
                }
                Err(e) => {
                    error!(?e, port, "Failed to bind status page");
                }
            }
        });
    }
}

// ── Handler ────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct StatusQuery {
    format: Option<String>,
}

async fn handle_status(
    State(page): State<StatusPage>,
    Query(query): Query<StatusQuery>,
    headers: HeaderMap,
) -> Response {
    let wants_json = query.format.as_deref() == Some("json")
        || headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("application/json"))
            .unwrap_or(false);

    let inner = &page.inner;
    let state = inner.state.read().unwrap();

    let uptime_secs = inner.started_at.elapsed().as_secs();
    let last_eval_ago = state
        .last_eval
        .map(|t| format!("{}s ago", t.elapsed().as_secs()))
        .unwrap_or_else(|| "never".to_string());

    if wants_json {
        let mut sections = serde_json::Map::new();
        for name in &state.section_order {
            if let Some(sec) = state.sections.get(name) {
                let obj: serde_json::Map<String, Value> = sec
                    .iter()
                    .map(|(k, v)| (k.clone(), status_value_to_json(v)))
                    .collect();
                sections.insert(name.clone(), Value::Object(obj));
            }
        }
        let body = json!({
            "name": inner.name,
            "uptime_seconds": uptime_secs,
            "eval_count": state.eval_count,
            "last_eval": last_eval_ago,
            "sections": sections,
        });
        axum::Json(body).into_response()
    } else {
        Html(render_html(
            &inner.name,
            uptime_secs,
            state.eval_count,
            &last_eval_ago,
            &state.section_order,
            &state.sections,
        ))
        .into_response()
    }
}

// ── JSON helpers ───────────────────────────────────────────────────

fn status_value_to_json(v: &StatusValue) -> Value {
    match v {
        StatusValue::Text(s) => json!(s),
        StatusValue::Bool(b) => json!(b),
        StatusValue::Score(f) => json!(f),
        StatusValue::Int(i) => json!(i),
        StatusValue::Enum { value, variants } => {
            json!({ "value": value, "variants": variants })
        }
        StatusValue::Countdown {
            remaining_secs,
            total_secs,
        } => {
            json!({
                "remaining_secs": remaining_secs,
                "total_secs": total_secs,
            })
        }
    }
}

// ── HTML rendering (dark "incident detected" theme) ────────────────

fn render_html(
    name: &str,
    uptime_secs: u64,
    eval_count: u64,
    last_eval: &str,
    section_order: &[String],
    sections: &BTreeMap<String, BTreeMap<String, StatusValue>>,
) -> String {
    let uptime = format_uptime(uptime_secs);
    let mut html = String::with_capacity(4096);

    let _ = write!(
        html,
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<meta http-equiv="refresh" content="5"/>
<title>{name} — status</title>
<style>
:root {{
  --bg: #0a0a0a;
  --surface: #111111;
  --card: #141414;
  --accent: #6b9a8a;
  --text: #a3a3a3;
  --text-dim: #606060;
  --border: #1a1a1a;
  --radius: 8px;
}}

* {{ margin: 0; padding: 0; box-sizing: border-box; }}

body {{
  font-family: 'Iosevka Web','Iosevka','Consolas','SF Mono',monospace;
  background: var(--bg);
  color: var(--text);
  min-height: 100vh;
  padding: 3rem 2rem;
}}

.container {{ max-width: 720px; margin: 0 auto; }}

header {{
  margin-bottom: 2rem;
  padding-bottom: 1rem;
  border-bottom: 1px solid var(--border);
}}

header h1 {{
  font-size: 1.3rem;
  font-weight: 500;
  letter-spacing: -0.02em;
  color: var(--text);
}}

header h1 span {{ color: var(--accent); }}

.meta {{
  display: flex;
  gap: 2rem;
  margin-top: 0.5rem;
  font-size: 0.75rem;
  color: var(--text-dim);
}}

.meta .val {{ color: var(--accent); }}

section {{
  margin-bottom: 1.5rem;
}}

section h2 {{
  font-size: 0.7rem;
  text-transform: uppercase;
  letter-spacing: 0.12em;
  color: var(--text-dim);
  margin-bottom: 0.5rem;
  padding-bottom: 0.3rem;
  border-bottom: 1px solid var(--border);
}}

table {{
  width: 100%;
  border-collapse: collapse;
}}

tr {{
  border-bottom: 1px solid var(--border);
}}

tr:last-child {{ border-bottom: none; }}

td {{
  padding: 0.35rem 0.6rem;
  font-size: 0.82rem;
}}

td:first-child {{
  color: var(--text-dim);
  width: 40%;
  white-space: nowrap;
}}

td:last-child {{
  color: var(--text);
  word-break: break-word;
}}

.val-on {{ color: #4ade80; }}
.val-off {{ color: var(--text-dim); }}
.val-score {{ color: var(--accent); }}

.enum-strip {{
  display: inline-flex;
  flex-wrap: wrap;
  gap: 2px;
  max-width: 100%;
}}

.enum-strip .pill {{
  padding: 0.15rem 0.5rem;
  font-size: 0.75rem;
  color: var(--text-dim);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 3px;
}}

.enum-strip .pill.active {{
  color: var(--bg);
  background: var(--accent);
  font-weight: 600;
  border-color: var(--accent);
}}

footer {{
  margin-top: 3rem;
  font-size: 0.7rem;
  color: var(--text-dim);
  text-align: center;
}}

@media (max-width: 600px) {{
  body {{ padding: 1.5rem 1rem; }}
  .meta {{ flex-direction: column; gap: 0.3rem; }}
}}

.countdown {{
  display: flex;
  align-items: center;
  gap: 0.6rem;
}}

.countdown-bar {{
  flex: 1;
  height: 6px;
  background: var(--surface);
  border-radius: 3px;
  overflow: hidden;
  border: 1px solid var(--border);
  min-width: 120px;
}}

.countdown-fill {{
  height: 100%;
  background: var(--accent);
  border-radius: 3px;
  transition: width 1s linear;
}}

.countdown-time {{
  color: var(--accent);
  font-size: 0.82rem;
  font-variant-numeric: tabular-nums;
  min-width: 4ch;
  text-align: right;
}}
</style>
</head>
<body>
<div class="container">
<header>
  <h1><span>●</span> {name}</h1>
  <div class="meta">
    <span>uptime <span class="val">{uptime}</span></span>
    <span>evals <span class="val">{eval_count}</span></span>
    <span>last eval <span class="val">{last_eval}</span></span>
  </div>
</header>
"#
    );

    for sec_name in section_order {
        if let Some(sec) = sections.get(sec_name) {
            let _ = write!(
                html,
                "<section>\n<h2>{sec_name}</h2>\n<table>\n"
            );
            for (key, value) in sec {
                let _ = write!(
                    html,
                    "<tr><td>{key}</td><td>{}</td></tr>\n",
                    render_value(value)
                );
            }
            let _ = write!(html, "</table>\n</section>\n");
        }
    }

    let _ = write!(
        html,
        r#"<footer>signal-ha · auto-refresh 5s · <a href="?format=json" style="color:var(--accent)">json</a></footer>
</div>
<script>
(function(){{
  var els=document.querySelectorAll('.countdown');
  if(!els.length) return;
  setInterval(function(){{
    els.forEach(function(el){{
      var r=parseInt(el.dataset.remaining,10);
      var t=parseInt(el.dataset.total,10);
      if(r>0) r--;
      el.dataset.remaining=r;
      var pct=t>0?(r/t*100):0;
      var fill=el.querySelector('.countdown-fill');
      if(fill) fill.style.width=pct.toFixed(1)+'%';
      var time=el.querySelector('.countdown-time');
      if(time) time.textContent=Math.floor(r/60)+':'+(r%60<10?'0':'')+r%60;
    }});
  }},1000);
}})();
</script>
</body>
</html>"#
    );

    html
}

/// Render a StatusValue as an HTML fragment.
fn render_value(value: &StatusValue) -> String {
    match value {
        StatusValue::Text(s) => {
            let css = text_value_class(s);
            if css.is_empty() {
                html_escape(s)
            } else {
                format!("<span class=\"{css}\">{}</span>", html_escape(s))
            }
        }
        StatusValue::Bool(b) => {
            if *b {
                "<span class=\"val-on\">on</span>".to_string()
            } else {
                "<span class=\"val-off\">off</span>".to_string()
            }
        }
        StatusValue::Score(f) => {
            format!("<span class=\"val-score\">{f:.1}</span>")
        }
        StatusValue::Int(i) => {
            format!("<span class=\"val-score\">{i}</span>")
        }
        StatusValue::Enum { value, variants } => {
            let mut out = String::from("<div class=\"enum-strip\">");
            for v in variants {
                let active = if v == value { " active" } else { "" };
                let _ = write!(
                    out,
                    "<span class=\"pill{active}\">{}</span>",
                    html_escape(v)
                );
            }
            out.push_str("</div>");
            out
        }
        StatusValue::Countdown {
            remaining_secs,
            total_secs,
        } => {
            let pct = if *total_secs > 0 {
                (*remaining_secs as f64 / *total_secs as f64 * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let mins = remaining_secs / 60;
            let secs = remaining_secs % 60;
            // data attributes drive the client-side JS ticker
            format!(
                r#"<div class="countdown" data-remaining="{remaining_secs}" data-total="{total_secs}"><div class="countdown-bar"><div class="countdown-fill" style="width:{pct:.1}%"></div></div><span class="countdown-time">{mins}:{secs:02}</span></div>"#
            )
        }
    }
}

/// CSS class for plain text values (heuristic fallback for `set()`).
fn text_value_class(value: &str) -> &'static str {
    let lower = value.to_lowercase();
    match lower.as_str() {
        "on" | "true" | "yes" | "occupied" => "val-on",
        "off" | "false" | "no" | "vacant" => "val-off",
        _ => {
            if lower.parse::<f64>().is_ok() {
                "val-score"
            } else {
                ""
            }
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_read_back() {
        let page = StatusPage::new("test", 0);
        page.set("inputs", "pir", "on");
        page.set("inputs", "tv", "off");
        page.set("presence", "score", "42.0");

        let state = page.inner.state.read().unwrap();
        assert_eq!(
            state.sections["inputs"]["pir"],
            StatusValue::Text("on".into())
        );
        assert_eq!(
            state.sections["inputs"]["tv"],
            StatusValue::Text("off".into())
        );
        assert_eq!(
            state.sections["presence"]["score"],
            StatusValue::Text("42.0".into())
        );
        assert_eq!(state.section_order, vec!["inputs", "presence"]);
    }

    #[test]
    fn set_many_works() {
        let page = StatusPage::new("test", 0);
        page.set_many(
            "scores",
            &[
                ("working", "80.0".to_string()),
                ("relaxing", "0.0".to_string()),
            ],
        );

        let state = page.inner.state.read().unwrap();
        assert_eq!(
            state.sections["scores"]["working"],
            StatusValue::Text("80.0".into())
        );
        assert_eq!(
            state.sections["scores"]["relaxing"],
            StatusValue::Text("0.0".into())
        );
    }

    #[test]
    fn typed_setters() {
        let page = StatusPage::new("test", 0);
        page.set_bool("inputs", "pir", true);
        page.set_score("presence", "score", 42.5);
        page.set_int("stats", "count", 7);
        page.set_enum("activity", "label", "working", &["vacant", "working", "relaxing"]);

        let state = page.inner.state.read().unwrap();
        assert_eq!(state.sections["inputs"]["pir"], StatusValue::Bool(true));
        assert_eq!(
            state.sections["presence"]["score"],
            StatusValue::Score(42.5)
        );
        assert_eq!(state.sections["stats"]["count"], StatusValue::Int(7));
        assert_eq!(
            state.sections["activity"]["label"],
            StatusValue::Enum {
                value: "working".into(),
                variants: vec!["vacant".into(), "working".into(), "relaxing".into()],
            }
        );
    }

    #[test]
    fn tick_increments_eval_count() {
        let page = StatusPage::new("test", 0);
        assert_eq!(page.inner.state.read().unwrap().eval_count, 0);
        page.tick();
        page.tick();
        page.tick();
        assert_eq!(page.inner.state.read().unwrap().eval_count, 3);
    }

    #[test]
    fn section_order_preserved() {
        let page = StatusPage::new("test", 0);
        page.set("zebra", "a", "1");
        page.set("alpha", "b", "2");
        page.set("middle", "c", "3");
        // Updating existing section shouldn't change order.
        page.set("zebra", "d", "4");

        let state = page.inner.state.read().unwrap();
        assert_eq!(state.section_order, vec!["zebra", "alpha", "middle"]);
    }

    #[test]
    fn text_value_classes() {
        assert_eq!(text_value_class("on"), "val-on");
        assert_eq!(text_value_class("On"), "val-on");
        assert_eq!(text_value_class("true"), "val-on");
        assert_eq!(text_value_class("off"), "val-off");
        assert_eq!(text_value_class("false"), "val-off");
        assert_eq!(text_value_class("42.0"), "val-score");
        assert_eq!(text_value_class("working"), "");
    }

    #[test]
    fn format_uptime_ranges() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(65), "1m 5s");
        assert_eq!(format_uptime(3665), "1h 1m 5s");
        assert_eq!(format_uptime(90061), "1d 1h 1m");
    }

    #[test]
    fn render_bool_values() {
        let on = render_value(&StatusValue::Bool(true));
        assert!(on.contains("val-on"));
        assert!(on.contains("on"));

        let off = render_value(&StatusValue::Bool(false));
        assert!(off.contains("val-off"));
        assert!(off.contains("off"));
    }

    #[test]
    fn render_score_value() {
        let html = render_value(&StatusValue::Score(42.567));
        assert!(html.contains("val-score"));
        assert!(html.contains("42.6")); // .1 precision
    }

    #[test]
    fn render_int_value() {
        let html = render_value(&StatusValue::Int(7));
        assert!(html.contains("val-score"));
        assert!(html.contains("7"));
    }

    #[test]
    fn render_enum_value() {
        let html = render_value(&StatusValue::Enum {
            value: "working".into(),
            variants: vec!["vacant".into(), "working".into(), "relaxing".into()],
        });
        assert!(html.contains("enum-strip"));
        assert!(html.contains("<span class=\"pill\">vacant</span>"));
        assert!(html.contains("<span class=\"pill active\">working</span>"));
        assert!(html.contains("<span class=\"pill\">relaxing</span>"));
    }

    #[test]
    fn html_render_contains_sections() {
        let mut sections = BTreeMap::new();
        let mut inputs = BTreeMap::new();
        inputs.insert("pir".to_string(), StatusValue::Bool(true));
        sections.insert("inputs".to_string(), inputs);
        let order = vec!["inputs".to_string()];

        let html = render_html("test-auto", 120, 5, "2s ago", &order, &sections);
        assert!(html.contains("test-auto"));
        assert!(html.contains("inputs"));
        assert!(html.contains("pir"));
        assert!(html.contains("val-on"));
        assert!(html.contains("auto-refresh 5s"));
    }

    #[test]
    fn json_serialization() {
        assert_eq!(
            status_value_to_json(&StatusValue::Text("hello".into())),
            json!("hello")
        );
        assert_eq!(
            status_value_to_json(&StatusValue::Bool(true)),
            json!(true)
        );
        assert_eq!(
            status_value_to_json(&StatusValue::Score(3.14)),
            json!(3.14)
        );
        assert_eq!(status_value_to_json(&StatusValue::Int(42)), json!(42));
        assert_eq!(
            status_value_to_json(&StatusValue::Enum {
                value: "a".into(),
                variants: vec!["a".into(), "b".into()],
            }),
            json!({"value": "a", "variants": ["a", "b"]})
        );
        assert_eq!(
            status_value_to_json(&StatusValue::Countdown {
                remaining_secs: 300,
                total_secs: 600,
            }),
            json!({"remaining_secs": 300, "total_secs": 600})
        );
    }

    #[test]
    fn typed_countdown_setter() {
        let page = StatusPage::new("test", 0);
        page.set_countdown("state", "timer", 300, 600);

        let state = page.inner.state.read().unwrap();
        assert_eq!(
            state.sections["state"]["timer"],
            StatusValue::Countdown {
                remaining_secs: 300,
                total_secs: 600,
            }
        );
    }

    #[test]
    fn clear_countdown_sets_text_dash() {
        let page = StatusPage::new("test", 0);
        page.set_countdown("state", "timer", 300, 600);
        page.clear_countdown("state", "timer");

        let state = page.inner.state.read().unwrap();
        assert_eq!(
            state.sections["state"]["timer"],
            StatusValue::Text("—".into())
        );
    }

    #[test]
    fn render_countdown_value() {
        let html = render_value(&StatusValue::Countdown {
            remaining_secs: 325,
            total_secs: 600,
        });
        assert!(html.contains("countdown"));
        assert!(html.contains("countdown-bar"));
        assert!(html.contains("countdown-fill"));
        assert!(html.contains("countdown-time"));
        assert!(html.contains("5:25")); // 325s = 5:25
        assert!(html.contains("data-remaining=\"325\""));
        assert!(html.contains("data-total=\"600\""));
        // Progress should be ~54.2%
        assert!(html.contains("54.2%"));
    }

    #[test]
    fn render_countdown_zero() {
        let html = render_value(&StatusValue::Countdown {
            remaining_secs: 0,
            total_secs: 600,
        });
        assert!(html.contains("0:00"));
        assert!(html.contains("width:0.0%"));
    }

    #[test]
    fn render_countdown_full() {
        let html = render_value(&StatusValue::Countdown {
            remaining_secs: 600,
            total_secs: 600,
        });
        assert!(html.contains("10:00")); // 600s = 10:00
        assert!(html.contains("width:100.0%"));
    }
}
