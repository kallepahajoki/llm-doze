# LLM-Doze

A Rust-based reverse proxy that automatically starts and stops LLM backend services on demand.

## Why?

- **No power draw when idle** — GPU servers only run when there are active requests
- **Authentication** — Bearer token auth in front of services that don't have their own
- **Localhost isolation** — Bind backends to 127.0.0.1, expose only the proxy on 0.0.0.0
- **Model-based routing** — Multiple models on a single port, routed by the `model` field in the request

## How it works

```
Client → [LLM-Doze proxy :8000] → [vLLM backend localhost:8900]
                                      ↑ started on first request
                                      ↓ stopped after idle timeout
```

1. A request arrives at the proxy port
2. If multiple routes share the port, the request body is inspected for the `model` field to pick the right backend
3. If the backend is stopped, LLM-Doze starts it and waits for the health check to pass
4. The request is forwarded to the backend
5. After no requests for `idle_timeout` seconds, the backend is automatically stopped

On startup, LLM-Doze probes each backend's health endpoint to detect already-running services and track them for idle shutdown.

## Backend lifecycle

Any service that can be started and stopped with a shell command will work. The `start` and `stop` fields are run via `sh -c`, so anything you can run in a terminal is supported.

There is one special mode: setting `stop: managed-subprocess` makes LLM-Doze spawn the `start` command as a child process and manage its lifetime directly (SIGTERM, then SIGKILL after a grace period). This is useful for services that don't have their own start/stop mechanism.

**Examples:**

| Backend | `start` | `stop` |
|---------|---------|--------|
| Docker Compose | `docker compose -f ... up -d` | `docker compose -f ... down` |
| systemctl | `systemctl start ollama` | `systemctl stop ollama` |
| Direct process | `/usr/bin/llama-server -m model.gguf --port 8091` | `managed-subprocess` |

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
llm-doze serve

# Specify config file and bind address
llm-doze --config /path/to/config.yaml serve --bind 0.0.0.0

# Set log level
RUST_LOG=debug llm-doze serve

# Check status of all backends
llm-doze status
```

### Status

`llm-doze status` connects to the running process via a unix socket (`/run/llm-doze.sock`) and shows live state, idle time, and timeout for each backend:

```
NAME            PORT  BACKEND          STATUS              IDLE  TIMEOUT
────────────  ──────  ───────────────  ────────────  ──────────  ───────
vllm-qwen3.6    8000  localhost:8900   ● running       3m 42s     600s
ollama         11434  localhost:11435  ○ stopped              -     600s
reranker        8090  localhost:8091   ○ stopped              -     300s
```

## Configuration

See [config.sample.yaml](config.sample.yaml) for a full example.

The config is organized as **listeners** (one per port), each with one or more **routes** (backends):

```yaml
auth:
  token: "your-secret-token"

listeners:
  - port: 8000
    routes:
      - name: my-llm
        backend: localhost:8900
        start: docker compose up -d
        stop: docker compose down
        health: /health           # default: /health
        idle_timeout: 600         # seconds before auto-stop (default: 600)
        startup_timeout: 300      # max wait for health check (default: 300)
        startup_poll_interval: 2  # poll interval in seconds (default: 2)
```

### Model-based routing

Multiple models can share a single port. The proxy inspects the `model` field in the JSON request body and routes to the matching backend. Each model has its own independent lifecycle.

```yaml
listeners:
  - port: 8000
    routes:
      - name: large-model
        model: Large-70B        # matches {"model": "Large-70B"} in requests
        backend: localhost:8900
        start: docker compose -f large.yml up -d
        stop: docker compose -f large.yml down

      - name: small-model
        model: Small-7B
        backend: localhost:8901
        start: docker compose -f small.yml up -d
        stop: docker compose -f small.yml down
```

Single-route listeners don't need a `model` field — requests are forwarded directly without body inspection.

`GET /v1/models` on a multi-route listener returns the list of available models.

### Authentication

Auth is resolved in three tiers: route > listener > global. Each level can override or disable auth:

```yaml
auth:
  token: "global-token"

listeners:
  - port: 8000
    auth:
      token: "listener-token"     # overrides global for all routes on this port
    routes:
      - name: public-model
        auth:
          enabled: false          # no auth for this route
      - name: private-model
        model: Private-7B
        auth:
          token: "route-token"    # overrides listener token
```

Clients authenticate with: `Authorization: Bearer <token>`

## Testing

```bash
cargo test
```
