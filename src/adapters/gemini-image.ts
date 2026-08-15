import type { ModelAdapter } from "../adapter.js";
import { AdapterError, type ChatRequest, type ChatChunk, type GeneratedImage } from "../types.js";
import { parseGeminiUsage } from "./gemini-usage.js";

// ---------------------------------------------------------------------------
// Gemini image adapter — "Nano Banana".
//
// Same provider and the same request shape as the text adapter, but two
// differences that matter enough to justify a separate file:
//
//   1. NO STREAMING. These models expose generateContent only — no
//      streamGenerateContent — so the answer arrives in one piece. We yield a
//      single terminal chunk. The meter and the protocol both already handle
//      that (an adapter is free to emit one chunk), so nothing above changes.
//
//   2. The payload is enormous. A 1024px image is ~1-2 MB of base64, roughly a
//      thousand Sphinx packets. The client has to budget reply SURBs for that
//      BEFORE sending, which is why adapters now declare `kind` — see
//      adapter.ts.
//
// responseModalities must include IMAGE or the model answers with text about
// the picture it would have drawn.
//
// Model ids below were read from the live models endpoint, not guessed. Display
// names are Google's own: gemini-2.5-flash-image is "Nano Banana",
// gemini-3.1-flash-image is "Nano Banana 2", the *-lite variant is
// "Nano Banana 2 Lite".
// ---------------------------------------------------------------------------

const BASE = "https://generativelanguage.googleapis.com/v1beta/models";

interface GeminiPart {
  text?: string;
  inlineData?: { mimeType: string; data: string };
}

export const geminiImageAdapter: ModelAdapter = {
  // Flash/Lite only. The Pro image models (gemini-3-pro-image,
  // nano-banana-pro-preview) are deliberately left out for now — they are the
  // expensive tier and nothing here needs them yet.
  models: [
    "gemini-3.1-flash-lite-image", // Nano Banana 2 Lite — cheapest
    "gemini-3.1-flash-image",      // Nano Banana 2
    "gemini-2.5-flash-image",      // Nano Banana (original)
  ],
  vendor: "google",
  kind: "image",
  apiKeyEnv: "GEMINI_API_KEY",
  trainsOnInput: true,

  async *stream(req: ChatRequest, apiKey: string): AsyncGenerator<ChatChunk> {
    // Image models take the prompt as plain turns; a system instruction is
    // folded into the first user turn since there is nothing to steer beyond
    // the description itself.
    const system = req.messages.filter((m) => m.role === "system").map((m) => m.content).join("\n");
    const turns = req.messages.filter((m) => m.role !== "system");
    const contents = turns.map((m, i) => ({
      role: m.role === "assistant" ? "model" : "user",
      parts: [{ text: i === 0 && system ? `${system}\n\n${m.content}` : m.content }],
    }));

    const res = await fetch(`${BASE}/${req.model}:generateContent`, {
      method: "POST",
      headers: {
        // Header rather than ?key= — a URL-borne secret ends up in proxy and
        // access logs; a header does not.
        "x-goog-api-key": apiKey,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        contents,
        generationConfig: {
          responseModalities: ["TEXT", "IMAGE"],
          ...(req.temperature != null ? { temperature: req.temperature } : {}),
        },
      }),
    });

    if (!res.ok) {
      const detail = await res.text().catch(() => "");
      throw new AdapterError("google", res.status, describe(res.status, req.model, detail));
    }

    const body = (await res.json()) as {
      candidates?: Array<{ content?: { parts?: GeminiPart[] } }>;
      usageMetadata?: Parameters<typeof parseGeminiUsage>[0];
    };

    const parts = body.candidates?.[0]?.content?.parts ?? [];
    const images: GeneratedImage[] = [];
    let text = "";

    for (const p of parts) {
      if (p.inlineData?.data) {
        images.push({ mimeType: p.inlineData.mimeType || "image/png", data: p.inlineData.data });
      } else if (p.text) {
        text += p.text;
      }
    }

    if (!images.length && !text) {
      throw new AdapterError("google", 502, `${req.model} returned neither an image nor text`);
    }

    const usage = parseGeminiUsage(body.usageMetadata);
    yield {
      delta: text,
      done: true,
      ...(images.length ? { images } : {}),
      ...(usage ? { usage } : {}),
    };
  },
};

/**
 * Turn Google's error into something a user can act on.
 *
 * The 429 case is the one worth naming explicitly: for every image model the
 * free-tier quota is `limit: 0` — not exhausted, but absent. Reading that as
 * "try again later" wastes real time, so we say what it actually means.
 */
function describe(status: number, model: string, detail: string): string {
  if (status === 429 && /limit: 0/.test(detail)) {
    return (
      `${model} has no free-tier quota (limit: 0) — image generation on this key ` +
      `requires billing enabled on the Google Cloud project. ` +
      `See https://aistudio.google.com/apikey`
    );
  }
  if (status === 429) return `${model}: rate limited — ${firstLine(detail)}`;
  if (status === 404) return `${model} is not available to this key`;
  return `${model} request failed: ${status} ${firstLine(detail)}`;
}

function firstLine(detail: string): string {
  try {
    const j = JSON.parse(detail) as { error?: { message?: string } };
    return (j.error?.message ?? detail).split("\n")[0]!.trim();
  } catch {
    return detail.slice(0, 200);
  }
}
