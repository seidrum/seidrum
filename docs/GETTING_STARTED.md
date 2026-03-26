# Getting Started with Seidrum

## Prerequisites

1. **Rust** (stable) — [rustup.rs](https://rustup.rs)
2. **Docker** — [docker.com/get-started](https://docker.com/get-started) (required for ArangoDB on macOS; on Linux, Podman also works)
3. **At least one LLM API key** — Google (Gemini) or OpenAI

## Install

```bash
git clone https://github.com/seidrum/seidrum.git
cd seidrum
cargo build --workspace --release
```

This builds the `seidrum` CLI, the kernel, and all plugins. Binaries go to `target/release/`.

## Setup

```bash
seidrum setup
```

The setup wizard will:

1. **Check Docker/Podman** — needed for ArangoDB
2. **Download NATS** — a ~20MB binary, installed to `~/.seidrum/bin/`
3. **Pull ArangoDB image** — `arangodb:3.12` Docker image
4. **Prompt for API keys:**
   - `ARANGO_PASSWORD` — auto-generated, you can accept the default
   - `GOOGLE_API_KEY` — for Gemini LLM (blank to skip, disables `llm-google`)
   - `OPENAI_API_KEY` — for embeddings (blank to skip)
   - `TELEGRAM_TOKEN` — for Telegram bot (blank to skip, disables `telegram`)
   - `GATEWAY_API_KEY` — auto-generated for dashboard auth
5. **Initialize the database** — creates ArangoDB collections, indexes, and the root scope
6. **Enable the dashboard** — the web UI at `http://localhost:8080`

To skip prompts and use generated defaults: `seidrum setup --defaults`

## Start

```bash
seidrum start
```

This starts:
- NATS (native binary, port 4222)
- ArangoDB (Docker container, port 8529)
- Kernel (brain, registries, workflow engine)
- All enabled plugins

## Verify

```bash
seidrum status
```

You should see:
```
Infrastructure:
  NATS:     running (PID 12345, port 4222)
  ArangoDB: running (container seidrum-arangodb, port 8529)

Seidrum daemon: running (PID 67890)

+------------------+----------------------------+----------+------+--------+----------+
| Name             | Binary                     | Status   | PID  | Uptime | Restarts |
+------------------+----------------------------+----------+------+--------+----------+
| kernel           | seidrum-kernel             | running  | 123  | 2m     | 0        |
| llm-router       | seidrum-llm-router         | running  | 124  | 2m     | 0        |
| ...              | ...                        | ...      | ...  | ...    | ...      |
+------------------+----------------------------+----------+------+--------+----------+
```

## Dashboard

Open http://localhost:8080/dashboard in your browser.

Enter the API key shown during setup (also in your `.env` file as `GATEWAY_API_KEY`).

The dashboard shows:
- **Overview** — system health, plugin status
- **Plugins** — registered plugins with health checks
- **Capabilities** — tools and commands available to agents
- **Skills** — agent skills stored in the knowledge graph
- **Conversations** — chat history across channels

## Send a test message

If you configured Telegram, message your bot. Otherwise, enable the CLI plugin:

```bash
seidrum plugin enable cli
seidrum plugin start cli
```

Then interact via terminal (the CLI plugin reads from stdin).

## Plugin management

```bash
seidrum plugin list              # See all plugins and their state
seidrum plugin enable email      # Enable a plugin
seidrum plugin disable telegram  # Disable a plugin
seidrum plugin restart llm-router
```

## Stop

```bash
seidrum stop
```

Stops all plugins, the kernel, NATS, and ArangoDB.

## Power-user mode

If you prefer to manage NATS and ArangoDB yourself (e.g., running on a remote server):

```bash
# Start your own NATS and ArangoDB, then:
seidrum daemon start    # Only starts kernel + plugins, assumes infra is running
seidrum daemon stop
```

## Troubleshooting

**Port 4222 or 8529 already in use**: Another NATS or ArangoDB instance is running. Either stop it, or Seidrum will detect it and use the existing one.

**ArangoDB container won't start**: Check Docker is running (`docker info`). Check logs: `docker logs seidrum-arangodb`.

**Plugin keeps crashing**: Check logs in `~/.seidrum/logs/<plugin-name>.log`. The daemon auto-restarts crashed plugins up to 5 times with exponential backoff.

**Missing API key errors**: Run `seidrum setup` again to reconfigure, or edit `.env` directly.

**Dashboard shows "Unauthorized"**: Enter your `GATEWAY_API_KEY` (from `.env`) in the login prompt.

## File locations

| Path | Purpose |
|------|---------|
| `~/.seidrum/bin/` | Downloaded NATS binary |
| `~/.seidrum/data/nats/` | NATS JetStream data |
| `~/.seidrum/logs/` | Process log files |
| `~/.seidrum/pids/` | PID files and metadata |
| `~/.seidrum/infra.yaml` | Infrastructure configuration |
| `config/platform.yaml` | Kernel configuration |
| `config/plugins.yaml` | Plugin manifest |
| `.env` | API keys and secrets |
