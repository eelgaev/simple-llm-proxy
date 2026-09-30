/**
 * opencode plugin: list every model behind simple-llm-proxy automatically.
 *
 * OpenCode 2 has no built-in discovery for this OpenAI-compatible proxy. This
 * plugin registers a provider and its models from the proxy's GET /v1/models
 * when OpenCode loads it.
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
import { Model, Plugin, Provider } from "@opencode/plugin"

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

async function discoverModels(providerID: ReturnType<typeof Provider.ID.make>): Promise<Model.Info[]> {
  const response = await fetch(`${BASE_URL}/v1/models`, {
    headers: TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {},
    signal: AbortSignal.timeout(TIMEOUT_MS),
  })
  if (!response.ok) throw new Error(`${BASE_URL}/v1/models returned ${response.status}`)

  const body = (await response.json()) as { data?: ProxyModel[]; models?: ProxyListing[] }
  if (!body.data?.length) return []

  // llama.cpp reports capabilities in its second top-level block, keyed by name.
  const listings = new Map((body.models ?? []).map((entry) => [entry.model, entry]))

  const models: Model.Info[] = []
  for (const model of body.data) {
    // n_ctx is what the model is actually served at; n_ctx_train is only what it
    // was trained for, so never fall back to it — that would overstate the limit.
    const reportedContext = model.meta?.n_ctx ?? model.max_model_len
    const context =
      typeof reportedContext === "number" && Number.isSafeInteger(reportedContext) && reportedContext > 0
        ? reportedContext
        : undefined
    const multimodal = listings.get(model.id)?.capabilities?.includes("multimodal") ?? false

    const modelID = Model.ID.make(model.id)
    const defaults = Model.Info.default(providerID, modelID)
    models.push({
      ...defaults,
      name: model.aliases?.[0] ?? model.id,
      capabilities: {
        ...defaults.capabilities,
        tools: true,
        input: multimodal ? ["text", "image"] : ["text"],
        output: ["text"],
      },
      // Model.Info.default assumes 200k context. Zero means unknown when the
      // backend doesn't report a served context window.
      limit: context
        ? { ...defaults.limit, context, output: Math.min(32768, Math.max(1, Math.floor(context / 4))) }
        : { ...defaults.limit, context: 0 },
    })
  }

  return models
}

export default Plugin.define({
  id: "llm-proxy.models",
  async setup(ctx) {
    const providerID = Provider.ID.make(PROVIDER_ID)
    let models: Model.Info[]
    try {
      models = await discoverModels(providerID)
    } catch (error) {
      // A proxy that's down must not stop opencode from starting. One line, no
      // stack — this prints above the model list on every invocation.
      const reason = error instanceof Error ? error.message : String(error)
      console.error(`[${PROVIDER_ID}] model discovery failed: ${reason}`)
      return
    }
    if (!models.length) return

    await ctx.provider.transform((editor) => {
      const existing = editor.get(PROVIDER_ID)
      const settings = { baseURL: `${BASE_URL}/v1`, ...(TOKEN ? { apiKey: TOKEN } : {}) }
      if (!existing) {
        editor.add({
          info: {
            ...Provider.Info.empty(providerID),
            name: PROVIDER_NAME,
            activation: "enabled",
            package: "@opencode/ai/providers/openai-compatible",
            settings,
          },
          models,
        })
        return
      }

      // Keep provider settings and model definitions from opencode.json(c).
      editor.update(PROVIDER_ID, (provider) => {
        provider.package ||= "@opencode/ai/providers/openai-compatible"
        provider.settings = { ...settings, ...provider.settings }
      })
      editor.models.set(PROVIDER_ID, [
        ...models.filter((model) => !existing.models.has(model.id)),
        ...existing.models.values(),
      ])
    })
  },
})
