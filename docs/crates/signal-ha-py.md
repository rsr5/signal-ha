# signal-ha-py

> PyO3 bindings — use signal-ha from Python.

## Overview

Python bindings for the core `signal-ha` library via PyO3. Exposes
`HaClient`, `Scheduler`, and `EntityState` as native Python classes. Each
automation runs in its own asyncio event loop backed by a shared tokio
runtime.

## Installation

```bash
pip install maturin
cd crates/signal-ha-py
maturin develop --release
```

This builds a native Python extension module (`signal_ha`) that you can
import directly.

## Usage

```python
import asyncio
from signal_ha import HaClient, Scheduler, EntityState

async def main():
    client = await HaClient.connect(
        "ws://homeassistant.local:8123/api/websocket",
        "your_token"
    )

    # Query state
    state = await client.get_state("sensor.temperature")
    print(f"Temperature: {state.state}")
    print(f"Attributes: {state.attributes_json}")

    # Call a service
    await client.call_service("light", "turn_on", {
        "entity_id": "light.office",
        "brightness": 200
    })

asyncio.run(main())
```

### Scheduler

```python
from signal_ha import Scheduler

sched = Scheduler(48.86, 2.35)  # lat, lon
sunrise = sched.next_sunrise()
sunset = sched.next_sunset()
```

## API Reference

### HaClient

| Method | Returns | Purpose |
|:-------|:--------|:--------|
| `connect(url, token)` | `HaClient` | Connect to HA WebSocket API |
| `get_state(entity_id)` | `EntityState` | Get current entity state |
| `call_service(domain, service, data)` | `None` | Call a HA service |
| `send_raw(json_msg)` | `str` | Send arbitrary WebSocket message |

### EntityState

| Property | Type | Purpose |
|:---------|:-----|:--------|
| `.state` | `str` | State value |
| `.attributes_json` | `str` | Attributes as JSON string |
| `.last_changed` | `str` | ISO 8601 timestamp |

### Scheduler

| Method | Returns | Purpose |
|:-------|:--------|:--------|
| `Scheduler(lat, lon)` | `Scheduler` | Create sun-aware scheduler |
| `next_sunrise()` | `datetime` | Next sunrise time |
| `next_sunset()` | `datetime` | Next sunset time |
