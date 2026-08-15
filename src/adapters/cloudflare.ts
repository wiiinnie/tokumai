import type { ModelAdapter } from "../adapter.js";
import { EMPTY_USAGE } from "../billing.js";
import { AdapterError, type ChatRequest, type ChatChunk } from "../types.js";

// ---------------------------------------------------------------------------
// Cloudflare Workers AI — Flux image generation on a real free allowance.
//
// 10 000 Neurons per day at no charge, and the allowance covers image models.
// That makes this the first image provider here that is both free AND good:
// Pollinations is free but weak, Nano Banana is good but has no free tier at
// all (limit: 0).
//
// Endpoint shape:
//   POST /client/v4/accounts/{account}/ai/run/@cf/black-forest-labs/flux-1-schnell
//   { "prompt": "...", "steps": 4 }
//   -> { "result": { "image": "<base64 jpeg>" }, "success": true, "errors": [] }
//
// Note the envelope: Cloudflare answers 200 with success:false for model-level
// failures, so checking res.ok alone silently yields an empty image.
//
// The account id is a path component, not a credential — it identifies which
// account to bill, and leaks nothing on its own. It is therefore read from the
// environment here rather than injected like the token.
// ---------------------------------------------------------------------------

const API = "https://api.cloudflare.com/client/v4/accounts";

/** Friendly id -> Cloudflare's own model path. */
const MODELS: Record<string, string> = {
  "flux-schnell": "@cf/black-forest-labs/flux-1-schnell",
  "flux-2-klein": "@cf/black-forest-labs/flux-2-klein-4b",
  "lucid-origin": "@cf/leonardo/lucid-origin",
};

interface CfResponse {
  result?: { image?: string };
  success?: boolean;
  errors?: Array<{ message?: string; code?: number }>;
}

export const cloudflareAdapter: ModelAdapter = {
  models: Object.keys(MODELS),
  vendor: "cloudflare",
  kind: "image",
  apiKeyEnv: "CLOUDFLARE_API_TOKEN",
  trainsOnInput: false,

  async *stream(req: ChatRequest, apiKey: string): AsyncGenerator<ChatChunk> {
    const account = process.env.CLOUDFLARE_ACCOUNT_ID;
    if (!account) {
      throw new AdapterError(
        "cloudflare",
        400,
        "CLOUDFLARE_ACCOUNT_ID is not set — the token alone is not enough, the account id is part of the URL",
      );
    }

    const path = MODELS[req.model];
    if (!path) throw new AdapterError("cloudflare", 404, `unknown cloudflare model "${req.model}"`);

    const prompt = req.messages
      .filter((m) => m.role === "user")
      .map((m) => m.content)
      .join(" ")
      .trim();
    if (!prompt) throw new AdapterError("cloudflare", 400, "no prompt to draw");

    const res = await fetch(`${API}/${account}/ai/run/${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${apiKey}`, "content-type": "application/json" },
      body: JSON.stringify({ prompt: prompt.slice(0, 2048), steps: 4 }),
      signal: AbortSignal.timeout(120_000),
    });

    const body = (await res.json().catch(() => ({}))) as CfResponse;

    if (!res.ok || body.success === false) {
      const msg = body.errors?.[0]?.message ?? `HTTP ${res.status}`;
      throw new AdapterError("cloudflare", res.status, describe(res.status, req.model, msg));
    }

    const image = body.result?.image;
    if (!image) throw new AdapterError("cloudflare", 502, `${req.model} returned no image`);

    yield {
      delta: "",
      done: true,
      // Documented as base64 usable directly as a data: URI with an image/jpeg
      // prefix, so that is the mime type.
      images: [{ mimeType: "image/jpeg", data: image }],
      // Neurons, not tokens. Nothing meaningful to meter, and the free
      // allowance costs us nothing — so it bills nothing.
      usage: { ...EMPTY_USAGE },
    };
  },
};

function describe(status: number, model: string, msg: string): string {
  if (status === 401 || status === 403) {
    return "cloudflare rejected the credentials — check CLOUDFLARE_API_TOKEN (needs the Workers AI permission) and CLOUDFLARE_ACCOUNT_ID";
  }
  if (status === 429) {
    return `${model}: daily free allowance (10 000 Neurons) is used up — it resets at 00:00 UTC`;
  }
  return `${model} request failed: ${status} ${msg}`;
}
