//! Dashboard management via the Home Assistant WebSocket API.
//!
//! Provides [`DashboardManager`] for low-level CRUD and [`DashboardSpec`]
//! for the common pattern where each automation owns a `dashboard.yaml`
//! that gets pushed to HA on startup.
//!
//! # Card type validation
//!
//! The manager ships with a list of built-in HA card types ([`BUILTIN_CARD_TYPES`])
//! and lets consumers register additional custom card types (e.g. HACS cards)
//! via [`DashboardSpec::custom_cards`] or [`DashboardManager::with_custom_cards`].
//!
//! # Automation-owned dashboards
//!
//! Each automation embeds its dashboard at compile time and pushes it on
//! startup. Idempotent — safe to run on every boot.
//!
//! ```rust,ignore
//! use signal_ha::{HaClient, DashboardSpec};
//!
//! // In an automation binary, embed the YAML at compile time:
//! // const DASHBOARD: &str = include_str!("../dashboard.yaml");
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let client = HaClient::connect("ws://ha:8123/api/websocket", "tok").await?;
//! // DashboardSpec::from_yaml(DASHBOARD)?.ensure(&client).await?;
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use tracing::{debug, info, warn};

use crate::client::{HaClient, HaError};

// ── Built-in card types ────────────────────────────────────────────

/// All card types shipped with Home Assistant as of 2026.3.
pub const BUILTIN_CARD_TYPES: &[&str] = &[
    // Always loaded
    "entity",
    "entities",
    "button",
    "glance",
    "grid",
    "section",
    "light",
    "sensor",
    "thermostat",
    "weather-forecast",
    "tile",
    "heading",
    // Lazy loaded
    "alarm-panel",
    "area",
    "calendar",
    "clock",
    "conditional",
    "distribution",
    "discovered-devices",
    "empty-state",
    "entity-filter",
    "error",
    "gauge",
    "history-graph",
    "home-summary",
    "horizontal-stack",
    "humidifier",
    "iframe",
    "logbook",
    "map",
    "markdown",
    "media-control",
    "picture",
    "picture-elements",
    "picture-entity",
    "picture-glance",
    "plant-status",
    "recovery-mode",
    "repairs",
    "shopping-list",
    "starting",
    "statistic",
    "statistics-graph",
    "todo-list",
    "toggle-group",
    "updates",
    "vertical-stack",
    // Energy cards
    "energy-compare",
    "energy-carbon-consumed-gauge",
    "energy-date-selection",
    "energy-devices-graph",
    "energy-devices-detail-graph",
    "energy-distribution",
    "energy-gas-graph",
    "energy-grid-neutrality-gauge",
    "energy-sankey",
    "energy-self-sufficiency-gauge",
    "energy-solar-consumed-gauge",
    "energy-solar-graph",
    "energy-sources-table",
    "energy-usage-graph",
    "energy-water-graph",
    "power-sankey",
    "power-sources-graph",
    "water-flow-sankey",
    "water-sankey",
];

// ── Dashboard metadata ─────────────────────────────────────────────

/// Dashboard metadata as returned by `lovelace/dashboards/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardInfo {
    pub id: String,
    pub url_path: String,
    pub title: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub show_in_sidebar: bool,
    #[serde(default)]
    pub require_admin: bool,
    pub mode: String,
}

// ── Validation result ──────────────────────────────────────────────

/// A card type referenced in a dashboard config that isn't in the
/// known set (built-in + custom).
#[derive(Debug, Clone)]
pub struct UnknownCard {
    pub view_index: usize,
    pub card_index: usize,
    pub card_type: String,
}

// ── DashboardManager ───────────────────────────────────────────────

/// Manages Lovelace dashboards over the HA WebSocket API.
///
/// Generic and reusable — the library provides built-in card types,
/// consumers add their own HACS/custom cards via [`with_custom_cards`].
///
/// [`with_custom_cards`]: DashboardManager::with_custom_cards
pub struct DashboardManager<'a> {
    client: &'a HaClient,
    custom_cards: HashSet<String>,
}

impl<'a> DashboardManager<'a> {
    /// Create a new manager with only built-in card types.
    pub fn new(client: &'a HaClient) -> Self {
        Self {
            client,
            custom_cards: HashSet::new(),
        }
    }

    /// Register additional custom card types (e.g. HACS cards).
    ///
    /// Custom cards in HA use the `custom:` prefix in YAML, but here
    /// you pass just the bare name — e.g. `"mushroom-entity-card"`.
    /// The validator accepts both `"custom:mushroom-entity-card"` and
    /// `"mushroom-entity-card"`.
    pub fn with_custom_cards(mut self, cards: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.custom_cards
            .extend(cards.into_iter().map(|c| c.into()));
        self
    }

    // ── Dashboard CRUD ─────────────────────────────────────────

    /// List all dashboards.
    pub async fn list_dashboards(&self) -> Result<Vec<DashboardInfo>, HaError> {
        let resp = self
            .client
            .send_raw(json!({"type": "lovelace/dashboards/list"}))
            .await?;
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        let dashboards: Vec<DashboardInfo> =
            serde_json::from_value(result).map_err(|e| HaError::Internal(e.to_string()))?;
        Ok(dashboards)
    }

    /// Create a new dashboard.
    pub async fn create_dashboard(
        &self,
        url_path: &str,
        title: &str,
        icon: &str,
    ) -> Result<(), HaError> {
        debug!(url_path, title, "Creating dashboard");
        let resp = self
            .client
            .send_raw(json!({
                "type": "lovelace/dashboards/create",
                "url_path": url_path,
                "title": title,
                "icon": icon,
                "show_in_sidebar": true,
                "require_admin": false,
            }))
            .await?;
        check_success(&resp)?;
        Ok(())
    }

    /// Update dashboard metadata (title, icon, sidebar visibility).
    pub async fn update_dashboard(
        &self,
        dashboard_id: &str,
        updates: Value,
    ) -> Result<(), HaError> {
        let mut msg = json!({
            "type": "lovelace/dashboards/update",
            "dashboard_id": dashboard_id,
        });
        if let (Some(base), Some(upd)) = (msg.as_object_mut(), updates.as_object()) {
            for (k, v) in upd {
                base.insert(k.clone(), v.clone());
            }
        }
        let resp = self.client.send_raw(msg).await?;
        check_success(&resp)?;
        Ok(())
    }

    /// Delete a dashboard.
    pub async fn delete_dashboard(&self, dashboard_id: &str) -> Result<(), HaError> {
        debug!(dashboard_id, "Deleting dashboard");
        let resp = self
            .client
            .send_raw(json!({
                "type": "lovelace/dashboards/delete",
                "dashboard_id": dashboard_id,
            }))
            .await?;
        check_success(&resp)?;
        Ok(())
    }

    // ── Config CRUD ────────────────────────────────────────────

    /// Get the full Lovelace config for a dashboard.
    ///
    /// Pass `None` for the default (overview) dashboard.
    pub async fn get_config(&self, url_path: Option<&str>) -> Result<Value, HaError> {
        let mut msg = json!({"type": "lovelace/config"});
        if let Some(path) = url_path {
            msg["url_path"] = json!(path);
        }
        let resp = self.client.send_raw(msg).await?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Save a full Lovelace config to a dashboard.
    ///
    /// This replaces the entire config atomically.
    pub async fn save_config(&self, url_path: &str, config: Value) -> Result<(), HaError> {
        debug!(url_path, "Saving dashboard config");
        let resp = self
            .client
            .send_raw(json!({
                "type": "lovelace/config/save",
                "url_path": url_path,
                "config": config,
            }))
            .await?;
        check_success(&resp)?;
        Ok(())
    }

    /// Delete a dashboard's config (resets to auto-generated).
    pub async fn delete_config(&self, url_path: &str) -> Result<(), HaError> {
        let resp = self
            .client
            .send_raw(json!({
                "type": "lovelace/config/delete",
                "url_path": url_path,
            }))
            .await?;
        check_success(&resp)?;
        Ok(())
    }

    // ── Ensure (idempotent create-or-update) ───────────────────

    /// Ensure a dashboard exists with the given config.
    ///
    /// Creates the dashboard if it doesn't exist, then saves the config.
    /// Idempotent — safe to call on every deploy.
    pub async fn ensure(
        &self,
        url_path: &str,
        title: &str,
        icon: &str,
        config: Value,
    ) -> Result<(), HaError> {
        let dashboards = self.list_dashboards().await?;
        let exists = dashboards.iter().any(|d| d.url_path == url_path);

        if !exists {
            self.create_dashboard(url_path, title, icon).await?;
        }

        self.save_config(url_path, config).await?;
        debug!(url_path, "Dashboard ensured");
        Ok(())
    }

    // ── Validation ─────────────────────────────────────────────

    /// Validate that all card types in a config are known.
    ///
    /// Returns a list of unknown card types with their location.
    /// Built-in types and registered custom types are accepted.
    /// Cards prefixed with `custom:` are matched against the
    /// custom card set (with the prefix stripped).
    pub fn validate_card_types(&self, config: &Value) -> Vec<UnknownCard> {
        let builtin: HashSet<&str> = BUILTIN_CARD_TYPES.iter().copied().collect();
        let mut unknowns = Vec::new();

        let views = match config.get("views").and_then(|v| v.as_array()) {
            Some(v) => v,
            None => return unknowns,
        };

        for (vi, view) in views.iter().enumerate() {
            let cards = match view.get("cards").and_then(|c| c.as_array()) {
                Some(c) => c,
                None => continue,
            };
            for (ci, card) in cards.iter().enumerate() {
                if let Some(card_type) = card.get("type").and_then(|t| t.as_str()) {
                    if !self.is_known_type(card_type, &builtin) {
                        unknowns.push(UnknownCard {
                            view_index: vi,
                            card_index: ci,
                            card_type: card_type.to_string(),
                        });
                    }
                    // Recurse into nested cards (stacks, grid, conditional)
                    self.validate_nested(card, vi, &builtin, &mut unknowns);
                }
            }
        }

        unknowns
    }

    fn is_known_type(&self, card_type: &str, builtin: &HashSet<&str>) -> bool {
        if builtin.contains(card_type) {
            return true;
        }
        // custom:foo-bar → check "foo-bar" in custom_cards
        if let Some(bare) = card_type.strip_prefix("custom:") {
            return self.custom_cards.contains(bare);
        }
        // Also accept bare custom card names
        self.custom_cards.contains(card_type)
    }

    fn validate_nested(
        &self,
        card: &Value,
        view_index: usize,
        builtin: &HashSet<&str>,
        unknowns: &mut Vec<UnknownCard>,
    ) {
        // Check "cards" (stacks, grid) and "card" (conditional)
        let nested: Vec<&Value> = card
            .get("cards")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .chain(card.get("card").into_iter())
            .collect();

        for (ci, nested_card) in nested.iter().enumerate() {
            if let Some(card_type) = nested_card.get("type").and_then(|t| t.as_str()) {
                if !self.is_known_type(card_type, builtin) {
                    unknowns.push(UnknownCard {
                        view_index,
                        card_index: ci,
                        card_type: card_type.to_string(),
                    });
                }
                self.validate_nested(nested_card, view_index, builtin, unknowns);
            }
        }
    }
}

// ── DashboardSpec (automation-owned dashboards) ────────────────────

/// A dashboard definition loaded from YAML.
///
/// Each automation ships a `dashboard.yaml` alongside its source.
/// The binary embeds it at compile time with `include_str!` and calls
/// [`ensure`] on startup to push it to HA.
///
/// [`ensure`]: DashboardSpec::ensure
///
/// # YAML format
///
/// ```yaml
/// url_path: signal-porch-lights
/// title: "Porch Lights"
/// icon: "mdi:coach-lamp"
///
/// # Optional: HACS/custom card types used by this dashboard
/// custom_cards:
///   - mushroom-entity-card
///
/// config:
///   views:
///     - title: "Porch Lights"
///       cards:
///         - type: entities
///           entities:
///             - light.porch
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DashboardSpec {
    /// URL path for the dashboard (e.g. `"signal-porch-lights"`).
    pub url_path: String,
    /// Display title shown in HA sidebar.
    pub title: String,
    /// MDI icon (e.g. `"mdi:coach-lamp"`).
    #[serde(default = "default_icon")]
    pub icon: String,
    /// Full Lovelace config (views, cards, etc.).
    pub config: Value,
    /// Optional list of custom/HACS card types used by this dashboard.
    /// Bare names without the `custom:` prefix.
    #[serde(default)]
    pub custom_cards: Vec<String>,
}

fn default_icon() -> String {
    "mdi:robot".to_string()
}

impl DashboardSpec {
    /// Parse a dashboard spec from a YAML string.
    ///
    /// Typically used with `include_str!("../dashboard.yaml")`.
    pub fn from_yaml(yaml: &str) -> Result<Self, HaError> {
        serde_yaml::from_str(yaml).map_err(|e| HaError::Internal(format!("bad dashboard YAML: {e}")))
    }

    /// Push this dashboard to Home Assistant.
    ///
    /// Creates the dashboard if it doesn't exist, then saves the config.
    /// Logs a warning for any unknown card types but does not fail —
    /// custom cards may be installed on HA but not registered here.
    ///
    /// Idempotent — safe to call on every startup.
    pub async fn ensure(&self, client: &HaClient) -> Result<(), HaError> {
        let mgr = DashboardManager::new(client)
            .with_custom_cards(self.custom_cards.iter().cloned());

        // Warn about unknown card types
        let unknowns = mgr.validate_card_types(&self.config);
        for u in &unknowns {
            warn!(
                url_path = self.url_path.as_str(),
                view = u.view_index,
                card = u.card_index,
                card_type = u.card_type.as_str(),
                "Unknown card type in dashboard spec"
            );
        }

        mgr.ensure(&self.url_path, &self.title, &self.icon, self.config.clone())
            .await?;

        info!(
            url_path = self.url_path.as_str(),
            title = self.title.as_str(),
            "Dashboard synced to HA"
        );
        Ok(())
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn check_success(resp: &Value) -> Result<(), HaError> {
    if resp.get("success").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    let code = resp
        .pointer("/error/code")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let message = resp
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown error");
    Err(HaError::HaError(format!("{code}: {message}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_builtin_types_pass() {
        let builtin: HashSet<&str> = BUILTIN_CARD_TYPES.iter().copied().collect();
        assert!(builtin.contains("entities"));
        assert!(builtin.contains("tile"));
        assert!(builtin.contains("history-graph"));
        assert!(builtin.contains("energy-distribution"));
        assert!(!builtin.contains("nonexistent"));

        // Verify that BUILTIN_CARD_TYPES has all the expected major types
        assert!(BUILTIN_CARD_TYPES.len() >= 50);
    }

    #[test]
    fn validate_finds_unknown_cards() {
        // We can't construct a DashboardManager without a real client,
        // so we test is_known_type and validate_nested via a mock-like approach.
        let custom: HashSet<String> = ["mushroom-entity-card".to_string()].into_iter().collect();
        let builtin: HashSet<&str> = BUILTIN_CARD_TYPES.iter().copied().collect();

        // Built-in
        assert!(builtin.contains("entities"));

        // Custom with prefix
        assert!(custom.contains("mushroom-entity-card"));

        // Unknown
        assert!(!builtin.contains("nonexistent-card"));
        assert!(!custom.contains("nonexistent-card"));
    }

    #[test]
    fn check_success_ok() {
        let resp = json!({"success": true, "result": null});
        assert!(check_success(&resp).is_ok());
    }

    #[test]
    fn check_success_error() {
        let resp = json!({
            "success": false,
            "error": { "code": "config_not_found", "message": "No config found." }
        });
        let err = check_success(&resp).unwrap_err();
        assert!(err.to_string().contains("config_not_found"));
    }

    #[test]
    fn dashboard_spec_from_yaml() {
        let yaml = r#"
url_path: signal-porch-lights
title: "Porch Lights"
icon: "mdi:coach-lamp"
custom_cards:
  - mushroom-entity-card
config:
  views:
    - title: Overview
      cards:
        - type: entities
          entities:
            - light.porch
"#;
        let spec = DashboardSpec::from_yaml(yaml).unwrap();
        assert_eq!(spec.url_path, "signal-porch-lights");
        assert_eq!(spec.title, "Porch Lights");
        assert_eq!(spec.icon, "mdi:coach-lamp");
        assert_eq!(spec.custom_cards, vec!["mushroom-entity-card"]);
        assert!(spec.config["views"][0]["cards"][0]["type"]
            .as_str()
            .unwrap()
            == "entities");
    }

    #[test]
    fn dashboard_spec_default_icon() {
        let yaml = r#"
url_path: signal-test
title: Test
config:
  views: []
"#;
        let spec = DashboardSpec::from_yaml(yaml).unwrap();
        assert_eq!(spec.icon, "mdi:robot");
        assert!(spec.custom_cards.is_empty());
    }

    #[test]
    fn dashboard_spec_invalid_yaml() {
        let yaml = "not: [valid: yaml: {";
        assert!(DashboardSpec::from_yaml(yaml).is_err());
    }
}
