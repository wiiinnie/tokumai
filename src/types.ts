// ---------------------------------------------------------------------------
// Neutral internal format.
//
// This is the ONLY shape the server and client speak. Every provider lives
// behind an adapter that translates this <-> its own wire format. Adding a new
// model (Claude, Groq, GPT, ...) means writing one adapter, never touching the
// server, the client, or these types.
// ---------------------------------------------------------------------------

export type Role = "system" | "user" | "assistant";

export interface ChatMessage {
  role: Role;
  content: string;
  /**
   * Optional file inputs (images, PDFs, text) for models that accept them
   * (Gemini); others ignore them. Each entry is EITHER inline base64 (`data`)
   * or a reference to a chunk-uploaded file (`uploadId`) that the server resolves
   * to bytes before calling the adapter.
   */
  attachments?: Array<{ mimeType: string; data?: string; uploadId?: string }>;
}

export interface ChatRequest {
  /** Neutral model id, e.g. "gemini-2.5-flash". Resolved to an adapter by the registry. */
  model: string;
  messages: ChatMessage[];
  /** Streaming is the default; the server always streams to the client. */
  temperature?: number;
  maxTokens?: number;
  /**
   * Provider-side cap on thinking tokens, which bill as output. Injected by the
   * server so the reservation can be a true upper bound; never client-controlled,
   * or a client could unbound its own thinking and outrun what it reserved.
   */
  thinkingBudget?: number;
  /** Requested picture size for Gemini 3.x image models: "512" | "1K" | "2K" | "4K". */
  imageSize?: string;
}

/** An image a model produced, exactly as the provider handed it over. */
export interface GeneratedImage {
  /** e.g. "image/png". */
  mimeType: string;
  /** base64, undecoded — it is forwarded as-is and only written out at the edge. */
  data: string;
}

/** One streamed piece of the answer. `done` marks the final chunk. */
export interface ChatChunk {
  delta: string;
  done: boolean;
  /**
   * Images produced by this chunk. Image models answer in one shot rather than
   * streaming, so in practice these ride the final chunk — but the field sits
   * on ChatChunk so a future model that interleaves text and images needs no
   * protocol change.
   */
  images?: GeneratedImage[];
  /** Present only on the final chunk when the provider reports it. */
  usage?: TokenUsage;
  /** Set on the final chunk when the request failed. */
  error?: string;
}

/**
 * Token counts, provider-neutral.
 *
 * `inputTokens` / `outputTokens` are the BILLABLE counts, which is not always
 * what a provider hands you raw:
 *   - thinking tokens are billed as output, so they are folded into outputTokens
 *     and repeated in thoughtTokens for display only — never billed twice
 *   - cache hits are cheaper, so they are carved out of inputTokens into
 *     cachedInputTokens
 *   - audio input has its own rate on some models, hence audioInputTokens
 *
 * Only the first two are required, so an adapter that knows nothing about
 * caches or thinking still satisfies the type.
 */
export interface TokenUsage {
  /** Billable input at the normal rate: uncached, non-audio. */
  inputTokens: number;
  /** Billable output, thinking included. */
  outputTokens: number;
  /** Input served from the provider's context cache, billed at the cache rate. */
  cachedInputTokens?: number;
  /** Audio input tokens, billed at the audio rate where the model has one. */
  audioInputTokens?: number;
  /** Thinking tokens. A subset of outputTokens — display only. */
  thoughtTokens?: number;
  /** The provider's own total, when it reports one. */
  totalTokens?: number;
  /**
   * What this exchange costs and what it sells for. Attached by the server, not
   * by the adapter — an adapter never learns the margin. It rides inside usage
   * so it reaches the client through the same onDone(usage) callback the
   * transport already forwards.
   */
  billing?: BillingFrame;
}

/**
 * The money frame. The unit is SCRAI: 1 SCRAI = USD 0.00001.
 *
 * Both numbers travel to the client on purpose — the UI shows priceScrai and
 * tracks costScrai invisibly. Nothing here is ever written to disk.
 */
export interface BillingFrame {
  model: string;
  /** Provider cost in SCRAI, 4 decimals kept against rounding drift. */
  costScrai: number;
  /** What the user pays: whole SCRAI, cost x margin, rounded up. */
  priceScrai: number;
  /** Table id, e.g. "2026-07-30+remote". Useful when a price looks wrong. */
  pricingVersion: string;
  /** true when token counts were estimated from characters, not reported. */
  estimated: boolean;
  /** true when the model had no price entry and the conservative default applied. */
  fallbackPrice: boolean;
}

/** Thrown by adapters for provider-side failures, mapped to a clean client error. */
export class AdapterError extends Error {
  constructor(
    public readonly provider: string,
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "AdapterError";
  }
}
