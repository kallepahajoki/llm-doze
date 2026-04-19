# LLM-Doze

A Rust-based reverse proxy that automatically starts and stops LLM backend services on demand.

## Why?

- **No power draw when idle** — GPU servers only run when there are active requests
- **Authentication** — Bearer token auth in front of services that don't have their own
- **Localhost isolation** — Bind backends to 127.0.0.1, expose only the proxy on 0.0.0.0

## How it works

```
Client → [LLM-Doze proxy :8000] → [vLLM backend localhost:8900]
                                      ↑ started on first request
                                      ↓ stopped after idle timeout
```

1. A request arrives at the proxy port
2. If the backend is stopped, LLM-Doze starts it and waits for the health check to pass
3. The request is forwarded to the backend
4. After no requests for `idle_timeout` seconds, the backend is automatically stopped

On startup, LLM-Doze probes each backend's health endpoint to detect already-running services and track them for idle shutdown.

## Supported backend types

| Type | `stop` field | Behavior |
|------|-------------|----------|
| Docker Compose | `docker compose ... down` | Runs start/stop commands via shell |
| systemctl | `systemctl stop <service>` | Runs start/stop commands via shell |
| Managed subprocess | `managed-subprocess` | Spawns the `start` command as a child process, kills it on stop (SIGTERM → SIGKILL) |

## Installation

### Build

```bash
cargo build --release
```

### Install as systemd service

```bash
# Copy binary
sudo cp target/release/llm-doze /usr/local/bin/

# Create config directory and copy your config
sudo mkdir -p /etc/llm-doze
sudo cp config.yaml /etc/llm-doze/config.yaml

# Install and enable the service
sudo cp llm-doze.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now llm-doze
```

### Logs

LLM-Doze logs to stderr, which systemd captures in journald:

```bash
journalctl -u llm-doze -f            # follow live
journalctl -u llm-doze --since today  # today's logs
```

Set `Environment=RUST_LOG=debug` in the service file for verbose output.

## Usage

```bash
# Start the proxy (default config: /etc/llm-doze/config.yaml)
llm-doze

# Specify config file and bind address
llm-doze --config /path/to/config.yaml --bind 0.0.0.0

# Set log level
RUST_LOG=debug llm-doze

# Check status of all backends
llm-doze status
```

### Status

`llm-doze status` connects to the running process via a unix socket (`/run/llm-doze.sock`) and shows live state, idle time, and timeout for each server:

```
NAME            PORT  BACKEND          STATUS              IDLE  TIMEOUT
────────────  ──────  ───────────────  ────────────  ──────────  ───────
vllm-qwen3.6    8000  localhost:8900   ● running       3m 42s     600s
ollama         11434  localhost:11435  ○ stopped              -     600s
reranker        8090  localhost:8091   ○ stopped              -     300s
```

## Configuration

See [config.sample.yaml](config.sample.yaml) for a full example.

```yaml
auth:
  token: "your-secret-token"

servers:
  - name: my-llm
    listen: 8000              # proxy listens on this port
    backend: localhost:8900   # forward to this address
    start: docker compose up -d
    stop: docker compose down
    health: /health           # health check endpoint (default: /health)
    idle_timeout: 600         # seconds before auto-stop (default: 600)
    startup_timeout: 300      # max wait for health check (default: 300)
    startup_poll_interval: 2  # poll interval in seconds (default: 2)
```

### Authentication

Global auth applies to all servers. Per-server auth overrides the global setting:

```yaml
auth:
  token: "global-token"

servers:
  - name: public-model
    # ...
    auth:
      enabled: false          # no auth for this server

  - name: private-model
    # ...
    auth:
      token: "private-token"  # different token for this server
```

Clients authenticate with: `Authorization: Bearer <token>`

## Testing

```bash
cargo test
```
