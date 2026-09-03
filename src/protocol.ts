// ---------------------------------------------------------------------------
// protocol.ts — the wire envelope that travels through the mixnet.
//
// This is the ONLY thing the client and server agree on across the mixnet. It
// is deliberately separate from types.ts: types.ts is the internal shape shared
// with the adapters, this is the external contract that two independently
// evolving programs speak. When the Rust core replaces this TS client, it
// implements THIS file and nothing else.
//
// Rules that keep it forward-compatible:
//   - every message carries `v`; a peer that sees an unknown v says so rather
//     than guessing
//   - every message carries `id`, echoed in the response, so a client can have
//     more than one request in flight over one mixnet connection
//   - `kind` is a closed set; unknown kinds are an error, never ignored
//
// A chat can be answered two ways, chosen by the client with `stream`:
//
//   stream: false   one `chat.ok` carrying the whole answer
//   stream: true    many `chat.chunk` frames, then one `chat.end`
//
// Streaming over a mixnet is not streaming over TCP. Each frame is an
// independent message that takes its own route, so THEY ARRIVE OUT OF ORDER.
// Every chunk therefore carries `seq`, and `chat.end` carries the total count
// so the receiver can tell "still arriving" from "one is never coming". See
// assembler.ts for the reordering; getting this wrong looks like a model that
// occasionally scrambles its own sentences.
// ---------------------------------------------------------------------------

import type { ChatMessage, GeneratedImage, TokenUsage } from "./types.js";
import type { ModelKind } from "./adapter.js";
import type { BlindedOutput, SignedOutput, Proof, PublicKey } from "./money/token.js";

export const PROTOCOL_VERSION = 1;

export type { BlindedOutput, SignedOutput, Proof, PublicKey };

/** What the client asks for. */
export type Request =
  | { v: number; kind: "models"; id: string }
  // The issuer's public keyset, so the client can blind against it, verify the
  // DLEQ on what it gets back, and pin it to notice if it ever changes.
  | { v: number; kind: "keys"; id: string }
  // The PUBLIC KEY travels, not the account id: the id is its hash, so the
  // server can derive one from the other but cannot verify a signature from a
  // hash alone. `nonce` is single-use — without it a captured withdrawal
  // request could be replayed to drain an entitlement.
  | { v: number; kind: "invoice.create"; id: string; publicKey: string; usd: number; method: "btc" | "nyx"; nonce: string; sig: string }
  | { v: number; kind: "invoice.status"; id: string; invoiceId: string }
  | { v: number; kind: "invoice.cancel"; id: string; invoiceId: string }
  // Read-only: how much has this account paid for but not yet withdrawn? Signed
  // so only the account holder can ask; used by `claim` to size a withdrawal.
  | { v: number; kind: "entitlement"; id: string; publicKey: string; nonce: string; sig: string }
  // `outputs` are BLINDED: the issuer signs values it cannot read. The account
  // signature covers the TOTAL of their amounts, so it authorises exactly the
  // sum being withdrawn and nothing more.
  | { v: number; kind: "withdraw"; id: string; publicKey: string; outputs: BlindedOutput[]; nonce: string; sig: string }
  // `proofs` are the unblinded tokens. No account key here — that is the whole
  // point: a session is funded by bearer ecash the issuer cannot tie to a buyer.
  | { v: number; kind: "session.open"; id: string; proofs: Proof[]; publicKey: string }
  | { v: number; kind: "session.status"; id: string; sessionId: string; sig: string }
  // Chunked image upload for vision. Large images are unreliable as one mixnet
  // message (fragment loss) and give no progress; instead the client stages the
  // image in small acked chunks, then references it from a chat by `uploadId`.
  | { v: number; kind: "upload.begin"; id: string; mimeType: string; totalBytes: number }
  | { v: number; kind: "upload.chunk"; id: string; uploadId: string; seq: number; data: string }
  | {
      v: number;
      kind: "chat";
      id: string;
      model: string;
      messages: ChatMessage[];
      maxTokens?: number;
      temperature?: number;
      /** Ask for chunk frames instead of one complete answer. */
      stream?: boolean;
      /**
       * Payment. Present once a session funds the request; absent only while
       * the server runs with enforcement off.
       *
       * `sig` covers sessionId, counter AND a hash of the request body, so a
       * captured signature authorises exactly the request it came with — not a
       * longer, costlier one.
       */
      sessionId?: string;
      counter?: number;
      sig?: string;
    };

/** What the server answers. `error` is a valid answer to anything. */
export type Response =
  | { v: number; kind: "models.ok"; id: string; models: ModelInfo[] }
  | { v: number; kind: "keys.ok"; id: string; keysetId: string; keys: PublicKey[] }
  | {
      v: number;
      kind: "invoice.ok";
      id: string;
      invoiceId: string;
      payTo: string;
      instruction: string;
      /**
       * Every way to pay, carried to the client OVER THE MIXNET and rendered
       * there. Deliberately not a checkout URL: opening one would have the
       * user's browser connect to our payment server directly, revealing their
       * IP at the exact moment they are least anonymous.
       */
      options: PaymentOption[];
      amountUsd: number;
      amountScrai: number;
      expiresAt: number;
    }
  | { v: number; kind: "invoice.state"; id: string; status: "pending" | "paid" | "expired"; entitlement: number }
  | { v: number; kind: "invoice.cancelled"; id: string; ok: boolean }
  | { v: number; kind: "entitlement.ok"; id: string; entitlement: number }
  | { v: number; kind: "withdraw.ok"; id: string; keysetId: string; signatures: SignedOutput[] }
  | { v: number; kind: "session.ok"; id: string; sessionId: string; balance: number; counter?: number }
  | { v: number; kind: "chat.ok"; id: string; text: string; images?: GeneratedImage[]; usage: TokenUsage; balance?: number }
  | { v: number; kind: "chat.chunk"; id: string; seq: number; delta: string }
  | { v: number; kind: "chat.end"; id: string; chunks: number; images?: GeneratedImage[]; usage: TokenUsage; balance?: number }
  | { v: number; kind: "upload.begin.ok"; id: string; uploadId: string }
  | { v: number; kind: "upload.chunk.ok"; id: string; uploadId: string; received: number }
  | {
      v: number;
      kind: "error";
      id: string;
      error: string;
      reason?: ErrorReason;
      /**
       * The counter the server has already seen, sent with a `replay`
       * rejection so a client whose own counter fell behind can resynchronise
       * instead of being locked out for good.
       *
       * Safe to disclose: the counter orders requests, it does not authorise
       * them. Replaying still needs a signature, which still needs the private
       * key the server has never held.
       */
      counter?: number;
    };

/**
 * Machine-readable failure cause, so the client can react rather than pattern
 * match on prose. `insufficient` in particular is the one a client must handle:
 * it means top up, not retry.
 */
export type ErrorReason =
  | "insufficient"
  | "replay"
  | "unknown-session"
  | "bad-signature"
  | "bad-token"
  | "spent-token"
  | "payment-required"
  | "bad-account"
  | "no-entitlement"
  | "invoice-unknown";

export interface PaymentOption {
  method: string;
  destination: string;
  /** BIP21 or lightning: URI — this is what goes into the QR code. */
  uri: string;
  amount: string;
  currency: string;
}

export interface ModelInfo {
  model: string;
  vendor: string;
  /** "image" tells the client to budget reply SURBs for a megabyte, not a kilobyte. */
  kind: ModelKind;
  trainsOnInput: boolean;
  /** Whether this model accepts file inputs (images/PDF/text) — gates the "+". */
  acceptsImages?: boolean;
  /**
   * Retail price in TOKU per 1M tokens, margin already applied.
   *
   * Sent so the client can estimate a price before spending a mixnet round
   * trip on it. It is advisory only: the server prices every exchange from its
   * own table and enforces the result, so a client that lies to itself about
   * the cost changes nothing except what it displays.
   */
  rate?: { in: number; out: number };
}

export function newId(): string {
  return crypto.randomUUID();
}

/**
 * Parse an inbound envelope.
 *
 * Never throws: a malformed or wrong-version message is a protocol event the
 * caller has to answer, not an exception that kills the server loop.
 */
export function parseRequest(raw: string): { ok: true; req: Request } | { ok: false; error: string; id: string } {
  let msg: unknown;
  try {
    msg = JSON.parse(raw);
  } catch {
    return { ok: false, error: "malformed json", id: "unknown" };
  }
  if (!msg || typeof msg !== "object") return { ok: false, error: "not an object", id: "unknown" };

  const m = msg as Partial<Request> & Record<string, unknown>;
  const id = typeof m.id === "string" ? m.id : "unknown";

  if (m.v !== PROTOCOL_VERSION) {
    return { ok: false, error: `unsupported protocol version ${String(m.v)}, expected ${PROTOCOL_VERSION}`, id };
  }
  if (m.kind === "models") return { ok: true, req: { v: PROTOCOL_VERSION, kind: "models", id } };
  if (m.kind === "keys") return { ok: true, req: { v: PROTOCOL_VERSION, kind: "keys", id } };

  if (m.kind === "invoice.create") {
    const mm = m as { publicKey?: unknown; usd?: unknown; method?: unknown; nonce?: unknown; sig?: unknown };
    if (typeof mm.publicKey !== "string" || typeof mm.sig !== "string" || typeof mm.nonce !== "string") {
      return { ok: false, error: "invoice.create needs publicKey, nonce and sig", id };
    }
    if (typeof mm.usd !== "number" || !Number.isFinite(mm.usd) || mm.usd <= 0) {
      return { ok: false, error: "invoice.create needs a positive usd amount", id };
    }
    // method is optional for backward compatibility: an older client that omits
    // it still gets the Bitcoin gateway it always got.
    const method = mm.method === "nyx" ? "nyx" : "btc";
    return {
      ok: true,
      req: {
        v: PROTOCOL_VERSION,
        kind: "invoice.create",
        id,
        publicKey: mm.publicKey,
        usd: mm.usd,
        method,
        nonce: mm.nonce,
        sig: mm.sig,
      },
    };
  }

  if (m.kind === "invoice.status") {
    const iv = (m as { invoiceId?: unknown }).invoiceId;
    if (typeof iv !== "string") return { ok: false, error: "invoice.status needs invoiceId", id };
    return { ok: true, req: { v: PROTOCOL_VERSION, kind: "invoice.status", id, invoiceId: iv } };
  }

  if (m.kind === "invoice.cancel") {
    // Unauthenticated like invoice.status: the id is a random uuid the client
    // holds, and cancelling only expires an UNPAID invoice — no funds, idempotent.
    const iv = (m as { invoiceId?: unknown }).invoiceId;
    if (typeof iv !== "string") return { ok: false, error: "invoice.cancel needs invoiceId", id };
    return { ok: true, req: { v: PROTOCOL_VERSION, kind: "invoice.cancel", id, invoiceId: iv } };
  }

  if (m.kind === "entitlement") {
    const mm = m as { publicKey?: unknown; nonce?: unknown; sig?: unknown };
    if (typeof mm.publicKey !== "string" || typeof mm.nonce !== "string" || typeof mm.sig !== "string") {
      return { ok: false, error: "entitlement needs publicKey, nonce and sig", id };
    }
    return { ok: true, req: { v: PROTOCOL_VERSION, kind: "entitlement", id, publicKey: mm.publicKey, nonce: mm.nonce, sig: mm.sig } };
  }

  if (m.kind === "withdraw") {
    const mm = m as { publicKey?: unknown; outputs?: unknown; nonce?: unknown; sig?: unknown };
    if (typeof mm.publicKey !== "string" || typeof mm.sig !== "string" || typeof mm.nonce !== "string") {
      return { ok: false, error: "withdraw needs publicKey, nonce and sig", id };
    }
    const outputs = parseOutputs(mm.outputs);
    if (!outputs) return { ok: false, error: "withdraw needs a non-empty outputs array of {amount, B_}", id };
    return {
      ok: true,
      req: {
        v: PROTOCOL_VERSION,
        kind: "withdraw",
        id,
        publicKey: mm.publicKey,
        outputs,
        nonce: mm.nonce,
        sig: mm.sig,
      },
    };
  }

  if (m.kind === "session.status") {
    const mm = m as { sessionId?: unknown; sig?: unknown };
    if (typeof mm.sessionId !== "string" || typeof mm.sig !== "string") {
      return { ok: false, error: "session.status needs sessionId and sig", id };
    }
    return { ok: true, req: { v: PROTOCOL_VERSION, kind: "session.status", id, sessionId: mm.sessionId, sig: mm.sig } };
  }

  if (m.kind === "session.open") {
    const mm = m as { proofs?: unknown; publicKey?: unknown };
    if (typeof mm.publicKey !== "string") {
      return { ok: false, error: "session.open needs a publicKey", id };
    }
    const proofs = parseProofs(mm.proofs);
    if (!proofs) return { ok: false, error: "session.open needs a non-empty proofs array of {amount, secret, C}", id };
    return {
      ok: true,
      req: { v: PROTOCOL_VERSION, kind: "session.open", id, proofs, publicKey: mm.publicKey },
    };
  }
  if (m.kind === "upload.begin") {
    const mm = m as { mimeType?: unknown; totalBytes?: unknown };
    if (typeof mm.mimeType !== "string" || !ACCEPTED_UPLOAD_MIME.test(mm.mimeType)) {
      return { ok: false, error: "upload.begin: unsupported file type", id };
    }
    if (typeof mm.totalBytes !== "number" || !Number.isInteger(mm.totalBytes) || mm.totalBytes <= 0 || mm.totalBytes > MAX_IMAGE_BYTES) {
      return { ok: false, error: `upload.begin totalBytes must be 1..${MAX_IMAGE_BYTES}`, id };
    }
    return { ok: true, req: { v: PROTOCOL_VERSION, kind: "upload.begin", id, mimeType: mm.mimeType, totalBytes: mm.totalBytes } };
  }
  if (m.kind === "upload.chunk") {
    const mm = m as { uploadId?: unknown; seq?: unknown; data?: unknown };
    if (typeof mm.uploadId !== "string" || mm.uploadId.length === 0) {
      return { ok: false, error: "upload.chunk needs an uploadId", id };
    }
    if (typeof mm.seq !== "number" || !Number.isInteger(mm.seq) || mm.seq < 0) {
      return { ok: false, error: "upload.chunk needs a seq >= 0", id };
    }
    if (typeof mm.data !== "string" || mm.data.length === 0 || mm.data.length > MAX_CHUNK_CHARS) {
      return { ok: false, error: `upload.chunk data must be 1..${MAX_CHUNK_CHARS} base64 chars`, id };
    }
    return { ok: true, req: { v: PROTOCOL_VERSION, kind: "upload.chunk", id, uploadId: mm.uploadId, seq: mm.seq, data: mm.data } };
  }
  if (m.kind === "chat") {
    if (typeof m.model !== "string" || !Array.isArray(m.messages)) {
      return { ok: false, error: "chat needs model and messages", id };
    }
    return {
      ok: true,
      req: {
        v: PROTOCOL_VERSION,
        kind: "chat",
        id,
        model: m.model,
        messages: m.messages as ChatMessage[],
        maxTokens: typeof m.maxTokens === "number" ? m.maxTokens : undefined,
        temperature: typeof m.temperature === "number" ? m.temperature : undefined,
        stream: m.stream === true,
        ...(typeof m.sessionId === "string" ? { sessionId: m.sessionId } : {}),
        ...(typeof m.counter === "number" ? { counter: m.counter } : {}),
        ...(typeof m.sig === "string" ? { sig: m.sig } : {}),
      },
    };
  }
  return { ok: false, error: `unknown kind "${String(m.kind)}"`, id };
}

/**
 * Upper bound on ecash items per message. An honest withdrawal is the binary
 * decomposition of the amount (~a few dozen tokens at most); this caps a client
 * that pads an array to make the server sign or verify thousands of items.
 */
const MAX_ECASH_ITEMS = 64;

/** Hard cap on a single uploaded file: 10 MB of raw bytes. */
export const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
/** Per-chunk base64 length cap (~768 KB raw → ~1 MB base64). */
export const MAX_CHUNK_CHARS = 1_100_000;
/**
 * File types a model may accept as input (Gemini): images, PDF, and text/code.
 * The provider is the final authority — this is a coarse gate to reject obvious
 * junk (executables, archives) before a byte crosses the mixnet.
 */
export const ACCEPTED_UPLOAD_MIME =
  /^(image\/(png|jpe?g|webp|heic|heif|gif)|application\/(pdf|json)|text\/)/i;

function parseOutputs(v: unknown): BlindedOutput[] | null {
  if (!Array.isArray(v) || v.length === 0 || v.length > MAX_ECASH_ITEMS) return null;
  const out: BlindedOutput[] = [];
  for (const o of v) {
    const oo = o as { amount?: unknown; B_?: unknown };
    if (typeof oo?.amount !== "number" || !Number.isInteger(oo.amount) || oo.amount <= 0) return null;
    if (typeof oo?.B_ !== "string") return null;
    out.push({ amount: oo.amount, B_: oo.B_ });
  }
  return out;
}

function parseProofs(v: unknown): Proof[] | null {
  if (!Array.isArray(v) || v.length === 0 || v.length > MAX_ECASH_ITEMS) return null;
  const out: Proof[] = [];
  for (const p of v) {
    const pp = p as { amount?: unknown; secret?: unknown; C?: unknown };
    if (typeof pp?.amount !== "number" || !Number.isInteger(pp.amount) || pp.amount <= 0) return null;
    if (typeof pp?.secret !== "string" || typeof pp?.C !== "string") return null;
    out.push({ amount: pp.amount, secret: pp.secret, C: pp.C });
  }
  return out;
}

export function parseResponse(raw: string): Response | null {
  try {
    const r = JSON.parse(raw) as Response;
    return r && typeof r === "object" && typeof r.kind === "string" ? r : null;
  } catch {
    return null;
  }
}

export const errorResponse = (
  id: string,
  error: string,
  reason?: ErrorReason,
  counter?: number,
): Response => ({
  v: PROTOCOL_VERSION,
  kind: "error",
  id,
  error,
  ...(reason ? { reason } : {}),
  ...(counter !== undefined ? { counter } : {}),
});
