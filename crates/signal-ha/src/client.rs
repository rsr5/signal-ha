//! WebSocket client for Home Assistant.
//!
//! Handles connection, authentication, state queries, service calls,
//! and real-time state change subscriptions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Notify};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, trace, warn};

use crate::types::{EntityState, StateChange};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type WsSource = SplitStream<WsStream>;

/// Errors that can occur in the HA client.
#[derive(Debug, thiserror::Error)]
pub enum HaError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("HA returned error: {0}")]
    HaError(String),

    #[error("Response too large (exceeded WebSocket frame limit): {0}")]
    ResponseTooLarge(String),

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Request timed out")]
    Timeout,

    #[error("Internal channel error: {0}")]
    Internal(String),
}

type Result<T> = std::result::Result<T, HaError>;

/// Pending request waiting for a response keyed by message id.
type PendingTx = oneshot::Sender<Value>;

/// A WebSocket client connected and authenticated to Home Assistant.
///
/// All methods are safe to call concurrently from multiple tasks.
/// Internally, a background task multiplexes reads/writes over a single
/// WebSocket connection.
#[derive(Clone)]
pub struct HaClient {
    /// Channel to send outbound messages through the writer task.
    cmd_tx: mpsc::Sender<ClientCmd>,
    /// Monotonically increasing message id.
    next_id: Arc<AtomicU64>,
    /// HTTP client for REST API calls (set_state, etc.)
    http: reqwest::Client,
    /// Base HTTP URL, e.g. "http://homeassistant.local:8123"
    base_url: String,
    /// Auth token for REST API calls.
    token: String,
    /// Fires when the WebSocket connection is lost.
    disconnect: Arc<Notify>,
}

/// Internal commands sent to the writer task.
enum ClientCmd {
    /// Send a message and expect a response with this id.
    Send {
        id: u64,
        message: Value,
        reply: PendingTx,
    },
    /// Subscribe to state changes for an entity.
    Subscribe {
        subscription_id: u64,
        entity_id: String,
        reply: PendingTx,
        event_tx: broadcast::Sender<StateChange>,
    },
}

impl HaClient {
    /// Connect to Home Assistant and authenticate.
    ///
    /// # Arguments
    /// * `url` — WebSocket URL, e.g. `ws://homeassistant.local:8123/api/websocket`
    /// * `token` — Long-lived access token
    pub async fn connect(url: &str, token: &str) -> Result<Self> {
        info!(url, "Connecting to Home Assistant");

        // Connect and authenticate before spawning background loops.
        let (ws, _) = connect_async(url).await?;
        let (mut sink, mut stream) = ws.split();

        // Step 1: read auth_required
        let auth_msg = Self::read_next(&mut stream).await?;
        let msg_type = auth_msg["type"].as_str().unwrap_or("");
        if msg_type != "auth_required" {
            return Err(HaError::AuthFailed(format!(
                "Expected auth_required, got: {msg_type}"
            )));
        }
        debug!("Received auth_required");

        // Step 2: send auth
        let auth = json!({
            "type": "auth",
            "access_token": token
        });
        sink.send(Message::Text(auth.to_string().into())).await?;
        debug!("Sent auth token");

        // Step 3: read auth response
        let auth_resp = Self::read_next(&mut stream).await?;
        let resp_type = auth_resp["type"].as_str().unwrap_or("");
        match resp_type {
            "auth_ok" => {
                info!(
                    ha_version = auth_resp["ha_version"].as_str().unwrap_or("unknown"),
                    "Authenticated with Home Assistant"
                );
            }
            "auth_invalid" => {
                return Err(HaError::AuthFailed(
                    auth_resp["message"]
                        .as_str()
                        .unwrap_or("invalid token")
                        .to_string(),
                ));
            }
            other => {
                return Err(HaError::AuthFailed(format!(
                    "Unexpected auth response: {other}"
                )));
            }
        }

        // Now spawn the background loops
        let next_id = Arc::new(AtomicU64::new(1));
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(64);
        let pending: Arc<Mutex<HashMap<u64, PendingTx>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let subscriptions: Arc<Mutex<HashMap<u64, broadcast::Sender<StateChange>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let disconnect = Arc::new(Notify::new());

        tokio::spawn(Self::writer_loop(
            sink,
            cmd_rx,
            pending.clone(),
            subscriptions.clone(),
        ));
        tokio::spawn(Self::reader_loop(
            stream,
            pending,
            subscriptions,
            disconnect.clone(),
        ));

        // Derive HTTP base URL from WebSocket URL:
        // ws://host:port/api/websocket → http://host:port
        let base_url = url
            .replace("ws://", "http://")
            .replace("wss://", "https://")
            .trim_end_matches("/api/websocket")
            .to_string();

        Ok(Self {
            cmd_tx,
            next_id,
            http: reqwest::Client::new(),
            base_url,
            token: token.to_string(),
            disconnect,
        })
    }

    /// Returns a future that resolves when the WebSocket connection is lost.
    ///
    /// Automations should select on this alongside their main loop and exit
    /// with an error so systemd can restart the process.
    pub async fn disconnected(&self) {
        self.disconnect.notified().await;
    }

    /// Get the current state of an entity.
    pub async fn get_state(&self, entity_id: &str) -> Result<EntityState> {
        debug!(entity_id, "Getting state");
        // HA doesn't have a single-entity get_state over WS,
        // so we fetch all states and filter. Cache could help later.
        let id = self.next_id();
        let msg = json!({
            "id": id,
            "type": "get_states"
        });
        let resp = self.send_and_wait(id, msg, Duration::from_secs(30)).await?;

        let states = resp["result"]
            .as_array()
            .ok_or_else(|| HaError::HaError("get_states returned no array".into()))?;

        for state in states {
            if state["entity_id"].as_str() == Some(entity_id) {
                return Self::parse_entity_state(state);
            }
        }

        Err(HaError::HaError(format!(
            "Entity not found: {entity_id}"
        )))
    }

    /// Call a Home Assistant service.
    ///
    /// # Arguments
    /// * `domain` — e.g. "light", "switch", "input_boolean"
    /// * `service` — e.g. "turn_on", "turn_off"
    /// * `data` — service data (entity_id, brightness, etc.)
    pub async fn call_service(
        &self,
        domain: &str,
        service: &str,
        data: Value,
    ) -> Result<()> {
        info!(domain, service, "Calling service");
        let id = self.next_id();
        let msg = json!({
            "id": id,
            "type": "call_service",
            "domain": domain,
            "service": service,
            "service_data": data
        });
        let resp = self.send_and_wait(id, msg, Duration::from_secs(30)).await?;
        if resp["success"].as_bool() != Some(true) {
            return Err(HaError::HaError(format!(
                "Service call failed: {}",
                resp["error"]
            )));
        }
        Ok(())
    }

    /// Set (create or update) a transient entity via the REST API.
    ///
    /// This is equivalent to AppDaemon's `set_state()`. The entity is
    /// created on the fly if it doesn't exist and survives until HA
    /// restarts.
    ///
    /// # Arguments
    /// * `entity_id` — e.g. "sensor.porch_lights_reason"
    /// * `state` — the state string value
    /// * `attributes` — optional JSON object of attributes
    pub async fn set_state(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Option<Value>,
    ) -> Result<()> {
        info!(entity_id, state, "Setting state (REST)");
        let url = format!("{}/api/states/{}", self.base_url, entity_id);
        let mut body = json!({ "state": state });
        if let Some(attrs) = attributes {
            body["attributes"] = attrs;
        }

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| HaError::HaError(format!("REST request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(HaError::HaError(format!(
                "set_state failed ({status}): {text}"
            )));
        }

        Ok(())
    }

    /// Subscribe to state changes for a specific entity.
    ///
    /// Returns a [`broadcast::Receiver`] that yields [`StateChange`] events.
    /// Multiple subscribers can listen to the same entity.
    pub async fn subscribe_state(
        &self,
        entity_id: &str,
    ) -> Result<broadcast::Receiver<StateChange>> {
        info!(entity_id, "Subscribing to state changes");
        let id = self.next_id();

        let (event_tx, event_rx) = broadcast::channel::<StateChange>(64);
        let (reply_tx, reply_rx) = oneshot::channel();

        self.cmd_tx
            .send(ClientCmd::Subscribe {
                subscription_id: id,
                entity_id: entity_id.to_string(),
                reply: reply_tx,
                event_tx,
            })
            .await
            .map_err(|_| HaError::ConnectionClosed)?;

        // The writer will send the message; wait for the ack
        let resp = reply_rx
            .await
            .map_err(|_| HaError::Internal("Reply channel dropped".into()))?;

        // Check if HA accepted the subscription
        let success = resp["success"].as_bool().unwrap_or(false);
        if !success {
            let err_msg = resp["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            error!(entity_id, id, error = err_msg, "Subscription rejected by HA");
            return Err(HaError::HaError(format!(
                "subscribe_trigger rejected for {entity_id}: {err_msg}"
            )));
        }
        info!(entity_id, id, "Subscription accepted");

        Ok(event_rx)
    }

    /// Send an arbitrary WebSocket message (escape hatch).
    ///
    /// The `id` field will be injected automatically.
    /// Uses the default 30-second timeout.
    pub async fn send_raw(&self, mut msg: Value) -> Result<Value> {
        let id = self.next_id();
        msg["id"] = json!(id);
        self.send_and_wait(id, msg, Duration::from_secs(30)).await
    }

    /// Like `send_raw` but with a custom timeout.
    ///
    /// Use this for slow operations like `conversation/process`
    /// (LLM calls can take 60-120 seconds).
    pub async fn send_raw_timeout(
        &self,
        mut msg: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id();
        msg["id"] = json!(id);
        self.send_and_wait(id, msg, timeout).await
    }

    // ── Internal helpers ───────────────────────────────────────────

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send_and_wait(&self, id: u64, msg: Value, timeout: Duration) -> Result<Value> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ClientCmd::Send {
                id,
                message: msg,
                reply: reply_tx,
            })
            .await
            .map_err(|_| HaError::ConnectionClosed)?;

        let resp = tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| HaError::Timeout)?
            .map_err(|_| HaError::ConnectionClosed)?;

        // Check for error markers injected by reader_loop when
        // the WebSocket connection hit a fatal error.
        if resp.get("_signal_ha_error").is_some() {
            let err_type = resp["_error_type"].as_str().unwrap_or("unknown");
            let message = resp["_message"].as_str().unwrap_or("unknown error").to_string();
            return match err_type {
                "response_too_large" => Err(HaError::ResponseTooLarge(message)),
                _ => Err(HaError::ConnectionClosed),
            };
        }

        Ok(resp)
    }

    async fn read_next(stream: &mut WsSource) -> Result<Value> {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    let v: Value = serde_json::from_str(&text)?;
                    return Ok(v);
                }
                Some(Ok(Message::Ping(_))) => continue,
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) => return Err(HaError::ConnectionClosed),
                Some(Err(e)) => return Err(HaError::WebSocket(e)),
                None => return Err(HaError::ConnectionClosed),
                _ => continue,
            }
        }
    }

    fn parse_entity_state(v: &Value) -> Result<EntityState> {
        Ok(EntityState {
            state: v["state"].as_str().unwrap_or("unknown").to_string(),
            attributes: v["attributes"].clone(),
            last_changed: v["last_changed"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default(),
        })
    }

    async fn writer_loop(
        mut sink: WsSink,
        mut cmd_rx: mpsc::Receiver<ClientCmd>,
        pending: Arc<Mutex<HashMap<u64, PendingTx>>>,
        subscriptions: Arc<Mutex<HashMap<u64, broadcast::Sender<StateChange>>>>,
    ) {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                ClientCmd::Send { id, message, reply } => {
                    pending.lock().await.insert(id, reply);
                    let text = message.to_string();
                    if let Err(e) = sink.send(Message::Text(text.into())).await {
                        error!(?e, id, "Failed to send message");
                        // Remove pending so caller gets an error
                        pending.lock().await.remove(&id);
                    }
                }
                ClientCmd::Subscribe {
                    subscription_id,
                    entity_id,
                    reply,
                    event_tx,
                } => {
                    // Store the subscription channel
                    subscriptions
                        .lock()
                        .await
                        .insert(subscription_id, event_tx);
                    // Send the subscribe message
                    let msg = json!({
                        "id": subscription_id,
                        "type": "subscribe_trigger",
                        "trigger": {
                            "platform": "state",
                            "entity_id": entity_id
                        }
                    });
                    pending.lock().await.insert(subscription_id, reply);
                    let text = msg.to_string();
                    if let Err(e) = sink.send(Message::Text(text.into())).await {
                        error!(?e, subscription_id, "Failed to send subscribe");
                        pending.lock().await.remove(&subscription_id);
                        subscriptions.lock().await.remove(&subscription_id);
                    }
                }
            }
        }
        debug!("Writer loop ended");
    }

    async fn reader_loop(
        mut stream: WsSource,
        pending: Arc<Mutex<HashMap<u64, PendingTx>>>,
        subscriptions: Arc<Mutex<HashMap<u64, broadcast::Sender<StateChange>>>>,
        disconnect: Arc<Notify>,
    ) {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    let msg: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(?e, "Failed to parse WS message");
                            continue;
                        }
                    };

                    let msg_type = msg["type"].as_str().unwrap_or("");
                    trace!(msg_type, raw = %msg, "WS recv");

                    match msg_type {
                        "result" => {
                            // Response to a command
                            if let Some(id) = msg["id"].as_u64() {
                                if let Some(tx) = pending.lock().await.remove(&id) {
                                    let _ = tx.send(msg);
                                }
                            }
                        }
                        "event" => {
                            // Subscription event
                            if let Some(id) = msg["id"].as_u64() {
                                let subs = subscriptions.lock().await;
                                if let Some(tx) = subs.get(&id) {
                                    if let Some(change) =
                                        Self::parse_state_change_event(&msg)
                                    {
                                        debug!(
                                            entity = %change.entity_id,
                                            new_state = ?change.new.as_ref().map(|s| &s.state),
                                            "Dispatching state change"
                                        );
                                        let _ = tx.send(change);
                                    } else {
                                        warn!(
                                            id,
                                            raw = %msg,
                                            "Failed to parse subscription event"
                                        );
                                    }
                                }
                            }
                        }
                        "pong" => { /* heartbeat response */ }
                        other => {
                            debug!(msg_type = other, "Unhandled message type");
                        }
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    debug!("Received ping");
                    // Pong is handled automatically by tungstenite
                    let _ = data;
                }
                Some(Ok(Message::Close(_))) | None => {
                    error!("WebSocket connection closed — signalling disconnect");
                    disconnect.notify_waiters();
                    break;
                }
                Some(Err(e)) => {
                    // Check if this is a capacity/message-too-long error.
                    // After a capacity error the WS stream is corrupt — we
                    // must close.  But first, notify all pending callers so
                    // they get a meaningful error instead of "channel dropped".
                    let err_str = format!("{e}");
                    let is_capacity = matches!(
                        e,
                        tokio_tungstenite::tungstenite::Error::Capacity(..)
                    );
                    if is_capacity {
                        error!(
                            error = %err_str,
                            "WebSocket response too large — notifying pending callers"
                        );
                    } else {
                        error!(error = %err_str, "WebSocket error");
                    }

                    // Drain all pending requests with an error marker.
                    // The marker JSON is detected by send_and_wait() to
                    // produce the appropriate HaError variant.
                    let error_value = json!({
                        "_signal_ha_error": true,
                        "_error_type": if is_capacity { "response_too_large" } else { "connection_lost" },
                        "_message": err_str,
                    });
                    let mut pending = pending.lock().await;
                    for (_id, tx) in pending.drain() {
                        let _ = tx.send(error_value.clone());
                    }
                    disconnect.notify_waiters();
                    break;
                }
                _ => {}
            }
        }
    }

    fn parse_state_change_event(msg: &Value) -> Option<StateChange> {
        let event = &msg["event"]["variables"]["trigger"];
        let entity_id = event["entity_id"].as_str()?.to_string();

        // HA subscribe_trigger with platform: state sends "from_state"
        // and "to_state" (NOT "old_state" / "new_state").
        let old = event.get("from_state").and_then(|s| {
            Some(EntityState {
                state: s["state"].as_str()?.to_string(),
                attributes: s["attributes"].clone(),
                last_changed: s["last_changed"]
                    .as_str()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or_default(),
            })
        });

        let new = event.get("to_state").and_then(|s| {
            Some(EntityState {
                state: s["state"].as_str()?.to_string(),
                attributes: s["attributes"].clone(),
                last_changed: s["last_changed"]
                    .as_str()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or_default(),
            })
        });

        Some(StateChange {
            entity_id,
            old,
            new,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify we parse the subscribe_trigger event format correctly.
    /// HA sends `from_state` / `to_state`, NOT `old_state` / `new_state`.
    #[test]
    fn parse_subscribe_trigger_event() {
        let msg: Value = serde_json::from_str(r#"{
            "id": 2,
            "type": "event",
            "event": {
                "variables": {
                    "trigger": {
                        "id": "0",
                        "idx": "0",
                        "platform": "state",
                        "entity_id": "binary_sensor.garage_motion_sensor_motion",
                        "from_state": {
                            "entity_id": "binary_sensor.garage_motion_sensor_motion",
                            "state": "off",
                            "attributes": { "device_class": "motion" },
                            "last_changed": "2026-03-05T22:00:00+00:00",
                            "last_updated": "2026-03-05T22:00:00+00:00"
                        },
                        "to_state": {
                            "entity_id": "binary_sensor.garage_motion_sensor_motion",
                            "state": "on",
                            "attributes": { "device_class": "motion" },
                            "last_changed": "2026-03-05T22:01:26+00:00",
                            "last_updated": "2026-03-05T22:01:26+00:00"
                        },
                        "for": null,
                        "attribute": null
                    }
                }
            }
        }"#).unwrap();

        let change = HaClient::parse_state_change_event(&msg)
            .expect("should parse trigger event");

        assert_eq!(change.entity_id, "binary_sensor.garage_motion_sensor_motion");

        let old = change.old.expect("old (from_state) should be Some");
        assert_eq!(old.state, "off");

        let new = change.new.expect("new (to_state) should be Some");
        assert_eq!(new.state, "on");
    }

    /// Ensure we return None when entity_id is missing.
    #[test]
    fn parse_event_missing_entity_id() {
        let msg: Value = serde_json::from_str(r#"{
            "id": 2,
            "type": "event",
            "event": { "variables": { "trigger": {} } }
        }"#).unwrap();

        assert!(HaClient::parse_state_change_event(&msg).is_none());
    }
}
