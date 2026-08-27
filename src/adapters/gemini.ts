import type { ModelAdapter } from "../adapter.js";
import { AdapterError, type ChatRequest, type ChatChunk, type TokenUsage } from "../types.js";
import { mergeGeminiUsage } from "./gemini-usage.js";

// ---------------------------------------------------------------------------
// Gemini adapter (Google AI Studio, native API — not the OpenAI-compat shim).
//
// Native Gemini uses `contents[].parts[]` with roles "user" / "model", and a
// separate top-level `systemInstruction`. We translate the neutral format to
// that on the way in, and parse its SSE stream back to neutral ChatChunks on
// the way out. This is the whole "provider quirk" surface — isolated here.
//
// Token accounting is part of that quirk surface and lives next door in
// gemini-usage.ts: thinking tokens bill as output, cache hits bill cheaper. The
// adapter reports counts and nothing else — it never sees a price or a margin.
// ---------------------------------------------------------------------------

const BASE = "https://generativelanguage.googleapis.com/v1beta/models";

interface GeminiPart { text?: string; inlineData?: { mimeType: string; data: string } }
interface GeminiContent { role: "user" | "model"; parts: GeminiPart[] }

function toGemini(req: ChatRequest): {
  systemInstruction?: { parts: GeminiPart[] };
  contents: GeminiContent[];
  generationConfig: Record<string, unknown>;
} {
  const system = req.messages.filter((m) => m.role === "system").map((m) => m.content).join("\n");
  const contents: GeminiContent[] = req.messages
    .filter((m) => m.role !== "system")
    .map((m) => {
      // Attachments (images / PDFs / text) come first, then the text — Gemini
      // reads the parts in order.
      const parts: GeminiPart[] = [];
      for (const att of m.attachments ?? []) {
        if (att.data) parts.push({ inlineData: { mimeType: att.mimeType, data: att.data } });
      }
      if (m.content) parts.push({ text: m.content });
      if (parts.length === 0) parts.push({ text: "" });
      return { role: m.role === "assistant" ? "model" : "user", parts };
    });

  return {
    ...(system ? { systemInstruction: { parts: [{ text: system }] } } : {}),
    contents,
    generationConfig: {
      ...(req.temperature != null ? { temperature: req.temperature } : {}),
      // On Gemini 3.x THINKING models the thinking tokens count AGAINST
      // maxOutputTokens — so a bare maxOutputTokens=answer lets thinking eat the
      // budget and truncates the visible answer mid-sentence. Give the visible
      // answer its full budget ON TOP of the thinking budget. The server's
      // ceiling already reserves (answer + thinking) output tokens, so this stays
      // within what was billed.
      ...(req.maxTokens != null
        ? { maxOutputTokens: req.maxTokens + (req.thinkingBudget ?? 0) }
        : {}),
      // Hard cap on thinking so it cannot exceed what the ceiling reserved.
      ...(req.thinkingBudget != null ? { thinkingConfig: { thinkingBudget: req.thinkingBudget } } : {}),
      // Picture size — Gemini 3.x image models only (2.5 rejects imageConfig.imageSize).
      ...(req.imageSize && req.model.includes("image") && !req.model.startsWith("gemini-2.5")
        ? { imageConfig: { imageSize: req.imageSize } }
        : {}),
    },
  };
}

export const geminiAdapter: ModelAdapter = {
  // Order = dropdown order; the first entry is the app's default. Flash-Lite
  // leads the free tier (~1000-1500 req/day, 15-30 RPM) vs. gemini-3.6-flash's
  // stingy 20/day — so a Lite model is the sensible test default. All are passed
  // straight to Google; an ID your key can't use just returns a clean error, so
  // pick whichever the dropdown shows working.
  //
  // Every id here needs an entry in pricing.json, or it bills at the
  // conservative default and the client shows "unlisted model".
  models: [
    "gemini-3.5-flash-lite",   // current GA Lite — accessible + generous free quota
    "gemini-flash-latest",     // auto-updates to newest Flash (currently 3.6 — only 20/day free)
    "gemini-3.5-flash",        // pinned GA Flash
  ],
  vendor: "google",
  kind: "text",
  apiKeyEnv: "GEMINI_API_KEY",
  trainsOnInput: true, // free tier trains on input (outside EU/UK/EEA) — surfaced to the client
  acceptsImages: true, // Gemini reads images, PDFs and text via inlineData

  // Live text/multimodal Gemini models this key can call (image-OUTPUT models are
  // the separate gemini-image adapter's job, so they're excluded here). The server
  // then keeps only the priced ones.
  async discover(apiKey: string): Promise<string[]> {
    const res = await fetch(`${BASE}?key=${apiKey}&pageSize=200`, { signal: AbortSignal.timeout(15_000) });
    if (!res.ok) throw new Error(`gemini /models ${res.status}`);
    const body = (await res.json()) as { models?: Array<{ name?: string; supportedGenerationMethods?: string[] }> };
    return (body.models ?? [])
      .filter((m) => (m.supportedGenerationMethods ?? []).includes("generateContent"))
      .map((m) => (m.name ?? "").replace(/^models\//, ""))
      .filter((id) => /^gemini-/.test(id))
      .filter((id) => !/(embedding|aqa|image|imagen|tts|native-audio|live|veo|robotics|computer-use|video-understanding)/i.test(id));
  },

  async *stream(req: ChatRequest, apiKey: string): AsyncGenerator<ChatChunk> {
    const url = `${BASE}/${req.model}:streamGenerateContent?alt=sse&key=${apiKey}`;
    const res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(toGemini(req)),
    });

    if (!res.ok || !res.body) {
      const detail = await res.text().catch(() => "");
      throw new AdapterError("google", res.status, `gemini request failed: ${res.status} ${detail}`);
    }

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let usage: TokenUsage | null = null;

    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });

      // SSE frames are separated by a blank line; each data line is JSON.
      let nl: number;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line.startsWith("data:")) continue;

        const json = line.slice(5).trim();
        if (!json || json === "[DONE]") continue;

        try {
          const evt = JSON.parse(json);
          const text: string =
            evt?.candidates?.[0]?.content?.parts?.map((p: GeminiPart) => p.text).join("") ?? "";
          // Not inside the `if (text)` branch on purpose: the frame that carries
          // the final, authoritative usageMetadata often carries no text at all.
          usage = mergeGeminiUsage(usage, evt?.usageMetadata);
          if (text) yield { delta: text, done: false };
        } catch {
          // partial JSON across chunk boundary — push it back and wait for more
          buf = line + "\n" + buf;
          break;
        }
      }
    }

    yield { delta: "", done: true, ...(usage ? { usage } : {}) };
  },
};
