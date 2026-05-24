# simple-llm-proxy

A lightweight Rust reverse proxy for OpenAI-compatible LLM backends (llama.cpp, sglang, vllm, etc.) with GPU-set-aware request queuing.

## What it does

- Sits in front of multiple LLM servers and exposes a single OpenAI-compatible API
- Groups backends by GPU set — servers sharing a GPU only process one inference request at a time, while different GPU sets work in parallel
- Streams responses (SSE) directly from backends with zero buffering
- Authenticates incoming requests via configurable API tokens

## Configuration

Create a `config.toml`:

```toml
listen = "0.0.0.0:8080"
api_tokens = ["sk-my-token-1", "sk-my-token-2"]

[servers]
"serverA-gpu01" = ["http://127.0.0.1:8081", "http://127.0.0.1:8082"]
"serverA-gpu23" = ["http://127.0.0.1:8083"]
```

- **listen** — address and port the proxy binds to
- **api_tokens** — list of Bearer tokens clients must use to authenticate
- **servers** — GPU sets mapping. Each key is a GPU set name, each value is a list of backend URLs. Servers within the same GPU set share GPU resources, so only one request is processed at a time per set. Different GPU sets run in parallel.

## Usage

```sh
cargo build --release
./target/release/simple-llm-proxy config.toml
```

Or during development:

```sh
cargo run -- config.toml
```

If no config path is given, it defaults to `config.toml` in the current directory.

## API Endpoints

- `POST /v1/chat/completions` — proxied to a backend
- `POST /v1/completions` — proxied to a backend
- `GET /v1/models` — aggregates models from all backends

All endpoints require `Authorization: Bearer <token>` with a token from your config.

## How queuing works

When a request arrives, the proxy races to acquire a slot on any GPU set. If all sets are busy, the request waits in a FIFO queue. Once a set is free, the request is forwarded to one of its servers (round-robin). The slot is held until the entire response (including streaming) completes.

## Logging

Set the `RUST_LOG` environment variable to control log verbosity:

```sh
RUST_LOG=info cargo run -- config.toml
RUST_LOG=debug cargo run -- config.toml
```
