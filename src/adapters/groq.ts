import type { ModelAdapter } from "../adapter.js";
import { AdapterError, type ChatRequest, type ChatChunk, type TokenUsage } from "../types.js";
import { EMPTY_USAGE } from "../billing.js";

// ---------------------------------------------------------------------------
// Groq — fast Llama inference with a genuinely usable free tier.
//
// Why this is worth having alongside Gemini: Google's free text quota is
// stingy, and Groq's is not. Documented free-tier limits at the time of
// writing:
//
//   llama-3.3-70b-versatile   30 req/min,  1 000 req/day
//   llama-3.1-8b-instant      30 req/min, 14 400 req/day
//
// Groq speaks the OpenAI chat-completions dialect, so this adapter is mostly
// SSE plumbing. Two details that are easy to get wrong:
//
//   - usage only arrives if you ask for it, via stream_options.include_usage.
//     Without that the meter falls back to counting characters and every bill
//     is an estimate.
//   - the final usage frame carries an EMPTY choices array. Reading
//     choices[0].delta blindly throws on the very last chunk.
// ---------------------------------------------------------------------------

const URL_ = "https://api.groq.com/openai/v1/chat/completions";

interface GroqChunk {
  choices?: Array<{ delta?: { content?: string }; finish_reason?: string | null }>;
  usage?: { prompt_tokens?: number; completion_tokens?: number; total_tokens?: number };
  error?: { message?: string };
}

function toUsage(u: NonNullable<GroqChunk["usage"]>): TokenUsage {
  const inputTokens = u.prompt_tokens ?? 0;
  const outputTokens = u.completion_tokens ?? 0;
  return {
    ...EMPTY_USAGE,
    inputTokens,
    outputTokens,
    totalTokens: u.total_tokens ?? inputTokens + outputTokens,
  };
}

export const groqAdapter: ModelAdapter = {
  models: ["llama-3.3-70b-versatile", "llama-3.1-8b-instant"],
  vendor: "groq",
  kind: "text",
  apiKeyEnv: "GROQ_API_KEY",
  // Groq's terms do not claim training rights over API input the way Google's
  // free tier does. Flagged as zero-retention, but worth re-reading their terms
  // before leaning on that in the UI.
  trainsOnInput: false,

  // Live Groq chat models this key can call, minus audio/safety/embedding models
  // that aren't text chat. The server then keeps only the priced ones and logs
  // any it skipped so the operator can price them.
  async discover(apiKey: string): Promise<string[]> {
    const res = await fetch("https://api.groq.com/openai/v1/models", {
      headers: { authorization: `Bearer ${apiKey}` },
      signal: AbortSignal.timeout(15_000),
    });
    if (!res.ok) throw new Error(`groq /models ${res.status}`);
    const body = (await res.json()) as { data?: Array<{ id?: string }> };
    const KEEP = /^(llama-3|meta-llama\/llama-[34]|openai\/gpt-oss|qwen\/qwen|groq\/compound)/i;
    const DROP = /(whisper|prompt-guard|guard|tts|embed|orpheus|allam|-vision-preview$)/i;
    return (body.data ?? [])
      .map((m) => m.id ?? "")
      .filter((id) => KEEP.test(id) && !DROP.test(id));
  },

  async *stream(req: ChatRequest, apiKey: string): AsyncGenerator<ChatChunk> {
    const res = await fetch(URL_, {
      method: "POST",
      headers: {
        authorization: `Bearer ${apiKey}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: req.model,
        messages: req.messages.map((m) => ({ role: m.role, content: m.content })),
        stream: true,
        stream_options: { include_usage: true },
        ...(req.temperature != null ? { temperature: req.temperature } : {}),
        ...(req.maxTokens != null ? { max_tokens: req.maxTokens } : {}),
      }),
    });

    if (!res.ok || !res.body) {
      const detail = await res.text().catch(() => "");
      throw new AdapterError("groq", res.status, describe(res.status, req.model, detail));
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let usage: TokenUsage | null = null;

    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });

      let nl: number;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line.startsWith("data:")) continue;

        const json = line.slice(5).trim();
        if (!json || json === "[DONE]") continue;

        let evt: GroqChunk;
        try {
          evt = JSON.parse(json);
        } catch {
          // partial JSON split across a chunk boundary — put it back
          buf = line + "\n" + buf;
          break;
        }

        if (evt.error?.message) throw new AdapterError("groq", 502, evt.error.message);
        // The usage frame has no choices at all, so guard rather than index.
        if (evt.usage) usage = toUsage(evt.usage);
        const text = evt.choices?.[0]?.delta?.content;
        if (text) yield { delta: text, done: false };
      }
    }

    yield { delta: "", done: true, ...(usage ? { usage } : {}) };
  },
};

function describe(status: number, model: string, detail: string): string {
  const msg = firstLine(detail);
  if (status === 401) return `groq rejected the key — check GROQ_API_KEY (console.groq.com/keys)`;
  if (status === 429) return `${model}: free-tier rate limit reached — ${msg}`;
  if (status === 404) return `${model} is not available on this account`;
  return `${model} request failed: ${status} ${msg}`;
}

function firstLine(detail: string): string {
  try {
    const j = JSON.parse(detail) as { error?: { message?: string } };
    return (j.error?.message ?? detail).split("\n")[0]!.trim();
  } catch {
    return detail.slice(0, 200);
  }
}
