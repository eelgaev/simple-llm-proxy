# simple-llm-proxy

A lightweight Rust reverse proxy for OpenAI-compatible LLM backends (llama.cpp, sglang, vllm, etc.) with GPU-set-aware request queuing. Also proxies llama.cpp's Anthropic-compatible Messages API.

## What it does

- Sits in front of multiple LLM servers and exposes a single OpenAI-compatible API
- Groups backends by GPU set — servers sharing a GPU only process one inference request at a time, while different GPU sets work in parallel
- Streams responses (SSE) directly from backends with zero buffering
- Authenticates incoming requests via configurable API tokens

## Configuration

Create a `config.toml`:

```ini
listen = "0.0.0.0:8000"
api_tokens = ["sk-my-token-1", "sk-my-token-2"]

[servers]
"shared-gpu-set" = [
  "https://backend-a.example.com",
  "https://backend-b.example.com",
]
"llama-backend" = [{ url = "https://llm.example.com", token = "<redacted>" }] # llama.cpp
"vllm-backend" = [{ url = "https://llm2.example.com", token = "<redacted>" }] # vLLM
```

- **listen** — address and port the proxy binds to
- **api_tokens** — list of Bearer tokens clients must use to authenticate
- **servers** — GPU sets mapping. Each key is a GPU set name, each value is a list of backend URLs or `{ url, token }` objects for backends that require their own bearer token. Servers within the same GPU set share GPU resources, so only one request is processed at a time per set. Different GPU sets run in parallel.

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
- `POST /v1/embeddings` — proxied to a backend
- `POST /v1/rerank` — proxied to a backend
- `POST /v1/messages` — proxied to a backend (llama.cpp's Anthropic Messages API)
- `POST /v1/messages/count_tokens` — proxied to a backend (llama.cpp only)
- `GET /v1/models` — aggregates models from all backends
- `GET /health` — liveness check (always `{"status":"ok"}` if the proxy process is up)
- `GET /props` — mirrors llama.cpp's `/props`; proxied to a backend so clients (e.g. pi-llama-cpp)
  can detect server mode and, with `?model=<id>`, per-model status/capabilities. An unknown model
  returns llama.cpp's own "model is not loaded" shape rather than a bare 404.

`/health` and `/props` are intentionally at the root, not under `/v1` — that's where llama.cpp
serves them, and clients that speak the llama.cpp API (like pi-llama-cpp) expect them there too.
Point such clients at the proxy's root URL (e.g. `https://llm.example.com`, not
`https://llm.example.com/v1`) — they append `/v1/...` themselves where needed.

Every proxied POST endpoint routes purely on the `"model"` field in the JSON body — any backend
that speaks that shape works, including llama.cpp, sglang, and vLLM. Endpoints a given backend
doesn't implement (e.g. `/v1/rerank` on a plain chat model, or `/v1/messages` on sglang/vLLM) will
simply return whatever error that backend returns.

All endpoints require `Authorization: Bearer <token>` with a token from your config. If you're
pointing an Anthropic SDK client (or Claude Code) at `/v1/messages`, set `ANTHROPIC_AUTH_TOKEN`
rather than `ANTHROPIC_API_KEY` — the latter sends the key via an `x-api-key` header, which this
proxy (and llama.cpp's Messages API implementation) does not check.

## How queuing works

When a request arrives, the proxy races to acquire a slot on any GPU set. If all sets are busy, the request waits in a FIFO queue. Once a set is free, the request is forwarded to one of its servers (round-robin). The slot is held until the entire response (including streaming) completes.

## Logging

Set the `RUST_LOG` environment variable to control log verbosity:

```sh
RUST_LOG=info cargo run -- config.toml
RUST_LOG=debug cargo run -- config.toml
```

## Examples

An nginx TLS vhost and an opencode plugin that auto-lists every model behind the
proxy: [examples/README.md](examples/README.md).

