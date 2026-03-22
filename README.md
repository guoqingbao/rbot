# rbot

`rbot` is a Rust-native autonomous bot runtime for persistent chat automation, tool execution, scheduled work, and multi-channel message delivery.

It provides:

- A long-running agent runtime with persistent sessions and memory files
- Filesystem, shell, web fetch, web search, messaging, cron, and background-task tools
- OpenAI-compatible provider integration, including local engines that expose OpenAI-style APIs
- MCP stdio tool integration for external tool servers
- Built-in skills for software engineering, research/reporting, GitHub/CI, and scheduled operations
- Channel backends for `email`, `slack`, `telegram`, and `feishu`
- A gateway process with webhook ingress, health checks, readiness checks, Prometheus metrics, and a web admin UI

## Docs

- [Getting Started](./docs/USAGE.md)
- [Architecture](./docs/ARCHITECTURE.md)
- [Operations Guide](./docs/OPERATIONS.md)

## Quick Start

Initialize config and workspace:

```bash
cargo run -- onboard
```

Start the backend:

```bash
cargo run -- run
```

Check runtime configuration and local state:

```bash
cargo run -- status
cargo run -- sessions
cargo run -- jobs
```

Run a one-shot prompt:

```bash
cargo run -- chat "summarize the repository structure"
```

Open the interactive shell:

```bash
cargo run -- repl
```

The CLI includes streamed responses, persistent history, local shell commands such as `/help` and `/clear`, and agent commands such as `/new`, `/status`, and `/stop`.

## Provider Modes

`rbot` supports both remote and local OpenAI-compatible backends.

Examples:

- OpenAI API
- Ollama at `http://localhost:11434/v1`
- vLLM at `http://localhost:8000/v1`
- LM Studio or any other OpenAI-compatible local server via `providers.custom.apiBase`

See [Getting Started](./docs/USAGE.md) for full config examples.

## Runtime Surfaces

- `email`: IMAP polling + SMTP send
- `slack`: webhook ingress + send
- `telegram`: webhook ingress + send
- `feishu`: webhook ingress + send, including inbound media/resource handling
- `mcp`: stdio-based external tool servers exposed as native tools

The gateway exposes:

- `GET /healthz`
- `GET /readyz`
- `GET /status`
- `GET /metrics`
- `GET /admin`
- `GET /api/admin/*`

## Verification

```bash
cargo fmt
cargo test
```
