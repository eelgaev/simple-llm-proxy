/**
 * opencode plugin: list every model behind simple-llm-proxy automatically.
 *
 * opencode has no built-in discovery for OpenAI-compatible providers — its docs
 * have you hand-write each model id under `provider.<id>.models` in
 * opencode.json. This plugin fills that map at startup from the proxy's
 * `GET /v1/models` instead, so adding or swapping a backend model shows up in
 * opencode without editing config.
 *
 * Context limits come from what the backend reports: llama.cpp's `meta.n_ctx`,
 * or `max_model_len` for vLLM/sglang. Image input is taken from llama.cpp's
 * `models[].capabilities` listing.
 *
 * Install: copy to ~/.config/opencode/plugins/ (global) or .opencode/plugins/
 * (per project), then set the env vars below.
 *
 *   LLM_PROXY_URL       proxy root, no trailing /v1   (default http://127.0.0.1:8000)
 *   LLM_PROXY_TOKEN     bearer token from config.toml api_tokens
 *   LLM_PROXY_ID        provider id shown in opencode (default "llm-proxy")
 *   LLM_PROXY_NAME      display name                  (default: the proxy's host)
 */
import type { Plugin } from "@opencode-ai/plugin"

const BASE_URL = (process.env["LLM_PROXY_URL"] ?? "http://127.0.0.1:8000").replace(/\/+$/, "")
const TOKEN = process.env["LLM_PROXY_TOKEN"] ?? ""
const PROVIDER_ID = process.env["LLM_PROXY_ID"] ?? "llm-proxy"

/** Host of the proxy, so the picker says which endpoint these models come from. */
function hostname(): string {
  try {
    return new URL(BASE_URL).host
  } catch {
    return BASE_URL
  }
}

const PROVIDER_NAME = process.env["LLM_PROXY_NAME"] ?? hostname()

/** Don't hang opencode's startup on an unreachable proxy. */
const TIMEOUT_MS = 5000

type ProxyModel = {
  id: string
  aliases?: string[]
  meta?: { n_ctx?: number; n_ctx_train?: number }
  max_model_len?: number
}

type ProxyListing = { model?: string; capabilities?: string[] }

export async function discoverModels(): Promise<Record<string, unknown> | undefined> {
  const response = await fetch(`${BASE_URL}/v1/models`, {
    headers: TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {},
    signal: AbortSignal.timeout(TIMEOUT_MS),
  })
  if (!response.ok) throw new Error(`${BASE_URL}/v1/models returned ${response.status}`)

  const body = (await response.json()) as { data?: ProxyModel[]; models?: ProxyListing[] }
  if (!body.data?.length) return undefined

  // llama.cpp reports capabilities in its second top-level block, keyed by name.
  const listings = new Map((body.models ?? []).map((entry) => [entry.model, entry]))

  const models: Record<string, unknown> = {}
  for (const model of body.data) {
    // n_ctx is what the model is actually served at; n_ctx_train is only what it
    // was trained for, so never fall back to it — that would overstate the limit.
    const context = model.meta?.n_ctx ?? model.max_model_len
    const multimodal = listings.get(model.id)?.capabilities?.includes("multimodal") ?? false

    // These are the flat keys opencode's provider loader actually reads
    // (`model.attachment`, `model.tool_call`, `model.modalities`, ...). A
    // nested `capabilities: {...}` object is silently ignored.
    models[model.id] = {
      name: model.aliases?.[0] ?? model.id,
      temperature: true,
      tool_call: true,
      attachment: multimodal,
      modalities: {
        input: multimodal ? ["text", "image"] : ["text"],
        output: ["text"],
      },
      // Leave the limit off entirely when the backend didn't say, rather than
      // inventing a number opencode would then budget against.
      ...(context
        ? { limit: { context, output: Math.min(32768, Math.floor(context / 4)) } }
        : {}),
    }
  }

  return models
}

export const LlmProxyModels: Plugin = async () => ({
  async config(input) {
    let models: Record<string, unknown> | undefined
    try {
      models = await discoverModels()
    } catch (error) {
      // A proxy that's down must not stop opencode from starting. One line, no
      // stack — this prints above the model list on every invocation.
      const reason = error instanceof Error ? error.message : String(error)
      console.error(`[${PROVIDER_ID}] model discovery failed: ${reason}`)
      return
    }
    if (!models) return

    // The config shape here is the one the provider loader reads (npm/options/
    // models); it isn't in every published version of the Config type, so this
    // stays loosely typed on purpose.
    const config = input as any
    const providers = (config.provider ??= {})
    const existing = providers[PROVIDER_ID] ?? {}

    providers[PROVIDER_ID] = {
      npm: "@ai-sdk/openai-compatible",
      name: PROVIDER_NAME,
      ...existing,
      options: {
        baseURL: `${BASE_URL}/v1`,
        ...(TOKEN ? { apiKey: TOKEN } : {}),
        ...existing.options,
      },
      // Anything hand-written in opencode.json wins over discovery, so you can
      // still pin a cost or a smaller context for one model.
      models: { ...models, ...existing.models },
    }
  },
})
