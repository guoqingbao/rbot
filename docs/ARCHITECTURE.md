# rbot Architecture

## Runtime Model

`rbot` is organized as a message-driven runtime:

1. A channel receives inbound user activity.
2. The channel publishes an `InboundMessage` onto the bus.
3. `AgentRuntime` pulls from the bus and invokes `AgentLoop`.
4. `AgentLoop` builds context, calls the model, executes tools, and persists the turn.
5. Outbound messages are published back onto the bus.
6. `ChannelManager` delivers outbound messages through the target transport.

This keeps transport, orchestration, and model execution separate.

## Main Components

| Module | Responsibility |
| --- | --- |
| `src/engine/orchestrator.rs` | Agent turn loop, session commands, tool iteration |
| `src/engine/context.rs` | Runtime context assembly from workspace files and media |
| `src/storage/session_store.rs` | JSONL-backed session storage |
| `src/engine/memory.rs` | Long-term memory archiving and consolidation policy |
| `src/tools.rs` | Tool registry and built-in tool implementations |
| `src/runtime/worker.rs` | Bus worker that connects inbound messages to the agent |
| `src/channels/` | Transport adapters and channel manager |
| `src/runtime/http.rs` | HTTP ingress plus health/readiness/status endpoints |
| `src/observability.rs` | Runtime telemetry, metrics, provider instrumentation, and system/provider snapshots |
| `src/cron.rs` | Scheduled jobs and execution history |
| `src/runtime/heartbeat.rs` | Periodic task review loop |
| `src/providers/` | Provider clients and registry metadata |
| `src/runtime/bootstrap.rs` | Backend startup validation and provider construction |
| `src/integrations/mcp.rs` | MCP stdio client and tool registration |

## Design Choices

### Domain-first module layout

The crate is organized by runtime domain instead of by a flat file list:

- `engine/` owns reasoning, context construction, memory policy, skills, and background subtasks
- `runtime/` owns process wiring, HTTP ingress, validation, and long-running services
- `storage/` owns session persistence and the internal message bus
- `channels/` owns transport adapters
- `providers/` owns model backends and provider metadata
- `integrations/` owns external protocol bridges such as MCP
- `observability.rs` owns metrics and monitoring state shared across the runtime

That layout keeps operational concerns separate from agent behavior and avoids coupling transports, storage, and orchestration together.

### Trait-based boundaries

Providers, tools, and channels are all expressed as traits. That keeps the runtime swappable and makes transport-specific logic independent from agent execution.

### Message bus orchestration

The bus is the seam between transport and reasoning. Channels do not call the agent directly, and the agent does not know how messages are physically delivered.

### Persistent workspace state

Sessions, memory files, cron jobs, and skills are stored on disk so the runtime can be restarted without losing state.

### OpenAI-compatible provider contract

`rbot` uses an OpenAI-compatible chat-completions contract as the common provider interface. That keeps remote APIs and local runtimes behind the same operational path.

## Backend Operation

`rbot run` starts:

- provider client
- agent runtime
- cron service
- heartbeat service
- enabled channels
- HTTP gateway

The same process also exposes:

- the admin UI
- the admin JSON API
- the Prometheus metrics endpoint

The HTTP gateway is operationally useful even when the main traffic is not HTTP because it exposes:

- `GET /healthz`
- `GET /readyz`
- `GET /status`

## Channel Model

The currently supported transport set is:

- `email`
- `slack`
- `telegram`
- `feishu`

Each channel owns:

- its transport-specific config
- inbound normalization
- outbound formatting and delivery
- any channel-specific file/media handling

## Local Provider Support

Local providers are treated as normal backends when they expose an OpenAI-style API. The runtime does not require an API key for known local engines such as:

- `ollama`
- `vllm`

Custom local gateways can also be configured through the `custom` provider entry.

## MCP Integration

Enabled MCP servers are connected during agent startup. Their tool definitions are registered into the same tool registry as the built-in filesystem, shell, web, cron, and messaging tools.

Current transport support:

- `stdio`

The startup path validates MCP configuration before the runtime begins serving traffic.

## Testing Strategy

`rbot` uses both module-local unit tests and integration tests:

- unit tests live next to the code for parsing, config validation, and loader behavior
- integration tests under `tests/` exercise runtime flow, channel behavior, and backend wiring

The split keeps low-level behavior close to its implementation while preserving end-to-end coverage for the public runtime surfaces.
