#!/usr/bin/env node
// ---------------------------------------------------------------------------
// scrai-server — the mixnet-side AI service.
//
// It runs its own nym-client, so it has a Nym address and NO public IP, no TLS
// certificate, and no DNS record. Clients reach it by knowing that address and
// nothing else. This is the service-provider model: the mixnet route terminates
// here rather than at an exit gateway.
//
// The pipeline below is the same one server.ts already had over HTTP — resolve
// the model to an adapter, meter the stream, price the exchange. Only the
// transport changed, which is exactly what the adapter/billing split was for.
//
// Statelessness is unchanged and still literal: no prompt, response, identity,
// or billing record is written anywhere. We answer via the sender's reply SURBs
// and never learn who they are.
// ---------------------------------------------------------------------------

import { registerIfAvailable, keyFor, resolve, catalog, refreshCatalog, pruneUnpriced, deregister } from "../adapter.js";
import { geminiAdapter } from "../adapters/gemini.js";
import { geminiImageAdapter } from "../adapters/gemini-image.js";
import { createMeter, retailRate } from "../billing.js";
import { warmPricing, pricingVersion, pricingAgeDays, hasPrice } from "../pricing.js";
import { PROTOCOL_VERSION, parseRequest, errorResponse, type Response } from "../protocol.js";
import { MoneyStore } from "../money/store.js";
import { Mint } from "../money/token.js";
import { Issuer } from "../money/issuer.js";
import { selectGateway, selectNyxGateway, type PaymentGateway } from "../money/gateway.js";
import { accountIdFor } from "../money/account.js";
import { createPublicKey, verify as verifySignature, randomBytes } from "node:crypto";
import type { ChatMessage } from "../types.js";
import { verifyRequest, keyMatchesSession, sessionIdFor as sessionIdOf } from "../money/session.js";
import { SCRAI_PER_USD } from "../billing.js";
import { NymSocket, DEFAULT_WS_PORT } from "../nym/socket.js";
import * as nym from "../nym/process.js";

// Each provider is offered only if its credential is actually present, so the
// catalog never advertises something this box cannot serve.
const PROVIDERS = [geminiAdapter, geminiImageAdapter];
const dormant: string[] = [];
for (const a of PROVIDERS) {
  if (!registerIfAvailable(a)) dormant.push(`${a.vendor} (set ${a.apiKeyEnv})`);
}

const CLIENT_ID = (process.env.SERVER_NYM_ID ?? process.env.SCRAI_SERVER_NYM_ID) ?? "scrai-server";
const WS_PORT = Number((process.env.NYM_WS_PORT ?? process.env.SCRAI_NYM_WS_PORT) ?? DEFAULT_WS_PORT);

// Chunk batching. Every frame is a separate mixnet message that costs the
// client a reply SURB, so per-token frames would be wasteful and slow. Batching
// to ~a short phrase keeps it feeling live at a small fraction of the frames.
const FLUSH_CHARS = Number((process.env.FLUSH_CHARS ?? process.env.SCRAI_FLUSH_CHARS) ?? 120);
const FLUSH_MS = Number((process.env.FLUSH_MS ?? process.env.SCRAI_FLUSH_MS) ?? 500);

/** Warn once at startup when the hand-maintained price table is older than this. */
const STALE_PRICING_DAYS = Number((process.env.STALE_PRICING_DAYS ?? process.env.SCRAI_STALE_PRICING_DAYS) ?? 30);

// Payment enforcement. On by default — an unenforced server is one where every
// client rides free, which is the state this whole layer exists to end.
const REQUIRE_PAYMENT = (process.env.REQUIRE_PAYMENT ?? process.env.SCRAI_REQUIRE_PAYMENT) !== "0";
// Dev minting hands out funding tokens for nothing. It is what stands in for a
// payment provider until one exists, and it must never be on in public.
const ALLOW_DEV_MINT = (process.env.DEV_MINT ?? process.env.SCRAI_DEV_MINT) === "1";
const SESSION_TTL_MS = Number((process.env.SESSION_TTL_SEC ?? process.env.SCRAI_SESSION_TTL_SEC) ?? 30 * 86_400) * 1000;

const money = new MoneyStore((process.env.MONEY_DB ?? process.env.SCRAI_MONEY_DB) ?? "./data/money.db");

// The mint's whole keyset is derived from one seed. SCRAI_ISSUER_SECRET when
// set (keeps the seed out of the DB); otherwise a persisted random seed, so
// tokens survive a restart. The mint exists independently of the gateway so
// that redeeming already-bought tokens and serving the keyset keep working on a
// server that cannot sell new TOKU.
const issuerSecret = process.env.ISSUER_SECRET ?? process.env.SCRAI_ISSUER_SECRET;
const mintSeed = issuerSecret ? Buffer.from(issuerSecret, "utf8") : money.getOrCreateMintSeed();
const mint = new Mint(mintSeed);

// The issuer is only wired up when a gateway is configured. Without one the
// server still serves paid sessions — it just cannot sell new TOKU.
let issuer: Issuer | null = null;
try {
  const gateways: Record<string, PaymentGateway> = { btc: selectGateway() };
  // NYM is additive: only wired up when NYX_RECEIVE_ADDRESS + NYX_RPC_WS are set.
  const nyx = await selectNyxGateway();
  if (nyx) {
    gateways.nyx = nyx;
    console.log("[issuer] NYM payments enabled (Nyx chain)");
    nyx.warmup?.(); // connect + prefetch now, so the first invoice is snappy
  }
  issuer = new Issuer(money, gateways, mint);
} catch (err) {
  console.warn(`[issuer] disabled: ${(err as Error).message}`);
}

/**
 * The bytes a chat signature covers.
 *
 * Must be derived from the SAME fields on both sides and in the same order, so
 * it is built here from the parsed request rather than from the raw message —
 * a client that reorders its JSON keys would otherwise fail verification for no
 * reason.
 */
function canonicalBody(req: { model: string; messages: unknown; maxTokens?: number }): string {
  return JSON.stringify({ model: req.model, messages: req.messages, maxTokens: req.maxTokens ?? null });
}

/**
 * Worst-case price of a request, in TOKU — the amount to reserve.
 *
 * Uses the same retail rates the client was quoted, so both sides arrive at the
 * same number and a client is never refused for a ceiling it could not predict.
 */
/** Safe upper bound on the input tokens one attachment bills as. Gemini tiles a
 *  large image into ~hundreds of tokens and a PDF page costs ~258+; this
 *  over-reserves rather than risk billing above the ceiling. */
const ATTACHMENT_INPUT_TOKENS = 4096;

function ceilingFor(req: { model: string; messages: Array<{ content: string; attachments?: unknown[] }>; maxTokens?: number }): number {
  const rate = retailRate(req.model);
  // Byte length, not chars/4: a byte-level BPE token decodes to at least one
  // byte, so the byte count is a GUARANTEED upper bound on the real input token
  // count. chars/4 is only an average, and an adversarial multibyte prompt (CJK,
  // emoji) tokenises to several times that — while the user is never charged
  // above this ceiling, so it must not undercount. Output dominates the ceiling
  // anyway, so over-reserving input by the ASCII bytes-per-token factor barely
  // moves the total.
  const inTokens = req.messages.reduce(
    (n, m) => n + Buffer.byteLength(m.content ?? "", "utf8") + (m.attachments?.length ?? 0) * ATTACHMENT_INPUT_TOKENS,
    0,
  );
  const { answer, thinking } = outputBudget(req);
  return Math.ceil((inTokens * rate.in + (answer + thinking) * rate.out) / 1_000_000);
}

/** Assumed output ceiling when a client sends no maxTokens of its own. */
const DEFAULT_MAX_TOKENS = Number((process.env.DEFAULT_MAX_TOKENS ?? process.env.SCRAI_DEFAULT_MAX_TOKENS) ?? 4096);

/**
 * Cap on thinking tokens. On Gemini these bill AS OUTPUT but are NOT bounded by
 * maxOutputTokens, so without a cap a thinking model can generate far more billed
 * output than the answer limit — blowing past the reservation and handing the
 * operator the difference, since the user is never charged above the ceiling. The
 * same number is reserved here and enforced at the provider (thinkingConfig), so
 * the two cannot drift. 0 disables thinking; raise it to trade a larger
 * reservation for more reasoning depth.
 */
const THINKING_BUDGET = Number((process.env.THINKING_BUDGET ?? process.env.SCRAI_THINKING_BUDGET) ?? 2048);

/**
 * The output the provider is ALLOWED to produce, split into the visible answer
 * and the hidden thinking. Both halves are reserved by the ceiling and enforced
 * at the provider, so reserved output == enforced output and the ceiling stays a
 * true upper bound. Deriving both from one place is what keeps them in step.
 */
function outputBudget(req: { maxTokens?: number }): { answer: number; thinking: number } {
  return { answer: req.maxTokens ?? DEFAULT_MAX_TOKENS, thinking: THINKING_BUDGET };
}

/**
 * Does this request really come from the holder of that account key?
 *
 * Three things must hold, and each closes a different door:
 *   - the signature verifies against the supplied key      (it is that account)
 *   - the signed text names the purpose and amount         (not some other action)
 *   - the nonce has never been used                        (not a replay)
 *
 * The nonce goes through the same atomic burn as a spent token serial, so two
 * concurrent replays cannot both slip past.
 */
function accountOwns(publicKey: string, nonce: string, purpose: string, sig: string): string | null {
  let accountId: string;
  try {
    accountId = accountIdFor(publicKey);
    const bytes = Buffer.from(`${accountId}:${purpose}:${nonce}`, "utf8");
    if (!verifySignature(null, bytes, createPublicKey(publicKey), Buffer.from(sig, "base64"))) return null;
  } catch {
    return null; // malformed key or signature
  }
  // Burn last: a failed signature must not consume a nonce, or an attacker
  // could invalidate someone else's pending request by guessing at it.
  if (!money.spendSerial(`acct:${accountId}:${nonce}`)) return null;
  return accountId;
}

function fail(msg: string): never {
  console.error(msg);
  process.exit(1);
}

// ---- chunked image upload staging -----------------------------------------
// Vision images arrive as small acked chunks (reliable over a lossy mixnet, and
// progress-reportable) and are reassembled here, then consumed by the chat that
// references them by uploadId. Ephemeral, capped and swept so an anonymous
// caller cannot exhaust memory. Uploads are unauthenticated by design (the chat
// that consumes them is signed + billed); the caps are the DoS bound.
interface Upload {
  mimeType: string;
  total: number;
  received: number;
  chunks: Map<number, Buffer>;
  at: number;
}
const uploads = new Map<string, Upload>();
const UPLOAD_TTL_MS = 3 * 60_000;
const MAX_CONCURRENT_UPLOADS = 12;
const MAX_STAGED_BYTES = 96 * 1024 * 1024;
let stagedBytes = 0;

function sweepUploads(): void {
  const now = Date.now();
  for (const [id, u] of uploads) {
    if (now - u.at > UPLOAD_TTL_MS) {
      stagedBytes -= u.received;
      uploads.delete(id);
    }
  }
}

function reassemble(u: Upload): Buffer {
  const parts: Buffer[] = [];
  for (let i = 0; i < u.chunks.size; i++) {
    const c = u.chunks.get(i);
    if (c) parts.push(c);
  }
  return Buffer.concat(parts);
}

/**
 * Replace `uploadId` image references with the reassembled base64 bytes, and
 * consume (free) each upload. Throws if a reference is unknown or incomplete —
 * the chat's try/catch turns that into an error and refunds the reservation.
 */
function resolveUploads(messages: ChatMessage[]): ChatMessage[] {
  return messages.map((m) => {
    if (!m.attachments?.length) return m;
    const attachments = m.attachments.map((att) => {
      if (att.data) return att;
      if (!att.uploadId) throw new Error("attachment is missing both data and uploadId");
      const u = uploads.get(att.uploadId);
      if (!u) throw new Error("referenced upload is unknown or expired");
      if (u.received < u.total) throw new Error("referenced upload is incomplete");
      const data = reassemble(u).toString("base64");
      stagedBytes -= u.received;
      uploads.delete(att.uploadId);
      return { mimeType: u.mimeType, data };
    });
    return { ...m, attachments };
  });
}

// ---- invoice.create rate limit -------------------------------------------
// A free BIP39 account can trigger a BTCPay call (an external, costly action),
// so this is the one anonymity-exposed endpoint worth throttling. Per-account
// caps stop one account spamming; the global cap protects BTCPay from a swarm of
// throwaway accounts. Sliding windows; opportunistically pruned so a throwaway
// swarm cannot grow the map without bound.
const INVOICE_PER_ACCT = Number((process.env.INVOICE_PER_ACCT ?? process.env.SCRAI_INVOICE_PER_ACCT) ?? 5);
const INVOICE_ACCT_WINDOW_MS = Number((process.env.INVOICE_ACCT_WINDOW_SEC ?? process.env.SCRAI_INVOICE_ACCT_WINDOW_SEC) ?? 600) * 1000;
const INVOICE_GLOBAL_PER_MIN = Number((process.env.INVOICE_GLOBAL_PER_MIN ?? process.env.SCRAI_INVOICE_GLOBAL_PER_MIN) ?? 30);
const invoiceHits = new Map<string, number[]>();
let invoiceGlobal: number[] = [];

type Admit = { ok: true } | { ok: false; retrySec: number; scope: "server" | "account" };
function admitInvoice(accountId: string): Admit {
  const now = Date.now();
  invoiceGlobal = invoiceGlobal.filter((t) => now - t < 60_000);
  if (invoiceGlobal.length >= INVOICE_GLOBAL_PER_MIN) {
    return { ok: false, retrySec: Math.max(1, Math.ceil((60_000 - (now - invoiceGlobal[0])) / 1000)), scope: "server" };
  }
  const arr = (invoiceHits.get(accountId) ?? []).filter((t) => now - t < INVOICE_ACCT_WINDOW_MS);
  if (arr.length >= INVOICE_PER_ACCT) {
    return { ok: false, retrySec: Math.max(1, Math.ceil((INVOICE_ACCT_WINDOW_MS - (now - arr[0])) / 1000)), scope: "account" };
  }
  arr.push(now);
  invoiceHits.set(accountId, arr);
  invoiceGlobal.push(now);
  if (invoiceHits.size > 2000) {
    for (const [k, v] of invoiceHits) if (v.every((t) => now - t >= INVOICE_ACCT_WINDOW_MS)) invoiceHits.delete(k);
  }
  return { ok: true };
}

/**
 * Serve one request, emitting one or more responses through `send`.
 *
 * Non-streaming answers call send() exactly once. Streaming answers call it
 * many times and finish with chat.end. Either way the meter sees every chunk,
 * so billing is identical — only the framing differs.
 */
async function handle(raw: string, send: (r: Response) => void): Promise<string> {
  const parsed = parseRequest(raw);
  if (!parsed.ok) {
    send(errorResponse(parsed.id, parsed.error));
    return "error";
  }
  const req = parsed.req;

  if (req.kind === "models") {
    // Rates ride along so the client can quote a price without a second trip.
    const models = catalog().map((m) => ({ ...m, rate: retailRate(m.model) }));
    send({ v: PROTOCOL_VERSION, kind: "models.ok", id: req.id, models });
    return "models.ok";
  }

  if (req.kind === "keys") {
    // Unauthenticated on purpose: the keyset is public by design — the client
    // needs it to blind against, to check the DLEQ, and to pin so it can notice
    // if it ever changes. Works with or without a gateway.
    send({ v: PROTOCOL_VERSION, kind: "keys.ok", id: req.id, keysetId: mint.keysetId(), keys: mint.publicKeys() });
    return "keys.ok";
  }

  if (req.kind === "invoice.create") {
    if (!issuer) {
      send(errorResponse(req.id, "this server cannot sell TOKU — no payment gateway configured", "payment-required"));
      return "invoice.denied";
    }
    const invoiceAccount = accountOwns(req.publicKey, req.nonce, `invoice:${req.usd}`, req.sig);
    if (!invoiceAccount) {
      send(errorResponse(req.id, "account signature does not check out, or the nonce was reused", "bad-account"));
      return "invoice.denied";
    }
    // Throttle BEFORE the external BTCPay call: an account or a swarm cannot make
    // us hammer the payment gateway.
    const gate = admitInvoice(invoiceAccount);
    if (!gate.ok) {
      const who = gate.scope === "server" ? "the server is issuing too many invoices right now" : "too many invoices from this account";
      send(errorResponse(req.id, `${who} — retry in ~${gate.retrySec}s`, "payment-required"));
      return `invoice.throttled/${gate.scope}`;
    }
    try {
      const inv = await issuer.createInvoice(invoiceAccount, req.usd, req.method);
      send({
        v: PROTOCOL_VERSION,
        kind: "invoice.ok",
        id: req.id,
        invoiceId: inv.id,
        payTo: inv.payTo,
        instruction: inv.instruction,
        options: inv.options,
        amountUsd: inv.amountUsd,
        amountScrai: inv.amountScrai,
        expiresAt: inv.expiresAt,
      });
      return `invoice.ok/${inv.amountUsd}USD`;
    } catch (err) {
      send(errorResponse(req.id, err instanceof Error ? err.message : "could not raise an invoice"));
      return "invoice.error";
    }
  }

  if (req.kind === "invoice.status") {
    if (!issuer) {
      send(errorResponse(req.id, "no payment gateway configured", "payment-required"));
      return "status.denied";
    }
    // Deliberately unauthenticated: an invoice id is a random uuid the client
    // just received, and the reply reveals nothing an eavesdropper could use.
    const st = await issuer.status(req.invoiceId);
    if (!st) {
      send(errorResponse(req.id, "unknown invoice", "invoice-unknown"));
      return "status.unknown";
    }
    send({ v: PROTOCOL_VERSION, kind: "invoice.state", id: req.id, status: st.status, entitlement: st.entitlement, ...(st.watch ? { watch: st.watch } : {}) });
    return `invoice.${st.status}`;
  }

  if (req.kind === "invoice.cancel") {
    if (!issuer) {
      send(errorResponse(req.id, "no payment gateway configured", "payment-required"));
      return "cancel.denied";
    }
    // Unauthenticated (see protocol.ts): only expires an unpaid invoice, no funds.
    const ok = issuer.cancel(req.invoiceId);
    send({ v: PROTOCOL_VERSION, kind: "invoice.cancelled", id: req.id, ok });
    return ok ? "invoice.cancelled" : "invoice.cancel/noop";
  }

  if (req.kind === "entitlement") {
    // Signed, read-only: only the account holder can read what it is owed. Uses
    // the same nonce burn as any account action, so a captured query cannot be
    // replayed. Works without a gateway — entitlement lives in the store.
    const acct = accountOwns(req.publicKey, req.nonce, "entitlement", req.sig);
    if (!acct) {
      send(errorResponse(req.id, "account signature does not check out, or the nonce was reused", "bad-account"));
      return "entitlement.denied";
    }
    send({ v: PROTOCOL_VERSION, kind: "entitlement.ok", id: req.id, entitlement: money.entitlement(acct) });
    return "entitlement.ok";
  }

  if (req.kind === "withdraw") {
    if (!issuer) {
      send(errorResponse(req.id, "no payment gateway configured", "payment-required"));
      return "withdraw.denied";
    }
    // The account signature authorises the TOTAL of the blinded outputs, so it
    // covers exactly the sum being withdrawn and nothing more.
    const total = req.outputs.reduce((n, o) => n + o.amount, 0);
    const wAccount = accountOwns(req.publicKey, req.nonce, `withdraw:${total}`, req.sig);
    if (!wAccount) {
      send(errorResponse(req.id, "account signature does not check out, or the nonce was reused", "bad-account"));
      return "withdraw.denied";
    }
    try {
      // Catch up on any payment that confirmed since the client stopped polling,
      // so a withdrawal never reports an empty account that is actually funded.
      await issuer.sweep();
      const { keysetId, signatures } = issuer.withdraw(wAccount, req.outputs);
      send({ v: PROTOCOL_VERSION, kind: "withdraw.ok", id: req.id, keysetId, signatures });
      return `withdraw.ok/${total}`;
    } catch (err) {
      send(errorResponse(req.id, err instanceof Error ? err.message : "withdrawal failed", "no-entitlement"));
      return "withdraw.refused";
    }
  }

  if (req.kind === "session.status") {
    const session = money.getSession(req.sessionId);
    if (!session) {
      send(errorResponse(req.id, "unknown session", "unknown-session"));
      return "status.unknown";
    }
    // Signed so only the key holder can read a balance, but deliberately NOT
    // counter-checked: this is read-only, and requiring a valid counter to
    // recover a broken counter would be circular.
    if (!verifyRequest(session.pubkey, req.sessionId, 0, "status", req.sig)) {
      send(errorResponse(req.id, "signature does not match this session", "bad-signature"));
      return "status.denied";
    }
    send({
      v: PROTOCOL_VERSION,
      kind: "session.ok",
      id: req.id,
      sessionId: req.sessionId,
      balance: session.balance,
      counter: session.counter,
    });
    return `status.ok/${session.balance}`;
  }

  if (req.kind === "session.open") {
    // The id is the hash of the key, so this stops a client claiming a session
    // that belongs to a key it does not hold.
    if (!keyMatchesSession(req.publicKey, sessionIdOf(req.publicKey))) {
      send(errorResponse(req.id, "public key does not match its session id", "bad-signature"));
      return "session.denied";
    }
    // Verify every proof cryptographically FIRST — no state changes until all of
    // them are known genuine, so a bad token in the set cannot burn the others.
    let amount = 0;
    for (const p of req.proofs) {
      if (!mint.verify(p)) {
        send(errorResponse(req.id, "a funding token is not valid", "bad-token"));
        return "session.denied";
      }
      amount += p.amount;
    }
    const sid = sessionIdOf(req.publicKey);
    // Burn all secrets and credit the balance in ONE transaction: any reused
    // secret (double-spend) rolls the whole redemption back — nothing burned,
    // nothing credited.
    const res = money.redeemProofs(sid, req.publicKey, req.proofs.map((p) => p.secret), amount);
    if (!res.ok) {
      send(errorResponse(req.id, "a funding token has already been spent", "spent-token"));
      return "session.denied";
    }
    send({ v: PROTOCOL_VERSION, kind: "session.ok", id: req.id, sessionId: sid, balance: res.session.balance, counter: res.session.counter });
    return `session.ok/${res.session.balance}`;
  }

  if (req.kind === "upload.begin") {
    sweepUploads();
    if (uploads.size >= MAX_CONCURRENT_UPLOADS || stagedBytes + req.totalBytes > MAX_STAGED_BYTES) {
      send(errorResponse(req.id, "upload capacity is exhausted — try again shortly"));
      return "upload.rejected";
    }
    const uploadId = randomBytes(16).toString("hex");
    uploads.set(uploadId, { mimeType: req.mimeType, total: req.totalBytes, received: 0, chunks: new Map(), at: Date.now() });
    send({ v: PROTOCOL_VERSION, kind: "upload.begin.ok", id: req.id, uploadId });
    return `upload.begin/${uploadId.slice(0, 8)}`;
  }

  if (req.kind === "upload.chunk") {
    const u = uploads.get(req.uploadId);
    if (!u) {
      send(errorResponse(req.id, "unknown or expired uploadId"));
      return "upload.unknown";
    }
    const bytes = Buffer.from(req.data, "base64");
    // Idempotent: a retried chunk (mixnet loss) replaces rather than double-counts.
    const prev = u.chunks.get(req.seq);
    const delta = bytes.length - (prev?.length ?? 0);
    if (u.received + delta > u.total || stagedBytes + delta > MAX_STAGED_BYTES) {
      send(errorResponse(req.id, "upload exceeds its declared size"));
      return "upload.overflow";
    }
    u.chunks.set(req.seq, bytes);
    u.received += delta;
    stagedBytes += delta;
    u.at = Date.now();
    send({ v: PROTOCOL_VERSION, kind: "upload.chunk.ok", id: req.id, uploadId: req.uploadId, received: u.received });
    return `upload.chunk/${req.seq}(${u.received}/${u.total})`;
  }

  // Refuse any model without an explicit price. Such models are already pruned
  // from the registry, but a hand-crafted request must not slip a free ride
  // through the provider on our tab either.
  if (!hasPrice(req.model)) {
    send(errorResponse(req.id, `model "${req.model}" is not available`));
    return "unpriced-model";
  }

  // ---- payment ----------------------------------------------------------
  // Everything here happens BEFORE the provider is called, so a request that
  // cannot pay costs the operator nothing.
  let charged: { sessionId: string; reserved: number } | null = null;

  if (REQUIRE_PAYMENT) {
    const { sessionId, counter, sig } = req;
    if (!sessionId || typeof counter !== "number" || !sig) {
      send(errorResponse(req.id, "this server requires a funded session", "payment-required"));
      return "unpaid";
    }
    const session = money.getSession(sessionId);
    if (!session) {
      send(errorResponse(req.id, "unknown session — open one first", "unknown-session"));
      return "unpaid";
    }
    // The signature covers the body, so it authorises this request and no other.
    const body = canonicalBody(req);
    if (!verifyRequest(session.pubkey, sessionId, counter, body, sig)) {
      send(errorResponse(req.id, "signature does not match this request", "bad-signature"));
      return "unpaid";
    }

    // Reserve the worst case. Reserving the ceiling rather than the eventual
    // price is what keeps two in-flight requests from jointly overspending.
    const ceiling = ceilingFor(req);
    const outcome = money.reserve(sessionId, counter, ceiling);
    if (outcome !== "ok") {
      const current = money.getSession(sessionId)?.counter;
      const msg =
        outcome === "insufficient"
          ? `not enough TOKU: this request reserves up to ${ceiling}, balance is ${session.balance}`
          : outcome === "replay"
            ? `counter ${counter} was already used (server is at ${current}) — resync and retry`
            : "unknown session";
      // Hand back the server's counter on a replay so the client can recover
      // rather than being stuck one number behind forever.
      send(
        errorResponse(
          req.id,
          msg,
          outcome === "unknown" ? "unknown-session" : outcome,
          outcome === "replay" ? current : undefined,
        ),
      );
      return `refused/${outcome}`;
    }
    charged = { sessionId, reserved: ceiling };
  }

  const meter = createMeter({
    model: req.model,
    promptChars: JSON.stringify(req.messages).length,
  });

  let text = "";
  let images: import("../types.js").GeneratedImage[] | undefined;
  let seq = 0;
  let buf = "";
  let lastFlush = Date.now();

  const flush = () => {
    if (!buf) return;
    send({ v: PROTOCOL_VERSION, kind: "chat.chunk", id: req.id, seq: seq++, delta: buf });
    buf = "";
    lastFlush = Date.now();
  };

  const finish = (usage: import("../types.js").TokenUsage): string => {
    const withImages = images?.length ? { images } : {};
    // Give back what the reservation did not use. The client learns its real
    // balance from the same frame that carries the price.
    let balance: number | undefined;
    if (charged) {
      balance = money.settle(charged.sessionId, charged.reserved, usage.billing?.priceScrai ?? 0);
      charged = null; // settled; the catch block must not refund again
    }
    const withBalance = balance !== undefined ? { balance } : {};

    if (req.stream) {
      flush();
      send({ v: PROTOCOL_VERSION, kind: "chat.end", id: req.id, chunks: seq, ...withImages, ...withBalance, usage });
      return `chat.end/${seq}${images?.length ? ` +${images.length}img` : ""}`;
    }
    send({ v: PROTOCOL_VERSION, kind: "chat.ok", id: req.id, text, ...withImages, ...withBalance, usage });
    return `chat.ok${images?.length ? ` +${images.length}img` : ""}`;
  };

  try {
    const adapter = resolve(req.model);
    const { answer, thinking } = outputBudget(req);
    // Swap uploadId references for the reassembled image bytes (and free them).
    // Done after the signature check + reservation, so the signed body carried
    // the small references, not the megabytes.
    const messages = resolveUploads(req.messages);
    const stream = adapter.stream(
      {
        model: req.model,
        messages,
        // Always cap output at the provider, with the SAME budget the ceiling
        // reserved. Without this a client can omit maxTokens and let the model
        // run to its own, higher default — billing more output than was reserved.
        maxTokens: answer,
        thinkingBudget: thinking,
        ...(req.temperature != null ? { temperature: req.temperature } : {}),
      },
      keyFor(adapter),
    );

    for await (const chunk of meter.wrap(stream)) {
      if (chunk.images?.length) images = [...(images ?? []), ...chunk.images];
      if (chunk.delta) {
        text += chunk.delta;
        if (req.stream) {
          buf += chunk.delta;
          if (buf.length >= FLUSH_CHARS || Date.now() - lastFlush >= FLUSH_MS) flush();
        }
      }
      if (chunk.done && chunk.usage) return finish(chunk.usage);
    }
    // Adapter ended without a terminating chunk — bill what we saw anyway.
    return finish({ ...meter.usage(), billing: meter.frame() });
  } catch (err) {
    // A provider that hung up mid-answer still charged us for what it produced.
    // Anything already streamed stays valid; the error is the terminator.
    const message = err instanceof Error ? err.message : "upstream error";
    console.error(`[chat] ${req.model}: ${message}`);
    // Self-heal: a provider that reports the model as gone (deprecated / not
    // available to this key) should stop being offered. Drop it from the catalog
    // so no client picks it again this run. It reappears on restart only if it is
    // still in /models AND priced — otherwise the operator prices it out.
    if (/\b404\b|NOT_FOUND|no longer available|is not found|not supported/i.test(message)) {
      if (deregister(req.model)) console.warn(`[models] dropped "${req.model}" — provider reports it unavailable`);
    }
    // The provider failed, so the user pays nothing — return the whole
    // reservation rather than keeping money for an answer that never came.
    if (charged) money.refund(charged.sessionId, charged.reserved);
    send(errorResponse(req.id, message));
    return "error";
  }
}

async function main(): Promise<void> {
  const cmd = process.argv[2] ?? "run";

  if (cmd === "setup") {
    const gwIdx = process.argv.indexOf("--gateway");
    const gateway = gwIdx > -1 ? process.argv[gwIdx + 1] : undefined;
    // Uniform random by default — see the note in cli/client.ts on --latency.
    const latencyBased = !gateway && process.argv.includes("--latency");
    console.log(nym.init(CLIENT_ID, { gateway, latencyBased, port: WS_PORT }));
    console.log(`\nnow start it with:  npm run server`);
    return;
  }

  if (cmd === "gateways") {
    console.log(nym.listGateways(CLIENT_ID));
    return;
  }

  if (cmd === "gateway") {
    const gw = process.argv[3];
    if (!gw) fail("usage: scrai-server gateway <ed25519-identity>");
    console.log(nym.useGateway(CLIENT_ID, gw));
    // The gateway is part of this server's Nym address (id.enc@GATEWAY), so
    // changing it changes the address every client was given. Say so loudly —
    // silently invalidating every client's config would be a nasty surprise.
    console.log(
      "\n⚠ this server's Nym address has CHANGED — the @gateway part is new.\n" +
        "  Start it (`npm run server`), copy the printed address, and re-point every client:\n" +
        "    npm run client -- server <new-address>",
    );
    return;
  }

  if (cmd !== "run") {
    fail(`usage: scrai-server [setup [--gateway <id>] | gateways | gateway <id> | run]`);
  }

  if (catalog().length === 0) {
    fail(
      "no provider is configured — set at least one credential in .env:\n" +
        "  GEMINI_API_KEY        aistudio.google.com/apikey      (text; images need billing)\n" +
        "  OPENAI_API_KEY        platform.openai.com/api-keys    (text, prepaid)",
    );
  }

  if (!nym.isInitialised(CLIENT_ID)) {
    fail(`nym client "${CLIENT_ID}" is not initialised.\n  run:  npm run server:setup`);
  }

  warmPricing();

  // Refresh the catalog from each provider's live /models — but keep ONLY models
  // that carry an explicit price. An unpriced PAID model is a money-losing hole
  // (we might charge the user the default while the provider bills us more), so
  // it is skipped and NAMED for the operator to price. Free models (explicit
  // in:0/out:0) are priced and appear normally.
  const discovery = await refreshCatalog(PROVIDERS, hasPrice);
  const prunedStatic = pruneUnpriced(hasPrice);
  for (const d of discovery) {
    if (d.error) console.warn(`[models] ${d.provider}: live discovery failed (${d.error}) — kept static list`);
    else console.log(`[models] ${d.provider}: ${d.offered.length} priced model(s) live`);
    if (d.skippedNoPrice.length)
      console.warn(`[models] ${d.provider}: SKIPPED — price unclear, add to pricing.json to offer: ${d.skippedNoPrice.join(", ")}`);
  }
  if (prunedStatic.length) console.warn(`[models] pruned unpriced static model(s): ${prunedStatic.join(", ")}`);
  if (catalog().length === 0) fail("no PRICED model is available — add prices to pricing.json for models your keys can serve");

  console.log(`[nym] starting nym-client "${CLIENT_ID}"…`);
  const running = await nym.start(CLIENT_ID, { port: WS_PORT, verbose: (process.env.VERBOSE ?? process.env.SCRAI_VERBOSE) === "1" });
  const sock = await NymSocket.connect(WS_PORT);
  const address = await sock.selfAddress();

  let stopping = false;
  const shutdown = async () => {
    if (stopping) return;
    stopping = true;
    // Wait for the flush — exiting early corrupts nym-client's SURB store and
    // the next start refuses until it is cleared by hand.
    console.log("\n[nym] shutting down (letting nym-client flush)…");
    sock.close();
    await running.stop();
    money.close();
    process.exit(0);
  };
  // Keep asking the gateway about unpaid invoices. An on-chain payment can
  // confirm an hour after the client gave up waiting, and nothing else would
  // ever notice.
  if (issuer) {
    const every = Number((process.env.SWEEP_SEC ?? process.env.SCRAI_SWEEP_SEC) ?? 120) * 1000;
    setInterval(() => {
      void issuer
        .sweep()
        .then(({ settled }) => {
          if (settled) console.log(`[issuer] swept: ${settled} late payment(s) credited`);
        })
        .catch((e) => console.warn(`[issuer] sweep failed: ${(e as Error).message.split("\n")[0]}`));
      issuer.expireStale();
    }, every).unref();
  }

  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());

  sock.onError((e) => console.error(`[nym] ${e}`));

  sock.onMessage(async (m) => {
    if (!m.senderTag) {
      // Sent with `send` rather than `sendAnonymous`: we have no reply path and
      // the sender exposed their address for nothing. Drop it.
      console.warn("[chat] dropping non-anonymous message (no reply SURBs)");
      return;
    }
    const tag = m.senderTag;
    let price = "";
    const kind = await handle(m.message, (res) => {
      if (res.kind === "chat.ok" || res.kind === "chat.end") {
        price = ` ${res.usage.billing?.priceScrai ?? "?"} TOKU`;
      }
      sock.reply(tag, JSON.stringify(res));
    });
    console.log(`[chat] served ${kind}${price}`);
  });

  console.log("\n──────────────────────────────────────────────────────────────");
  console.log("  scrai-server is listening on the mixnet");
  console.log("");
  console.log(`  ${address}`);
  console.log("");
  console.log("  point a client at it:");
  console.log(`    npm run client -- server ${address}`);
  console.log("──────────────────────────────────────────────────────────────");
  console.log(`models: ${catalog().map((c) => c.model).join(", ")}`);
  if (dormant.length) console.log(`dormant: ${dormant.join(", ")}`);
  const dropped = money.expire(SESSION_TTL_MS);
  const ms = money.stats();
  console.log(
    `payment: ${REQUIRE_PAYMENT ? "ENFORCED" : "OFF (every request is free)"}` +
      ` · ${ms.sessions} sessions · ${ms.spent} tokens burned` +
      (dropped ? ` · ${dropped} expired` : ""),
  );
  if (issuer) {
    const stale = issuer.expireStale();
    console.log(
      `issuer: gateway=${issuer.gatewayName} · methods=${issuer.availableMethods().join(",")}` +
        ` · ${ms.pendingInvoices} invoices pending · ${ms.owedScrai.toLocaleString("en-US")} TOKU owed` +
        (stale ? ` · ${stale} expired` : ""),
    );
    if (issuer.isFake) {
      console.warn("⚠ FAKE PAYMENTS — invoices settle by command, no money moves. Never expose this.");
    } else {
      console.log(`  BTCPay: ${process.env.BTCPAY_URL} · store ${(process.env.BTCPAY_STORE_ID ?? "").slice(0, 8)}…`);
    }
  }
  const age = pricingAgeDays();
  console.log(`pricing table ${pricingVersion()} — margin from MARGIN, server-side only`);
  if (age !== null && age > STALE_PRICING_DAYS) {
    console.warn(
      `⚠ the price table is ${age} days old. Nothing fetches provider prices — they are\n` +
        `  published as documentation, not as an API — so a rate change since then is\n` +
        `  being absorbed by you, not billed. Re-check pricing.json against the sources.`,
    );
  }
  console.log("stateless: no prompt, response, identity, or billing record is written to disk");
}

main().catch((err) => fail(err instanceof Error ? err.message : String(err)));
