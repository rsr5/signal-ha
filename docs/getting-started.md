# Getting Started

## Prerequisites

- **Rust** (stable or nightly) — `rustup` recommended
- **Home Assistant** instance with a long-lived access token
- **systemd** (Linux) for production deployment
- **Python 3.10+** and **maturin** (only if using Python bindings)

## Building

```bash
git clone https://github.com/rsr5/signal-ha.git
cd signal-ha
cargo build --release
```

All six crates build from the workspace root.

## Writing an automation

Create a new binary crate that depends on `signal-ha`:

```bash
cargo new my-automation
cd my-automation
```

Add dependencies to `Cargo.toml`:

```toml
[dependencies]
signal-ha = { path = "../signal-ha/crates/signal-ha" }
signal-ha-lighting = { path = "../signal-ha/crates/signal-ha-lighting" }  # optional
signal-ha-agent = { path = "../signal-ha/crates/signal-ha-agent" }        # optional
tokio = { version = "1", features = ["full"] }
```

### Minimal example

```rust
use signal_ha::{HaClient, Scheduler};
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let token = env::var("HA_TOKEN")?;
    let client = HaClient::connect(
        "ws://homeassistant.local:8123/api/websocket",
        &token,
    ).await?;

    // Subscribe to state changes
    let mut rx = client.subscribe_state_changes().await?;

    while let Some(change) = rx.recv().await {
        if change.entity_id == "binary_sensor.front_door" {
            if change.new.state == "on" {
                client.call_service("light", "turn_on", serde_json::json!({
                    "entity_id": "light.hallway"
                })).await?;
            }
        }
    }

    Ok(())
}
```

## Deploying with systemd

Create a service file:

```ini
[Unit]
Description=My Automation
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/my-automation
Environment=HA_TOKEN=your_token_here
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo cp my-automation.service /etc/systemd/system/
sudo systemctl enable --now my-automation
```

## Adding an agent

To embed an LLM observation agent in your automation:

```rust
use signal_ha_agent::{AgentConfig, AgentHandle};

// After setting up your HaClient...
let agent = AgentHandle::spawn(AgentConfig {
    name: "my-automation-agent".into(),
    role: "automation observer".into(),
    description: "Watches my-automation and reports anomalies".into(),
    ha_client: client.clone(),
    conversation_entity: "conversation.claude".into(),
    memory_path: "/var/lib/signal-ha/my-automation/memory.json".into(),
    disallowed_calls: vec!["call_service".into()],
}).await;
```

The agent runs in the background and can be triggered via `SIGUSR1`:

```bash
systemctl kill -s SIGUSR1 my-automation
```

## Python bindings

```bash
pip install maturin
cd crates/signal-ha-py
maturin develop --release
```

```python
import asyncio
from signal_ha import HaClient

async def main():
    client = await HaClient.connect(
        "ws://homeassistant.local:8123/api/websocket",
        "your_token"
    )
    state = await client.get_state("sensor.temperature")
    print(f"Temperature: {state.state}")

asyncio.run(main())
```
