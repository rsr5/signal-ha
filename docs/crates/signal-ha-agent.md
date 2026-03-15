# signal-ha-agent

> Embedded LLM observation agent — uses the HA Conversation API to watch automations at runtime.

## Overview

Each signal-ha automation can embed an **agent** that periodically assesses
the automation's behaviour. The agent sends a system prompt and conversation
history to Home Assistant's Conversation API (backed by any LLM — local
Llama, OpenAI, Anthropic, etc.), parses fenced Python code blocks from the
response, executes them via the Monty runtime, and posts findings to the
message board.

The agent is **read-only by default** — it can query state and history but
cannot call services unless explicitly permitted.

## Architecture

```mermaid
sequenceDiagram
    participant Auto as Automation
    participant Agent as signal-ha-agent
    participant HA as Home Assistant
    participant LLM as LLM (via Conversation API)
    participant Board as message-board

    Auto->>Agent: SIGUSR1 (trigger)
    Agent->>HA: conversation/process (system prompt)
    HA->>LLM: Forward prompt
    LLM-->>HA: Response with ```python blocks
    HA-->>Agent: Markdown response
    Agent->>Agent: Parse code blocks
    Agent->>Agent: Execute via Monty runtime
    Note over Agent: Python calls state(), history()
    Agent->>HA: Fulfil host calls (WebSocket)
    HA-->>Agent: Entity data
    Agent->>Agent: Resume Python execution
    Agent->>Board: POST /posts (findings)
```

## Usage

```rust
use signal_ha_agent::{AgentConfig, AgentHandle};

let config = AgentConfig {
    name: "porch-lights-agent".into(),
    role: "lighting observer".into(),
    description: "Monitors porch light automation behaviour".into(),
    ha_client: client.clone(),
    conversation_entity: "conversation.claude".into(),
    memory_path: "/var/lib/signal-ha/porch-lights/memory.json".into(),
    disallowed_calls: vec!["call_service".into()],
};

let handle = AgentHandle::spawn(config).await;
```

## API Reference

| Type | Purpose |
|:-----|:--------|
| `AgentConfig` | Configuration — name, role, HA client, conversation entity, memory path, disallowed calls |
| `AgentHandle` | Lifecycle handle returned by `spawn()` |

### Modules

| Module | Purpose |
|:-------|:--------|
| `conversation` | `conversation/process` WebSocket wrapper for LLM interaction |
| `parser` | Parses fenced `signal-deck` code blocks from LLM markdown |
| `engine` | Python execution engine wrapping signal-ha-shell's REPL |
| `ha_host` | HA-specific host call fulfillment — `state()`, `history()`, `call_service()` |
| `memory` | Persistent JSON memory across agent sessions |
| `session` | Agent session lifecycle management |

## Safety

!!! warning "Read-only by default"
    Agents can query entity state and history but **cannot call services**
    unless `call_service` is removed from `disallowed_calls`. This prevents
    an LLM from accidentally turning things on/off.

!!! info "Panic isolation"
    Panics in agent code are caught and logged — they never propagate to the
    host automation process.
