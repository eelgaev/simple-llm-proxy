# Examples

Two drop-in pieces for running simple-llm-proxy in practice.

## `nginx.conf` — TLS in front of the proxy

A vhost that terminates TLS and forwards to the proxy on `127.0.0.1:8000`.

```sh
cp examples/nginx.conf /etc/nginx/sites-available/llm.conf
ln -s /etc/nginx/sites-available/llm.conf /etc/nginx/sites-enabled/
# edit server_name, the certificate paths, and the proxy_pass port
nginx -t && systemctl reload nginx
```

Two nginx defaults are wrong for an LLM endpoint, and the file turns both off:

| directive | default | why it's set |
| --- | --- | --- |
| `proxy_request_buffering` | on | nginx otherwise reads the whole request body before contacting the proxy, undoing the proxy's own request streaming |
| `proxy_read_timeout` | 60s | measures time to *first byte*, so a request queued behind a long generation, or a cold prefill at large context, dies as a bare 504 |

`proxy_buffering off` (response streaming) and `proxy_http_version 1.1` are set for
the same reason — SSE tokens should reach the client as the backend emits them, and
unbuffered request bodies need HTTP/1.1 upstream.

`client_max_body_size` is the binding limit on multimodal payloads; the proxy
itself doesn't cap body size.

## `opencode-plugin.ts` — auto-list every model in opencode

[opencode](https://opencode.ai) has no discovery for OpenAI-compatible providers —
its docs have you hand-write each model id under `provider.<id>.models`. This plugin
fills that map from `GET /v1/models` at startup, so a model added or swapped on a
backend shows up without editing config.

```sh
cp examples/opencode-plugin.ts ~/.config/opencode/plugins/llm-proxy-models.ts
export LLM_PROXY_URL=https://llm.example.com
export LLM_PROXY_TOKEN=<a token from config.toml api_tokens>
opencode models        # models appear under llm-proxy/...
```

| variable | default | meaning |
| --- | --- | --- |
| `LLM_PROXY_URL` | `http://127.0.0.1:8000` | proxy root, no trailing `/v1` |
| `LLM_PROXY_TOKEN` | *(none)* | bearer token from `config.toml` |
| `LLM_PROXY_ID` | `llm-proxy` | provider id in model names |
| `LLM_PROXY_NAME` | the proxy's host | display name in the model picker |

Use `.opencode/plugins/` instead of `~/.config/opencode/plugins/` to scope it to one
project.

What each model gets, from what the backend reported:

- **context limit** — llama.cpp's `meta.n_ctx`, or `max_model_len` for vLLM/sglang.
  Omitted entirely when neither is present, rather than guessing a number opencode
  would then budget against. `n_ctx_train` is deliberately never used — it's the
  trained context, not what the model is being served at.
- **image input** — from `multimodal` in llama.cpp's `models[].capabilities`.
- **output limit** — `min(32768, context / 4)`, since no backend reports a
  generation cap.
- **tool calling** — asserted for every model; the proxy has no way to know which
  backends actually support it.

Anything you write by hand in `opencode.json` wins over discovery, so you can pin a
cost or a smaller context for one model. If the proxy is unreachable the plugin logs
one line and leaves the config alone rather than blocking startup.

### If models don't show up

The error line tells you which failure it is:

- `model discovery failed: ... returned 401` — the plugin loaded but has no valid
  token. Common when a GUI wrapper (T3 Code and friends) starts opencode as a child
  process: it only passes on the environment *it* was started with, so shell exports
  never reach it.
- `model discovery failed: Unable to connect` — wrong `LLM_PROXY_URL`, or the proxy
  is down.
- **no line at all** — the plugin was never loaded. Check the filename ends in `.ts`
  and that opencode isn't running with `--pure` (which skips external plugins) or
  pointing `OPENCODE_CONFIG_DIR` at a different directory.
