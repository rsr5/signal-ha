# Getting Started

Build your first Home Assistant automation as a standalone Rust process.

## Prerequisites

| Requirement | Why |
|-------------|-----|
| **Rust** (stable) | `rustup` recommended — workspace builds with `cargo` |
| **Home Assistant** | Long-lived access token ([how to create one](https://www.home-assistant.io/docs/authentication/#your-account-profile)) |
| **Linux + systemd** | Production supervisor — one service per automation |
| **Python 3.10+** | Only if using [Python bindings](crates/signal-ha-py.md) |

## Building the workspace

```bash
git clone https://github.com/rsr5/signal-ha.git
cd signal-ha
cargo build --release
```

All crates build from the workspace root. Binaries land in `target/release/`.

## 1. Create an automation

Each automation is its own binary crate. Create one alongside the library:

```bash
cargo new --name my-automation automations/my-automation
```

Add dependencies to `automations/my-automation/Cargo.toml`:

```toml
[dependencies]
signal-ha = { path = "../../crates/signal-ha" }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

## 2. Minimal example — motion-activated light

```rust
use signal_ha::HaClient;
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let url = env::var("HA_WS_URL")?;     // ws://ha:8123/api/websocket
    let token = env::var("HA_TOKEN")?;
    let client = HaClient::connect(&url, &token).await?;

    // Subscribe to a single entity
    let mut rx = client.subscribe_state("binary_sensor.hallway_motion").await?;

    while let Ok(change) = rx.recv().await {
        if let Some(ref new) = change.new {
            if new.state == "on" {
                client.call_service("light", "turn_on", serde_json::json!({
                    "entity_id": "light.hallway",
                    "brightness": 200
                })).await?;
            } else {
                client.call_service("light", "turn_off", serde_json::json!({
                    "entity_id": "light.hallway"
                })).await?;
            }
        }
    }

    Ok(())
}
```

Key points:

- `subscribe_state(entity_id)` returns a `broadcast::Receiver<StateChange>`
- Each `StateChange` has `entity_id`, `old: Option<EntityState>`, `new: Option<EntityState>`
- `EntityState` contains `state` (string), `attributes` (JSON), and `last_changed` (UTC)

## 3. Add a status page

Every automation should expose a status page for observability:

```rust
use signal_ha::StatusPage;

let status = StatusPage::new("my-automation", 9100);
status.spawn(); // starts HTTP server in background

// Update from your main loop
status.set_bool("State", "Motion detected", true);
status.set("State", "Last trigger", "2 min ago");
status.set_enum("Mode", "Current", "auto", &["auto", "manual", "off"]);
```

Browse to `http://host:9100` to see a live dashboard that auto-refreshes every 5 seconds.

## 4. Add sun-aware scheduling

```rust
use signal_ha::Scheduler;
use tokio_stream::StreamExt;

let sched = Scheduler::new(51.5, -0.1); // London

// Fire 30 min before sunset every day
let mut sunset_stream = sched.at_sunset(chrono::Duration::minutes(-30));
while let Some(time) = sunset_stream.next().await {
    tracing::info!("Pre-sunset trigger at {time}");
    // turn on outdoor lights...
}
```

Other scheduling methods: `at_sunrise()`, `daily()`, `after()`, `is_sun_up()`.

## 5. Add a dashboard

Ship a `dashboard.yaml` alongside your automation to auto-create a Lovelace dashboard in HA:

```yaml
url_path: signal-my-automation
title: "My Automation"
icon: "mdi:lightbulb"

config:
  views:
    - title: Status
      cards:
        - type: entities
          entities:
            - light.hallway
            - binary_sensor.hallway_motion
```

Load and sync it at startup:

```rust
use signal_ha::DashboardSpec;

let spec = DashboardSpec::from_yaml(include_str!("../dashboard.yaml"))?;
spec.ensure(&client).await?;
```

## 6. Deploy with systemd

```ini
[Unit]
Description=My Automation (signal-ha)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/my-automation
Environment=HA_WS_URL=ws://homeassistant.local:8123/api/websocket
Environment=HA_TOKEN=your_long_lived_token
Restart=on-failure
RestartSec=10
NoNewPrivileges=true
ProtectSystem=strict

[Install]
WantedBy=multi-user.target
```

```bash
sudo cp my-automation.service /etc/systemd/system/
sudo systemctl enable --now my-automation
```

## 7. Add an LLM observation agent (optional)

Embed an agent that periodically reviews your automation's behaviour and posts findings:

```rust
use signal_ha_agent::{AgentConfig, AgentHandle};
use signal_ha_agent::ha_host::HaHost;
use std::sync::Arc;
use std::time::Duration;

let ha_host = Arc::new(
    HaHost::new(client.clone(), &url, &token, "my-automation")
        .with_board_url("http://localhost:9200")
);

let agent = AgentHandle::spawn(AgentConfig {
    name: "my-automation".into(),
    role: "You are the hallway automation specialist.".into(),
    description: "Turns on hallway light when motion is detected.".into(),
    ha_client: client.clone(),
    conversation_entity: None,  // auto-detect
    primary_entities: vec![
        "light.hallway".into(),
        "binary_sensor.hallway_motion".into(),
    ],
    area: Some("hallway".into()),
    max_iterations: 8,
    default_interval: Duration::from_secs(86400), // daily
    ha_host,
    memory_path: "/var/lib/signal-ha/my-automation/memory.json".into(),
    disallowed_calls: vec![],
    transcript_dir: Some("/var/lib/signal-ha/transcripts".into()),
    inject_current_time: true,
    dashboard_url_path: Some("signal-my-automation".into()),
});
```

Trigger the agent manually:

```bash
systemctl kill -s SIGUSR1 my-automation
```

The agent writes findings to the [message board](crates/message-board.md) and updates an agent summary entity on your dashboard.

## Next steps

- [Architecture](architecture.md) — how the crates fit together
- [signal-ha](crates/signal-ha.md) — core library API reference
- [signal-ha-lighting](crates/signal-ha-lighting.md) — lighting primitives (actuators, overlays, lux curves)
- [signal-ha-agent](crates/signal-ha-agent.md) — agent configuration and tools
- [message-board](crates/message-board.md) — findings API for agent collaboration
