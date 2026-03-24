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
# Default config file
Config: /root/.rbot/config.json
Workspace: /root/.rbot/workspace
```

### Config Providers

`rbot` supports both remote and local OpenAI-compatible backends. 🎯
You can configure them interactively:

```bash
cargo run --release -- config --provider
```

Or manually edit `~/.rbot/config.json`. **Examples:**

- 🌐 OpenAI API
- 🐋 Ollama at `http://localhost:11434/v1`
- 🚀 vLLM at `http://localhost:8000/v1`
- 🛠️ LM Studio or any other OpenAI-compatible local server via `providers.custom.apiBase`

### Config Communication Channels

Before starting the backend, you should configure your preferred communication channels (Slack, Telegram, etc.) to enable message ingress and delivery. 📬

Use the interactive configuration tool:

```bash
cargo run --release -- config --channel
```

This tool helps you selectively enable channels, set permissions, and provide required tokens or secrets. For manual configuration or detailed channel options, see [Getting Started](./docs/USAGE.md#5-channel-configuration).

> [!TIP]
> **Slack Users:** To set up your Slack App for use with `rbot` (including Webhook and Socket Mode instructions), refer to the [OpenClaw Slack Manual](https://www.meta-intelligence.tech/en/insight-openclaw-slack).

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
- 🤖 Agent commands such as `/new`, `/clear`, `/status`, and `/stop`

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

### Channel Commands

When messaging the bot through Slack, Telegram, or other channels, you can send these signals as standalone messages:

- `stop` or `/stop` - Immediately stop the current agent task and cancel running subagents.
- `clear`, `new`, `/clear`, or `/new` - Reset the bot's memory for the current session.
- `status` or `/status` - Get the current version and runtime usage stats.
- `help` or `/help` - Show available commands.

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
