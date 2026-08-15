import type { ModelAdapter } from "../adapter.js";
import { EMPTY_USAGE } from "../billing.js";
import { AdapterError, type ChatRequest, type ChatChunk } from "../types.js";

// ---------------------------------------------------------------------------
// Pollinations — free, keyless image generation.
//
// Exists here for one concrete reason: every Gemini image model reports
// `limit: 0` on the free tier, so without this there is no way to exercise the
// image path — SURB budgeting, multi-hundred-kilobyte transfer, reassembly —
// without enabling billing first. This makes that path testable today.
//
// PRIVACY, AND IT IS A REAL DIFFERENCE: this is a third party, and unlike the
// Gemini adapters it takes no API key, which means no account and no contract.
// The prompt goes to a public service in the clear. The mixnet still hides WHO
// is asking — that property is unaffected — but WHAT is asked is visible to an
// operator we have no agreement with. Hence trainsOnInput: true, which surfaces
// in the client as the "trains on input" badge.
//
// Free means free: the adapter reports zero tokens, so a request costs zero
// SCRAI. It is a test instrument, not a product model.
// ---------------------------------------------------------------------------

const BASE = "https://image.pollinations.ai/prompt";

/**
 * Size is the point of this adapter — it is how you vary the payload. Bigger
 * dimensions mean more bytes, more Sphinx packets, more reply SURBs consumed.
 */
const SIZES: Record<string, number> = {
  "pollinations-512": 512,
  "pollinations-1024": 1024,
  "pollinations-1536": 1536,
};

export const pollinationsAdapter: ModelAdapter = {
  models: Object.keys(SIZES),
  vendor: "pollinations",
  kind: "image",
  trainsOnInput: true, // public, keyless, no contract — assume the worst

  async *stream(req: ChatRequest, _apiKey: string): AsyncGenerator<ChatChunk> {
    const size = SIZES[req.model] ?? 1024;
    const prompt = req.messages
      .filter((m) => m.role === "user")
      .map((m) => m.content)
      .join(" ")
      .trim();

    if (!prompt) throw new AdapterError("pollinations", 400, "no prompt to draw");

    const url =
      `${BASE}/${encodeURIComponent(prompt)}` +
      `?width=${size}&height=${size}&nologo=true&safe=true`;

    // Generation is genuinely slow at larger sizes (~45s at 1536px), and that
    // is before the mixnet gets involved — so the timeout is generous.
    const res = await fetch(url, { signal: AbortSignal.timeout(180_000) });
    if (!res.ok) {
      throw new AdapterError("pollinations", res.status, `pollinations returned ${res.status}`);
    }

    const buf = Buffer.from(await res.arrayBuffer());
    if (buf.length < 100) {
      throw new AdapterError("pollinations", 502, "pollinations returned an empty image");
    }

    const mimeType = res.headers.get("content-type")?.split(";")[0] || "image/jpeg";

    yield {
      delta: "",
      done: true,
      images: [{ mimeType, data: buf.toString("base64") }],
      // Zero tokens: this provider costs us nothing, so it bills nothing.
      usage: { ...EMPTY_USAGE },
    };
  },
};
