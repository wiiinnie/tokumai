// ---------------------------------------------------------------------------
// LOCAL DEV UI BACKEND — NOT A PRODUCTION COMPONENT. DO NOT EXPOSE.
//
// This exists only to develop the desktop/mobile UI in a browser. It runs the
// REAL money layer in-process (mint, issuer, store, adapters) so the UI can
// exercise the whole flow — create an account, buy credit, redeem, chat, see
// the balance move — without the mixnet. Payments use the FAKE gateway: no BTC
// moves, credit is granted on request. That is exactly why it must never leave
// loopback: on a public interface it would hand out free credit and free API
// calls to anyone.
//
// The point of this file is the API SHAPE, not its internals. The frontend
// talks to /api/* and /chat; the Tauri build will expose the same surface from
// the Rust core over IPC, and the blind-ecash internals move there. So this is
// the seam the UI is written against — fenced to dev, retired when Tauri lands.
//
// The real service is src/cli/server.ts (mixnet, real payments, no HTTP).
// ---------------------------------------------------------------------------

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { readFile } from "node:fs/promises";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";

import { registerIfAvailable, keyFor, resolve, catalog } from "./adapter.js";
import { geminiAdapter } from "./adapters/gemini.js";
import { geminiImageAdapter } from "./adapters/gemini-image.js";
import { pollinationsAdapter } from "./adapters/pollinations.js";
import { groqAdapter } from "./adapters/groq.js";
import { cloudflareAdapter } from "./adapters/cloudflare.js";
import { createMeter, retailRate, purchaseTiers } from "./billing.js";
import { warmPricing, pricingVersion } from "./pricing.js";
import { MoneyStore } from "./money/store.js";
import { Mint } from "./money/token.js";
import { Issuer } from "./money/issuer.js";
import { FakeGateway, selectGateway, selectNyxGateway, type PaymentGateway } from "./money/gateway.js";
import { createAccount, fromMnemonic, accountIdFor, fingerprint, deriveSessionKeys } from "./money/account.js";
import { blindPacket, unblindPacket, tierPackets } from "./money/ecash-client.js";
import QRCode from "qrcode";
import type { ChatMessage, GeneratedImage, TokenUsage } from "./types.js";

// Payment gateway: real BTCPay when BTCPAY_URL is set in .env (buy is a real
// invoice paid over Bitcoin/Lightning testnet); otherwise fall back to the fake
// gateway so the UI still runs with no payment backend at all.
if (!process.env.BTCPAY_URL && process.env.SCRAI_FAKE_PAYMENTS !== "1") {
  process.env.SCRAI_FAKE_PAYMENTS = "1";
}

// Gemini key slots (same rule as the Rust server): keep both keys in .env,
// flip by (un)commenting — exactly ONE may be active. The adapters read the
// legacy GEMINI_API_KEY, so the winner is written back into it.
{
  const main = process.env.GEMINI_API_KEY_MAINNET?.trim();
  const test = process.env.GEMINI_API_KEY_TESTNET?.trim();
  if (main && test) {
    console.error(
      "GEMINI_API_KEY_MAINNET and GEMINI_API_KEY_TESTNET are BOTH set — exactly one may be active; comment the other out in .env",
    );
    process.exit(1);
  }
  if (main || test) {
    process.env.GEMINI_API_KEY = main || test;
    console.log(`gemini key active: ${main ? "MAINNET" : "testnet"}`);
  }
}

const PORT = Number(process.env.PORT ?? 8787);
const HOST = process.env.SCRAI_DEV_HOST ?? "127.0.0.1";
const isLoopback = (h: string) => h === "127.0.0.1" || h === "::1" || h === "localhost";

const DEFAULT_MAX_TOKENS = Number(process.env.SCRAI_DEFAULT_MAX_TOKENS ?? 4096);
const THINKING_BUDGET = Number(process.env.SCRAI_THINKING_BUDGET ?? 2048);

// ---- providers ------------------------------------------------------------
const PROVIDERS = [geminiAdapter, geminiImageAdapter, pollinationsAdapter, groqAdapter, cloudflareAdapter];
for (const a of PROVIDERS) registerIfAvailable(a);
if (catalog().length === 0) {
  console.error("no provider configured — set at least one credential in .env (e.g. GEMINI_API_KEY)");
  process.exit(1);
}

// ---- money layer (in-process) ---------------------------------------------
const money = new MoneyStore(process.env.SCRAI_DEV_MONEY_DB ?? "./data/dev-money.db");
const mint = new Mint(
  process.env.SCRAI_ISSUER_SECRET ? Buffer.from(process.env.SCRAI_ISSUER_SECRET, "utf8") : money.getOrCreateMintSeed(),
);
const gateway = selectGateway();
const devGateways: Record<string, PaymentGateway> = { btc: gateway };
const devNyx = await selectNyxGateway();
if (devNyx) { devGateways.nyx = devNyx; devNyx.warmup?.(); }
const issuer = new Issuer(money, devGateways, mint);
const fakePayments = issuer.isFake;

// ---- dev wallet state (account + held ecash), persisted to one JSON file ---
// The browser is thin: the account phrase, held tokens and session index live
// here, exactly where the Rust core will keep them on the device later.
interface DevWallet {
  mnemonic?: string;
  sessionIndex: number;
  ecash: Array<Array<{ amount: number; secret: string; C: string }>>; // held packets
}
const WALLET_FILE = process.env.SCRAI_DEV_WALLET ?? "./data/dev-ui.json";
function loadWallet(): DevWallet {
  try {
    return { sessionIndex: 0, ecash: [], ...JSON.parse(readFileSync(WALLET_FILE, "utf8")) };
  } catch {
    return { sessionIndex: 0, ecash: [] };
  }
}
function saveWallet(w: DevWallet): void {
  mkdirSync(dirname(WALLET_FILE), { recursive: true });
  writeFileSync(WALLET_FILE, JSON.stringify(w, null, 2) + "\n");
}

function sessionKeys(w: DevWallet) {
  if (!w.mnemonic) throw new Error("no account — create one first");
  return deriveSessionKeys(w.mnemonic, w.sessionIndex);
}
function heldTotal(w: DevWallet): number {
  return w.ecash.reduce((n, pkt) => n + pkt.reduce((m, p) => m + p.amount, 0), 0);
}
function sessionBalance(w: DevWallet): number {
  if (!w.mnemonic) return 0;
  return money.getSession(sessionKeys(w).sessionId)?.balance ?? 0;
}

// ---- ceiling (mirrors cli/server.ts so the reservation is a true upper bound)
function ceilingFor(model: string, messages: Array<{ content: string }>, maxTokens?: number): number {
  const rate = retailRate(model);
  const inTokens = messages.reduce((n, m) => n + Buffer.byteLength(m.content ?? "", "utf8"), 0);
  const outTokens = (maxTokens ?? DEFAULT_MAX_TOKENS) + THINKING_BUDGET;
  return Math.ceil((inTokens * rate.in + outTokens * rate.out) / 1_000_000);
}

// ---- ecash operations, in-process -----------------------------------------

/** Withdraw ALL current entitlement into held ecash packets (one per tier). */
function collectEntitlement(w: DevWallet): { collected: number; held: number } {
  const account = fromMnemonic(w.mnemonic!);
  const entitlement = issuer.entitlement(account.accountId);
  if (!entitlement) return { collected: 0, held: heldTotal(w) };
  const keys = mint.publicKeys();
  for (const packetScrai of tierPackets(entitlement)) {
    const { outputs, state } = blindPacket(packetScrai);
    const { signatures } = issuer.withdraw(account.accountId, outputs);
    w.ecash.push(unblindPacket(state, signatures, keys));
    saveWallet(w); // persist each packet as it is drawn
  }
  return { collected: entitlement, held: heldTotal(w) };
}

/** Fake gateway only: create + settle an invoice instantly, then collect. */
async function buyCreditFake(w: DevWallet, usd: number): Promise<{ collected: number; held: number }> {
  const account = fromMnemonic(w.mnemonic!);
  const inv = await issuer.createInvoice(account.accountId, usd); // enforces the tier
  (gateway as FakeGateway).markPaid(inv.providerRef); // dev: what a real payment would have done
  issuer.settle(inv.providerRef);
  return collectEntitlement(w);
}

/** Redeem held packets into the session, one clean tier per redemption. */
function redeemHeld(w: DevWallet): number {
  if (!w.ecash.length) return sessionBalance(w);
  const k = sessionKeys(w);
  let remaining = [...w.ecash];
  while (remaining.length) {
    const pkt = remaining[0]!;
    const amount = pkt.reduce((n, p) => n + p.amount, 0);
    // Verify every proof before touching state, then burn+credit atomically.
    for (const p of pkt) if (!mint.verify(p)) throw new Error("a held token failed to verify");
    money.redeemProofs(k.sessionId, k.publicKey, pkt.map((p) => p.secret), amount);
    remaining = remaining.slice(1);
    w.ecash = remaining;
    saveWallet(w);
  }
  return sessionBalance(w);
}

// ---- HTTP plumbing --------------------------------------------------------

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((res, rej) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => res(data));
    req.on("error", rej);
  });
}
async function readJson<T>(req: IncomingMessage): Promise<T> {
  const raw = await readBody(req);
  return (raw ? JSON.parse(raw) : {}) as T;
}
function sendJson(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(body));
}
function sse(res: ServerResponse) {
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache, no-store", connection: "keep-alive" });
  return (event: unknown) => res.write(`data: ${JSON.stringify(event)}\n\n`);
}

function statePayload() {
  const w = loadWallet();
  const account = w.mnemonic
    ? { fingerprint: fingerprint(accountIdFor(fromMnemonic(w.mnemonic).publicKey)), sessionIndex: w.sessionIndex }
    : null;
  return {
    account,
    balance: sessionBalance(w),
    held: heldTotal(w),
    tiers: purchaseTiers(),
    fakePayments,
    gateway: issuer.gatewayName,
    models: catalog().map((m) => ({ ...m, rate: retailRate(m.model) })),
    pricingVersion: pricingVersion(),
  };
}

const server = createServer(async (req, res) => {
  try {
    const url = req.url ?? "/";

    // ---- static UI ----
    if (req.method === "GET" && (url === "/" || url === "/index.html")) {
      const html = await readFile(fileURLToPath(new URL("../public/index.html", import.meta.url)));
      res.writeHead(200, { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" });
      return void res.end(html);
    }
    if (req.method === "GET" && /^\/[a-zA-Z0-9_-]+\.js$/.test(url)) {
      try {
        const js = await readFile(fileURLToPath(new URL("../public" + url, import.meta.url)));
        res.writeHead(200, { "content-type": "text/javascript; charset=utf-8", "cache-control": "no-store" });
        return void res.end(js);
      } catch {
        return void res.writeHead(404).end();
      }
    }

    // ---- state / catalog ----
    if (req.method === "GET" && (url === "/api/state" || url === "/models")) {
      const s = statePayload();
      return void sendJson(res, 200, url === "/models" ? s.models : s);
    }

    // ---- account ----
    if (req.method === "POST" && url === "/api/account/new") {
      const w = loadWallet();
      const a = createAccount();
      saveWallet({ mnemonic: a.mnemonic, sessionIndex: (w.mnemonic ? w.sessionIndex : 0), ecash: [] });
      return void sendJson(res, 200, { mnemonic: a.mnemonic, fingerprint: fingerprint(a.accountId) });
    }
    if (req.method === "GET" && url === "/api/account/reveal") {
      const w = loadWallet();
      if (!w.mnemonic) return void sendJson(res, 404, { error: "no account" });
      return void sendJson(res, 200, { mnemonic: w.mnemonic });
    }
    if (req.method === "POST" && url === "/api/account/restore") {
      const { mnemonic } = await readJson<{ mnemonic?: string }>(req);
      let account;
      try {
        account = fromMnemonic(mnemonic ?? "");
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }
      // Scan derived sessions for a funded one (dev store is local).
      let best = 0;
      let bestBal = 0;
      for (let i = 0, empties = 0; empties < 3; i++) {
        const bal = money.getSession(deriveSessionKeys(account.mnemonic, i).sessionId)?.balance ?? 0;
        if (bal > 0) { if (bal > bestBal) { bestBal = bal; best = i; } empties = 0; } else empties++;
      }
      saveWallet({ mnemonic: account.mnemonic, sessionIndex: best, ecash: [] });
      return void sendJson(res, 200, { fingerprint: fingerprint(account.accountId), balance: bestBal });
    }

    // ---- credit / redeem ----
    // Fake gateway only: instant buy (create + settle + collect in one call).
    if (req.method === "POST" && url === "/api/credit") {
      if (!fakePayments) return void sendJson(res, 400, { error: "real payments are configured — use /api/invoice" });
      const w = loadWallet();
      if (!w.mnemonic) return void sendJson(res, 400, { error: "no account — create one first" });
      const { usd } = await readJson<{ usd?: number }>(req);
      if (!purchaseTiers().includes(Number(usd))) {
        return void sendJson(res, 400, { error: `fixed amounts only: ${purchaseTiers().map((t) => `$${t}`).join(", ")}` });
      }
      try {
        const r = await buyCreditFake(w, Number(usd));
        return void sendJson(res, 200, { ...r, balance: sessionBalance(loadWallet()) });
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }
    }
    // Real gateway: raise an invoice to pay over Bitcoin/Lightning.
    if (req.method === "POST" && url === "/api/invoice") {
      const w = loadWallet();
      if (!w.mnemonic) return void sendJson(res, 400, { error: "no account — create one first" });
      const { usd, method } = await readJson<{ usd?: number; method?: string }>(req);
      if (!purchaseTiers().includes(Number(usd))) {
        return void sendJson(res, 400, { error: `fixed amounts only: ${purchaseTiers().map((t) => `$${t}`).join(", ")}` });
      }
      try {
        const account = fromMnemonic(w.mnemonic);
        const inv = await issuer.createInvoice(account.accountId, Number(usd), method === "nyx" ? "nyx" : "btc");
        // A scannable QR per payment method, generated server-side (no browser QR
        // lib, no CSP headache). The frontend renders whichever it shows. NYM gets
        // the Nym-purple treatment to match the branded QR the desktop app draws.
        const options = await Promise.all(
          inv.options.map(async (o) => ({
            ...o,
            qr: await QRCode.toString(o.uri || o.destination, {
              type: "svg",
              margin: 1,
              width: 200,
              ...(o.method === "NYM" ? { color: { dark: "#7A5FFF", light: "#ffffff" } } : {}),
            }),
          })),
        );
        // Dev shortcut only — the real app never opens BTCPay's own page (IP leak).
        const checkout = process.env.BTCPAY_URL
          ? `${process.env.BTCPAY_URL.replace(/\/+$/, "")}/i/${inv.providerRef}`
          : "";
        return void sendJson(res, 200, {
          invoiceId: inv.id, amountUsd: inv.amountUsd, amountScrai: inv.amountScrai,
          expiresAt: inv.expiresAt, instruction: inv.instruction, options, checkout,
        });
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }
    }
    // Poll an invoice's status.
    if (req.method === "POST" && url === "/api/invoice/cancel") {
      const { id } = await readJson<{ id?: string }>(req);
      if (!id) return void sendJson(res, 400, { error: "id required" });
      return void sendJson(res, 200, { ok: issuer.cancel(id) });
    }
    if (req.method === "GET" && url.startsWith("/api/invoice/")) {
      const id = decodeURIComponent(url.slice("/api/invoice/".length));
      try {
        const st = await issuer.status(id);
        if (!st) return void sendJson(res, 404, { error: "unknown invoice" });
        return void sendJson(res, 200, st); // { status, entitlement }
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }
    }
    // Collect paid-for entitlement into held ecash.
    if (req.method === "POST" && url === "/api/collect") {
      const w = loadWallet();
      if (!w.mnemonic) return void sendJson(res, 400, { error: "no account" });
      try {
        await issuer.sweep(); // catch any late confirmation first
        const r = collectEntitlement(w);
        return void sendJson(res, 200, { ...r, balance: sessionBalance(loadWallet()) });
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }
    }
    if (req.method === "POST" && url === "/api/redeem") {
      const w = loadWallet();
      if (!w.mnemonic) return void sendJson(res, 400, { error: "no account" });
      try {
        return void sendJson(res, 200, { balance: redeemHeld(w) });
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }
    }

    // ---- chat (paid, streamed) ----
    if (req.method === "POST" && url === "/chat") {
      let body: { model: string; messages: ChatMessage[]; maxTokens?: number; temperature?: number; imageSize?: string };
      try {
        body = await readJson(req);
        if (!body.model || !Array.isArray(body.messages)) throw new Error("bad request");
      } catch {
        return void sendJson(res, 400, { error: "malformed request" });
      }

      let w = loadWallet();
      if (!w.mnemonic) return void sendJson(res, 402, { error: "no account — create one and buy credit" });
      // Fund the session from held ecash on first use (the anonymous half).
      try {
        if (w.ecash.length) { redeemHeld(w); w = loadWallet(); }
      } catch (e) {
        return void sendJson(res, 400, { error: (e as Error).message });
      }

      const price = retailRate(body.model);
      const free = !price.in && !price.out;
      const k = sessionKeys(w);
      const session = money.getSession(k.sessionId);
      if (!session) return void sendJson(res, 402, { error: "no funded session — buy credit first" });

      const ceiling = ceilingFor(body.model, body.messages, body.maxTokens);
      let reserved = 0;
      if (!free) {
        const counter = session.counter + 1;
        const outcome = money.reserve(k.sessionId, counter, ceiling);
        if (outcome !== "ok") {
          return void sendJson(res, 402, {
            error: outcome === "insufficient" ? `not enough SCRAI: need up to ${ceiling}, balance ${session.balance}` : outcome,
          });
        }
        reserved = ceiling;
      }

      const send = sse(res);
      const meter = createMeter({ model: body.model, promptChars: JSON.stringify(body.messages).length });
      let images: GeneratedImage[] | undefined;
      const answer = body.maxTokens ?? DEFAULT_MAX_TOKENS;
      try {
        const adapter = resolve(body.model);
        const stream = adapter.stream(
          { model: body.model, messages: body.messages, maxTokens: answer, thinkingBudget: THINKING_BUDGET,
            ...(body.temperature != null ? { temperature: body.temperature } : {}),
            ...(typeof body.imageSize === "string" ? { imageSize: body.imageSize } : {}) },
          keyFor(adapter),
        );
        for await (const chunk of meter.wrap(stream)) {
          if (chunk.images?.length) images = [...(images ?? []), ...chunk.images];
          if (chunk.delta) send({ delta: chunk.delta });
          if (chunk.done && chunk.usage) {
            const usage = chunk.usage;
            const balance = reserved
              ? money.settle(k.sessionId, reserved, usage.billing?.priceScrai ?? 0)
              : sessionBalance(loadWallet());
            send({ done: true, usage, balance, ...(images?.length ? { images } : {}) });
            return void res.end();
          }
        }
        const usage: TokenUsage = { ...meter.usage(), billing: meter.frame() };
        const balance = reserved ? money.settle(k.sessionId, reserved, usage.billing?.priceScrai ?? 0) : sessionBalance(loadWallet());
        send({ done: true, usage, balance, ...(images?.length ? { images } : {}) });
        return void res.end();
      } catch (err) {
        if (reserved) money.refund(k.sessionId, reserved); // provider failed → user pays nothing
        send({ error: err instanceof Error ? err.message : "upstream error" });
        return void res.end();
      }
    }

    res.writeHead(404).end();
  } catch (err) {
    if (!res.headersSent) sendJson(res, 500, { error: err instanceof Error ? err.message : "server error" });
    else res.end();
  }
});

if (!isLoopback(HOST)) {
  console.error(
    "\nREFUSING TO START: this DEV backend grants free credit and has no real auth.\n" +
      `  Binding to a non-loopback host (SCRAI_DEV_HOST=${HOST}) would expose a free relay.\n` +
      "  Use the mixnet server (npm run server) for anything reachable from outside.\n",
  );
  process.exit(1);
}

server.listen(PORT, HOST, () => {
  warmPricing();
  const pay = fakePayments ? "FAKE payments (dev)" : `BTCPay: ${process.env.BTCPAY_URL}`;
  console.log(`⚠ DEV UI backend (no real auth) — loopback only, never expose this.`);
  console.log(`scrambler dev UI on http://${HOST}:${PORT} — models: ${catalog().map((c) => c.model).join(", ")}`);
  console.log(`pricing table ${pricingVersion()} · tiers ${purchaseTiers().map((t) => `$${t}`).join(" ")} · ${pay}`);
  console.log(`wallet: ${WALLET_FILE} · money: dev store`);
});
