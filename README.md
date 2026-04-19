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

## Supported backend types

| Type | `stop` field | Behavior |
|------|-------------|----------|
| Docker Compose | `docker compose ... down` | Runs start/stop commands via shell |
| systemctl | `systemctl stop <service>` | Runs start/stop commands via shell |
| Managed subprocess | `managed-subprocess` | Spawns the `start` command as a child process, kills it on stop (SIGTERM → SIGKILL) |

## Installation

```bash
cargo build --release
# Binary at target/release/llm-doze
```

## Usage

```bash
# Use default config.yaml in current directory
llm-doze

# Specify config file and bind address
llm-doze --config /etc/llm-doze/config.yaml --bind 0.0.0.0

# Set log level
RUST_LOG=debug llm-doze --config config.yaml
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
