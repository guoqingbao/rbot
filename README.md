# 🤖 rbot: A Minimal AI Agent in Rust for Automation and Development

`rbot` is a Rust-native autonomous bot runtime for persistent chat automation, tool execution, scheduled work, and multi-channel message delivery. 🚀

## ✨ Features

- 🧠 **Persistent Agent Runtime** - Long-running agent runtime with persistent sessions and memory files
- 🛠️ **Rich Toolset** - Filesystem, shell, web fetch, web search, messaging, cron, and background-task tools
- 🌐 **Provider Integration** - OpenAI-compatible provider integration, including local engines that expose OpenAI-style APIs
- 🔌 **MCP Support** - MCP stdio tool integration for external tool servers
- 🧩 **Built-in Skills** - Software engineering, research/reporting, GitHub/CI, and scheduled operations
- 📬 **Multi-Channel** - Channel backends for `email`, `slack`, `telegram`, and `feishu`
- 🌐 **Gateway Process** - Webhook ingress, health checks, readiness checks, Prometheus metrics, and a web admin UI

## 📚 Documentation

- [🚀 Getting Started](./docs/USAGE.md)
- [🏗️ Architecture](./docs/ARCHITECTURE.md)
- [⚙️ Operations Guide](./docs/OPERATIONS.md)

## ⚡ Quick Start

### Initialize config and workspace:

```bash
cargo run --release -- onboard
```

This will generate:

```python
# Revise this config file first
Config: /root/.rbot/config.json
Workspace: /root/.rbot/workspace
```

### Config Providers

`rbot` supports both remote and local OpenAI-compatible backends. 🎯

**Examples:**

- 🌐 OpenAI API
- 🐋 Ollama at `http://localhost:11434/v1`
- 🚀 vLLM at `http://localhost:8000/v1`
- 🛠️ LM Studio or any other OpenAI-compatible local server via `providers.custom.apiBase`

See [Getting Started](./docs/USAGE.md) to configure local or remote providers in `/root/.rbot/config.json`.

### Run a one-shot prompt:

```bash
cargo run --release -- chat "summarize the repository structure"
```

### Open the interactive shell for vibe coding:

```bash
cargo run --release -- repl
```

The CLI includes:
- 📡 Streamed responses
- 📜 Persistent history
- 💻 Local shell commands such as `/help` and `/clear`
- 🤖 Agent commands such as `/new`, `/status`, and `/stop`

### Start the backend (Personal AI Assistant):

```bash
cargo run --release -- run
```

This starts the runtime and gateway even if no inbound channels are enabled yet.
With zero channels configured, `rbot` still serves the admin/status surfaces; enable
`email`, `slack`, `telegram`, or `feishu` in `/root/.rbot/config.json` when you want
message ingress and delivery.

### Check runtime configuration and local state:

```bash
cargo run --release -- status
cargo run --release -- sessions
cargo run --release -- jobs
```

## 📡 Runtime Surfaces

### Channel Backends

- 📧 **email**: IMAP polling + SMTP send
- 💬 **slack**: webhook ingress + send
- ✈️ **telegram**: webhook ingress + send
- 🦘 **feishu**: webhook ingress + send, including inbound media/resource handling
- 🔌 **mcp**: stdio-based external tool servers exposed as native tools

### Gateway Endpoints

The gateway exposes:

- ✅ `GET /healthz` - Health check
- 🟢 `GET /readyz` - Readiness check
- 📊 `GET /status` - Runtime status
- 📈 `GET /metrics` - Prometheus metrics
- 🎛️ `GET /admin` - Web admin UI
- 🔧 `GET /api/admin/*` - Admin API

## ✅ Verification

```bash
cargo fmt
cargo test
```

## 🎯 Use Cases

- 🤖 **Personal AI Assistant** - Always-on AI assistant across your communication channels
- 📊 **Automated Monitoring** - Scheduled tasks and webhook-based monitoring
- 🔧 **DevOps Automation** - Tool execution, file operations, and system management
- 📝 **Research & Reporting** - Web search, analysis, and report generation
- 🔄 **CI/CD Integration** - GitHub/CI automation and status updates

---

**Built with ❤️ in Rust** 🦀
