#!/usr/bin/env node
// ---------------------------------------------------------------------------
// scrai-client — talk to the AI service over the mixnet.
//
// Owns the nym-client lifecycle (install, init, gateway choice, run), knows the
// server's Nym address, and speaks the protocol in ../protocol.ts. It never
// learns an API key and never computes a price — the server sends a finished
// one, exactly as in the web client.
//
// v2 SEAM: every mixnet call goes through NymSocket. When the Tauri Rust core
// lands it replaces that module and this file's command surface is unchanged.
// ---------------------------------------------------------------------------

import { createInterface } from "node:readline/promises";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve as resolvePath } from "node:path";
import { formatScraiCli, formatTokensCli } from "./format.js";
import * as cfg from "./config.js";
import * as nym from "../nym/process.js";
import { listAvailableGateways, byCountry } from "../nym/directory.js";
import { install, platformSupported, unsupportedPlatformMessage } from "../nym/install.js";
import { NymSocket, DEFAULT_WS_PORT } from "../nym/socket.js";
import {
  PROTOCOL_VERSION,
  newId,
  parseResponse,
  type Request,
  type ModelInfo,
  type Proof,
  type PublicKey,
  type SignedOutput,
} from "../protocol.js";
import { StreamAssembler } from "../assembler.js";
import { signRequest as signSession } from "../money/session.js";
import { blindPacket, unblindPacket, tierPackets } from "../money/ecash-client.js";
import type { Account } from "../money/account.js";
import { purchaseTiers } from "../billing.js";
import { Progress, humanBytes } from "./progress.js";
import * as wallet from "./wallet.js";
import { createAccount, fromMnemonic, fingerprint, maskMnemonic, signAsAccount, deriveSessionKeys } from "../money/account.js";
import { randomBytes } from "node:crypto";
import qrcode from "qrcode-terminal";
import type { ChatMessage } from "../types.js";

// The server's nym-client already owns nym's default 1977. Running both on one
// machine is the normal dev case, so the client lives one port up. The value is
// baked into the config at `setup` time, so changing it means setting it before
// setup — not after.
const WS_PORT = Number(process.env.SCRAI_NYM_WS_PORT ?? DEFAULT_WS_PORT + 1);
const REPLY_TIMEOUT_MS = Number(process.env.SCRAI_TIMEOUT_MS ?? 120_000);
/** Gap between payment checks. Generous because each one crosses the mixnet twice. */
const POLL_INTERVAL_MS = Number(process.env.SCRAI_POLL_MS ?? 15_000);

function fail(msg: string): never {
  console.error(msg);
  process.exit(1);
}

function flag(name: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i > -1 ? process.argv[i + 1] : undefined;
}
const hasFlag = (name: string) => process.argv.includes(`--${name}`);

// ---- one mixnet round trip -------------------------------------------------
// Prefer a client that is already running (`daemon`), and start a throwaway one
// only if none is.
//
// The throwaway used to be the deliberate choice here, on the theory that a
// short-lived client is a smaller target. That was backwards. A connected
// nym-client emits loop cover traffic continuously, so a persistent client's
// real messages are hidden inside a stream that looks identical when idle.
// Connecting only when there is something to send discards that cover entirely:
// every connection then IS a request, and its timing is the signal. Short-lived
// clients leak activity timestamps; long-lived ones leak only presence.

/**
 * Bring up a mixnet client if needed, hand it to `fn`, and tear down only what
 * this call actually started.
 */
async function withMixnet<T>(
  fn: (sock: NymSocket, server: string, p?: Progress) => Promise<T>,
  p?: Progress,
): Promise<T> {
  const c = cfg.load();
  if (!c.serverAddress) {
    fail("no server address configured.\n  set it with:  npm run client -- server <nym-address>");
  }
  if (!nym.isInitialised(c.clientId)) {
    fail(`nym client "${c.clientId}" is not initialised.\n  run:  npm run client:setup`);
  }

  // Reuse a client that is already up (see `daemon`). Only ever tear down what
  // we started ourselves — killing someone else's long-lived client would drop
  // its cover traffic and defeat the reason it is running.
  const existing = nym.findRunning(c.clientId);
  let started: nym.RunningClient | null = null;

  if (existing) {
    p?.phase("using the running client");
  } else {
    started = await nym.start(c.clientId, {
      port: WS_PORT,
      verbose: process.env.SCRAI_VERBOSE === "1",
      onPhase: (phase) => p?.phase(phase),
    });
  }

  p?.phase("opening websocket");
  const sock = await NymSocket.connect(WS_PORT);
  try {
    return await fn(sock, c.serverAddress, p);
  } finally {
    sock.close();
    if (started) {
      p?.phase("shutting down");
      await started.stop();
    }
    p?.stop();
  }
}

/**
 * Reply budget for this request. An image answer is ~1000x a text one, and
 * asking for too few SURBs truncates it with no error the server can see — so
 * the budget follows the model kind rather than one global default.
 */
function surbsFor(c: cfg.Config): number {
  return c.modelKind === "image" ? c.imageSurbs : c.replySurbs;
}

/**
 * The issuer's public keyset — needed to blind against, to verify the DLEQ on
 * what comes back, and to unblind it. PINNED in the config: if the keyset id
 * ever changes, warn loudly, because a server that quietly swaps in per-user
 * keys could otherwise tag withdrawals despite the blinding.
 */
async function fetchKeys(): Promise<PublicKey[]> {
  const res = (await roundTrip({ v: PROTOCOL_VERSION, kind: "keys", id: newId() })) as {
    keysetId: string;
    keys: PublicKey[];
  };
  const pinned = cfg.load().issuerKeysetId;
  if (pinned && pinned !== res.keysetId) {
    console.warn(
      `  ⚠ the issuer's keyset changed (${pinned} → ${res.keysetId}).\n` +
        `    If you did not expect a key rotation, stop — a changed keyset can be\n` +
        `    used to de-anonymise withdrawals.`,
    );
  }
  cfg.save({ issuerKeysetId: res.keysetId });
  return res.keys;
}

/**
 * Withdraw ONE tier packet: blind fresh secrets, have the issuer sign them
 * blind, unblind (with the DLEQ anti-tagging check) into spendable proofs. The
 * crypto lives in ecash-client.ts, shared with the dev UI backend.
 */
async function signPacket(account: Account, amountScrai: number, keys: PublicKey[]): Promise<Proof[]> {
  const { outputs, state } = blindPacket(amountScrai);
  const nonce = randomBytes(16).toString("hex");
  const res = (await roundTrip({
    v: PROTOCOL_VERSION,
    kind: "withdraw",
    id: newId(),
    publicKey: account.publicKey,
    outputs,
    nonce,
    sig: signAsAccount(account, `withdraw:${amountScrai}`, nonce),
  })) as { keysetId: string; signatures: SignedOutput[] };
  return unblindPacket(state, res.signatures, keys);
}

/**
 * Withdraw an entitlement as tier packets and hold them locally.
 *
 * One account-signed withdrawal per packet, each stored the moment it is drawn
 * so a crash mid-way cannot lose what was already signed. Nothing is redeemed
 * here — that happens later, apart in time (see redeemHeld).
 */
async function withdrawPackets(account: Account, entitlementScrai: number): Promise<void> {
  const keys = await fetchKeys();
  for (const packetScrai of tierPackets(entitlementScrai)) {
    const proofs = await signPacket(account, packetScrai, keys);
    wallet.storePacket(proofs);
  }
}

/**
 * Buy SCRAI and fund the session.
 *
 * Two steps on purpose, because that is the shape real money has: an ISSUER
 * blind-signs bearer tokens (after a payment), and the SERVER redeems them into
 * a session funded by a fresh, unlinkable key.
 */
async function topUp(usd: number, onStep?: (msg: string) => void): Promise<number> {
  const c = cfg.load();
  if (!c.mnemonic) {
    fail("no account yet — create one first:\n  npm run client -- account new");
  }
  const account = fromMnemonic(c.mnemonic);
  const nonce = () => randomBytes(16).toString("hex");

  // 1. Raise an invoice. Signed with the ACCOUNT key — this is one of only two
  //    moments the issuer learns which account it is dealing with.
  const n1 = nonce();
  const inv = (await roundTrip({
    v: PROTOCOL_VERSION,
    kind: "invoice.create",
    id: newId(),
    publicKey: account.publicKey,
    usd,
    method: "btc",
    nonce: n1,
    sig: signAsAccount(account, `invoice:${usd}`, n1),
  })) as {
    invoiceId: string;
    payTo: string;
    instruction: string;
    options?: import("../protocol.js").PaymentOption[];
    amountUsd: number;
    amountScrai: number;
    expiresAt: number;
  };

  console.log("");
  console.log(`  Pay USD ${inv.amountUsd.toFixed(2)} for ${wallet.fmt(inv.amountScrai)} SCRAI`);
  console.log(`  ${inv.instruction}`);
  console.log(`  expires ${new Date(inv.expiresAt).toLocaleTimeString()}`);
  showPaymentOptions(inv.options ?? []);

  // 2. Wait for the money. Polling rather than waiting for a push, because a
  //    webhook that never arrives must not strand a paying customer.
  //
  //    The indicator is not decoration: without it the command prints an
  //    invoice and then falls silent for minutes, which reads as a crash. Each
  //    poll is a real mixnet round trip, so the glyph only advances when one
  //    actually completed.
  const deadline = Math.min(inv.expiresAt, Date.now() + 60 * 60_000);
  // Gaps of POLL_INTERVAL_MS are the design here, so only genuinely long
  // silence is worth flagging.
  const prog = new Progress("waiting for payment", POLL_INTERVAL_MS * 3);
  let entitlement = 0;
  try {
    for (let attempt = 0; Date.now() < deadline; attempt++) {
      const st = (await roundTrip({
        v: PROTOCOL_VERSION,
        kind: "invoice.status",
        id: newId(),
        invoiceId: inv.invoiceId,
      })) as { status: string; entitlement: number };

      if (st.status === "paid") {
        entitlement = st.entitlement;
        break;
      }
      if (st.status === "expired") throw new Error("the invoice expired before it was paid");

      const left = Math.max(0, Math.round((deadline - Date.now()) / 1000));
      prog.event(`checked ${attempt + 1}× · ${Math.floor(left / 60)}m${String(left % 60).padStart(2, "0")}s left`);
      onStep?.(`waiting for payment … (${attempt + 1})`);
      // Each poll is a full mixnet round trip of its own — several seconds out
      // and several back. Polling every 5s would keep the client talking
      // continuously for no benefit, since the thing being waited on is a human
      // making a payment.
      await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
    }
  } finally {
    prog.stop();
  }
  if (!entitlement) {
    throw new Error(
      "gave up waiting for the payment after 60 minutes.\n" +
        "  Nothing is lost: if the payment lands later, collect it with\n" +
        "    npm run client -- claim",
    );
  }

  // The entitlement may exceed this invoice: earlier purchases that were paid
  // but never collected are still owed, and a withdrawal takes everything.
  // Saying "+500,000 for USD 5.00" while moving 1,600,000 would be a lie about
  // money, even though the balance ends up right.
  const carried = Math.max(0, entitlement - inv.amountScrai);
  console.log(`  payment received — ${wallet.fmt(inv.amountScrai)} SCRAI for USD ${inv.amountUsd.toFixed(2)}`);
  if (carried > 0) {
    console.log(`  plus ${wallet.fmt(carried)} SCRAI from earlier purchases never collected`);
  }
  console.log("  withdrawing …");

  // 3. Trade entitlement for BLIND-signed bearer tokens, in tier packets. The
  //    issuer signs values it cannot read, so this is where the account stops
  //    being connected to what gets spent.
  // 4. HOLD the packets; do NOT redeem them now. Redeeming immediately would put
  //    the anonymous session.open seconds after the account-signed withdrawal,
  //    and the server could relink the two by timing — undoing the blinding. The
  //    packets are redeemed later, when the session is first used (redeemHeld),
  //    so the account-linked half and the anonymous half happen apart in time.
  await withdrawPackets(account, entitlement);
  return entitlement;
}

/**
 * Redeem locally-held ecash into the session — the ANONYMOUS half of the split.
 *
 * This is a mixnet round trip of its own, separated in time from the withdrawal,
 * so the server cannot tie the tokens back to the account that bought them. The
 * longer the gap the user leaves, the larger the anonymity set.
 *
 * Idempotent and crash-safe: the tokens are only cleared AFTER the server
 * confirms it took them, and if a previous redemption already went through but
 * its reply was lost (the tokens come back "already spent"), the value is on the
 * session — adopt it and drop the now-useless local copies rather than retrying
 * forever.
 */
async function redeemHeld(): Promise<number> {
  let remaining = wallet.heldPackets();
  if (!remaining.length) return wallet.read().balance;

  const k = wallet.keys();
  let skipped = false;

  while (remaining.length) {
    const packet = remaining[0]!;
    try {
      const opened = (await roundTrip({
        v: PROTOCOL_VERSION,
        kind: "session.open",
        id: newId(),
        proofs: packet,
        publicKey: k.publicKey,
      })) as { balance: number };
      wallet.syncBalance(opened.balance);
    } catch (err) {
      // A packet that comes back "already spent" was redeemed on an earlier run
      // whose reply was lost — its value is already on the session. Drop it and
      // move on. Any other error is real: stop and keep the rest held.
      if (!(err instanceof Error) || !/already been spent/.test(err.message)) throw err;
      skipped = true;
    }
    // Record progress after EACH packet: a crash mid-loop must not re-present a
    // packet already burned server-side.
    remaining = remaining.slice(1);
    wallet.setPackets(remaining);
  }

  // If we skipped an already-spent packet, the mirrored balance may be behind;
  // pull the authoritative figure once at the end.
  if (skipped) {
    const st = await sessionStatus();
    return st?.balance ?? wallet.read().balance;
  }
  return wallet.read().balance;
}

/** Redeem any held ecash before spending, so the session actually has the funds. */
async function ensureFunded(): Promise<void> {
  if (!wallet.heldPackets().length) return;
  process.stderr.write("  redeeming held credit …\n");
  await redeemHeld();
}

/**
 * Show how to pay, here in the terminal.
 *
 * The QR is rendered locally from a URI that came over the mixnet — no browser,
 * no page load, nothing that would let a payment server see the user's IP.
 * Lightning goes first when present because it settles in seconds where
 * on-chain waits for confirmations.
 */
function showPaymentOptions(options: import("../protocol.js").PaymentOption[]): void {
  if (!options.length) return;

  const sorted = [...options].sort((a, b) =>
    Number(/lightning/i.test(b.method)) - Number(/lightning/i.test(a.method)),
  );

  for (const o of sorted) {
    const lightning = /lightning/i.test(o.method);
    console.log("");
    console.log(`  ── ${lightning ? "Lightning (instant)" : o.method} ──`);
    if (o.amount) console.log(`     ${o.amount} ${o.currency}`);
    // Terminal QR only when there is a terminal; piped output stays parseable.
    if (process.stdout.isTTY) {
      qrcode.generate(o.uri, { small: true }, (qr: string) => {
        console.log(qr.split("\n").map((l) => "     " + l).join("\n"));
      });
    }
    console.log(`     ${o.destination}`);
  }
  console.log("");
}

/**
 * Collect an entitlement that was paid for but never withdrawn.
 *
 * A purchase is two round trips with a human payment in the middle, so it WILL
 * be interrupted — the client is closed, the laptop sleeps, the network drops.
 * Without this the money is stuck: the issuer has been paid and holds the
 * entitlement, and the only client that knew the invoice id is gone.
 *
 * Idempotent by construction: it withdraws whatever is outstanding, and if that
 * is nothing it says so instead of failing.
 */
async function claimEntitlement(): Promise<number | null> {
  const c = cfg.load();
  if (!c.mnemonic) fail("no account — npm run client -- account new");
  const account = fromMnemonic(c.mnemonic);

  // Ask the server, over a signed read-only query, exactly what this account is
  // owed — then withdraw that much.
  const nonce = randomBytes(16).toString("hex");
  const owedRes = (await roundTrip({
    v: PROTOCOL_VERSION,
    kind: "entitlement",
    id: newId(),
    publicKey: account.publicKey,
    nonce,
    sig: signAsAccount(account, "entitlement", nonce),
  })) as { entitlement: number };
  const owed = owedRes.entitlement;
  if (!owed) return null;

  // Hold the packets, same as a purchase — redemption happens later, on use.
  await withdrawPackets(account, owed);
  return owed;
}

/**
 * Find the funded sessions belonging to a recovery phrase.
 *
 * Sessions are derived by index, so recovery is a scan: derive 0, 1, 2 … and
 * ask the server which hold a balance. It stops after `gap` consecutive empty
 * ones — the same idea HD wallets use, because otherwise the scan never ends.
 *
 * Each query is a full mixnet round trip, so the gap is deliberately small.
 */
async function scanSessions(
  mnemonic: string,
  gap = 3,
  onStep?: (i: number, balance: number) => void,
): Promise<Array<{ index: number; balance: number; sessionId: string }>> {
  const found: Array<{ index: number; balance: number; sessionId: string }> = [];
  let empties = 0;

  for (let i = 0; empties < gap; i++) {
    const k = deriveSessionKeys(mnemonic, i);
    let balance = 0;
    try {
      const r = (await roundTrip({
        v: PROTOCOL_VERSION,
        kind: "session.status",
        id: newId(),
        sessionId: k.sessionId,
        sig: signRequestFor(k, "status"),
      })) as { balance: number };
      balance = r.balance;
    } catch {
      balance = 0; // unknown session — nothing was ever opened at this index
    }
    onStep?.(i, balance);
    if (balance > 0) {
      found.push({ index: i, balance, sessionId: k.sessionId });
      empties = 0;
    } else {
      empties += 1;
    }
  }
  return found;
}

/** Sign as a specific derived key, rather than whichever one is configured. */
function signRequestFor(k: import("../money/session.js").SessionKeys, body: string): string {
  return signSession(k, 0, body);
}

/** Thrown when the server refused a counter it had already seen. Retryable exactly once. */
class CounterDrift extends Error {}

/**
 * Ask the server where this session stands.
 *
 * Used at startup and after a counter rejection. Read-only, so it needs no
 * counter of its own — which matters, because needing a valid counter to fix a
 * broken counter would be circular.
 */
async function sessionStatus(): Promise<{ balance: number; counter: number } | null> {
  const k = wallet.keys();
  try {
    const r = (await roundTrip({
      v: PROTOCOL_VERSION,
      kind: "session.status",
      id: newId(),
      sessionId: k.sessionId,
      sig: wallet.signStatus(),
    })) as { balance: number; counter?: number };
    wallet.syncBalance(r.balance);
    if (typeof r.counter === "number") wallet.syncCounter(r.counter);
    return { balance: r.balance, counter: r.counter ?? 0 };
  } catch {
    return null; // no session yet, or the server is unreachable
  }
}

/** Single request, single answer. Used by `models` and by --no-stream chat. */
async function roundTrip(req: Request, p?: Progress): Promise<unknown> {
  const c = cfg.load();
  return withMixnet(async (sock, server) => {
    const once = async (body: Request) => {
      sock.sendAnonymous(server, JSON.stringify(body), surbsFor(c));
      p?.phase("sent · waiting for reply");
      const reply = await sock.nextMessage(REPLY_TIMEOUT_MS);
      p?.event(humanBytes(Buffer.byteLength(reply.message, "utf8")) + " received");
      const res = parseResponse(reply.message);
      if (!res) throw new Error("server sent something that is not a protocol message");
      return res;
    };

    let res = await once(req);

    // Counter drift is recoverable: adopt the server's number, re-sign, retry
    // once. Only once — a second rejection is a real problem, not drift.
    if (res.kind === "error" && res.reason === "replay" && typeof res.counter === "number" && req.kind === "chat") {
      wallet.syncCounter(res.counter);
      p?.phase("counter resynced · retrying");
      res = await once(signChat(req as Request & { kind: "chat" }));
    }

    if (res.kind === "error") throw new Error(`server: ${res.error}`);
    return res;
  }, p);
}

/**
 * Streaming round trip.
 *
 * Frames arrive as independent mixnet messages in whatever order the network
 * felt like, so everything goes through the assembler and `onDelta` only ever
 * sees text that is safe to print. Two distinct failures are surfaced rather
 * than papered over: a stall (nothing at all for a while) and a hole (the
 * terminator arrived but a chunk did not). Silently returning a short answer
 * would be the worst outcome — it reads as the model's own words.
 */
async function streamRoundTrip(
  req: Request,
  onDelta: (text: string) => void,
  p?: Progress,
): Promise<{
  usage: import("../types.js").TokenUsage;
  images?: import("../types.js").GeneratedImage[];
  /** First-frame-to-last-frame window — the only honest transfer measurement. */
  transferMs?: number;
  /** Balance the SERVER reports after settling. Authoritative. */
  balance?: number;
}> {
  const c = cfg.load();
  const idleMs = Number(process.env.SCRAI_IDLE_MS ?? 45_000);
  let frames = 0;
  let bytes = 0;
  let firstFrameAt = 0;
  let lastFrameAt = 0;

  return withMixnet(
    (sock, server) =>
      new Promise((resolveP, rejectP) => {
        const asm = new StreamAssembler();
        let usage: import("../types.js").TokenUsage | null = null;
        let images: import("../types.js").GeneratedImage[] | undefined;
        let balance: number | undefined;
        let settled = false;

        const settle = (fn: () => void) => {
          if (settled) return;
          settled = true;
          clearTimeout(idle);
          clearTimeout(overall);
          fn();
        };

        const check = () => {
          if (usage && asm.done) {
            const transferMs = frames > 1 ? lastFrameAt - firstFrameAt : undefined;
            settle(() => resolveP({ usage: usage!, images, transferMs, balance }));
          }
        };

        let idle: NodeJS.Timeout;
        const bumpIdle = () => {
          clearTimeout(idle);
          idle = setTimeout(() => {
            const { missing } = asm.state;
            settle(() =>
              rejectP(
                new Error(
                  missing.length
                    ? `stream stalled — chunk${missing.length > 1 ? "s" : ""} ${missing.join(", ")} never arrived`
                    : `no data from the mixnet for ${Math.round(idleMs / 1000)}s`,
                ),
              ),
            );
          }, idleMs);
        };

        const overall = setTimeout(
          () => settle(() => rejectP(new Error("stream exceeded the overall timeout"))),
          REPLY_TIMEOUT_MS,
        );

        sock.onMessage((m) => {
          const res = parseResponse(m.message);
          if (!res || res.id !== req.id) return; // not ours
          bumpIdle();
          // Real arrival: one frame, this many bytes. Both measured.
          frames += 1;
          if (!firstFrameAt) firstFrameAt = Date.now();
          lastFrameAt = Date.now();
          bytes += Buffer.byteLength(m.message, "utf8");
          p?.event(`${frames} frame${frames > 1 ? "s" : ""} · ${humanBytes(bytes)}`);

          switch (res.kind) {
            case "chat.chunk": {
              const text = asm.chunk(res.seq, res.delta);
              if (text) onDelta(text);
              check();
              break;
            }
            case "chat.end":
              asm.end(res.chunks);
              usage = res.usage;
              if (res.images?.length) images = res.images;
              if (typeof res.balance === "number") balance = res.balance;
              // A hole with the terminator already in hand is fatal, but late
              // frames are legal — the idle timer decides which it was.
              check();
              break;
            case "error":
              // Counter drift surfaces here too. Record the server's number so
              // the caller's retry is signed with a value that will be accepted.
              if (res.reason === "replay" && typeof res.counter === "number") {
                wallet.syncCounter(res.counter);
                settle(() => rejectP(new CounterDrift(res.error)));
                break;
              }
              settle(() => rejectP(new Error(`server: ${res.error}`)));
              break;
            default:
              break;
          }
        });

        bumpIdle();
        sock.sendAnonymous(server, JSON.stringify(req), surbsFor(c));
        p?.phase("sent · waiting for first frame");
      }),
    p,
  );
}

// ---- commands --------------------------------------------------------------

async function cmdSetup(): Promise<void> {
  const c = cfg.load();
  const gateway = flag("gateway");
  // Uniform random by default. --latency probes gateways to pick the fastest,
  // which is nicer when it works but fails outright on hosts where the probe
  // cannot compute a latency — so it stays opt-in.
  console.log(nym.init(c.clientId, { gateway, latencyBased: !gateway && hasFlag("latency"), port: WS_PORT }));
  console.log(`\nconfig: ${cfg.configPath()}`);
  if (!c.serverAddress) console.log("next:  npm run client -- server <nym-address-from-scrai-server>");
}

/**
 * Hold the mixnet client open.
 *
 * This is the better default for privacy, not merely a speed-up. A connected
 * nym-client emits loop cover traffic — roughly five packets a second by
 * default (DEFAULT_LOOP_COVER_STREAM_AVERAGE_DELAY is 200ms) — so real messages
 * travel inside a stream that looks the same whether or not you are saying
 * anything.
 *
 * Connecting only when you have something to send throws that away: every
 * connection is then, by definition, a real request, and its timing IS the
 * signal. A gateway watching a short-lived client learns exactly when you asked
 * something. Watching a long-lived one it learns only that you are online.
 *
 * The daemon deliberately does NOT hold a websocket open — it supervises the
 * process and nothing more. nym-client delivers an inbound message to one
 * websocket consumer, so a daemon sitting on the socket could swallow replies
 * meant for `chat`.
 */
async function cmdDaemon(): Promise<void> {
  const c = cfg.load();
  const existing = nym.findRunning(c.clientId);
  if (existing) fail(`a mixnet client is already running (pid ${existing}).\n  stop it with:  npm run client -- stop`);
  if (!nym.isInitialised(c.clientId)) fail(`nym client "${c.clientId}" is not initialised.\n  run:  npm run client:setup`);

  const prog = new Progress("starting nym-client");
  const running = await nym.start(c.clientId, {
    port: WS_PORT,
    verbose: process.env.SCRAI_VERBOSE === "1",
    onPhase: (phase) => prog.phase(phase),
  });

  // Grab the address, then let go of the socket so requests can use it.
  const sock = await NymSocket.connect(WS_PORT);
  const address = await sock.selfAddress();
  sock.close();
  prog.stop();

  let stopping = false;
  const shutdown = async () => {
    if (stopping) return;
    stopping = true;
    process.stderr.write("\nshutting down (letting nym-client flush)…\n");
    await running.stop();
    process.exit(0);
  };
  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());

  console.log("──────────────────────────────────────────────────────────────");
  console.log("  mixnet client is up and staying up");
  console.log("");
  console.log(`  ${address}`);
  console.log("");
  console.log("  Cover traffic is flowing (~5 packets/s), so your real requests");
  console.log("  are indistinguishable from an idle connection.");
  console.log("");
  console.log("  Run chats from another terminal — they reuse this client:");
  console.log("    npm run client -- chat \"…\"");
  console.log("──────────────────────────────────────────────────────────────");
  console.log("ctrl-c to stop");

  await new Promise<never>(() => {}); // hold until a signal arrives
}

function cmdStop(): void {
  const c = cfg.load();
  const pid = nym.findRunning(c.clientId);
  if (!pid) {
    console.log("no mixnet client is running");
    return;
  }
  // SIGTERM, not SIGKILL: nym-client needs a moment to flush its SURB store or
  // the next start refuses to come up.
  process.kill(pid, "SIGTERM");
  console.log(`stopped mixnet client (pid ${pid})`);
}

async function cmdModels(): Promise<void> {
  const prog = new Progress("starting nym-client");
  const res = (await roundTrip({ v: PROTOCOL_VERSION, kind: "models", id: newId() }, prog)) as {
    models: ModelInfo[];
  };
  const c = cfg.load();

  // Cache it so `model <name>` can validate the name and learn its kind without
  // paying for another mixnet round trip.
  cfg.save({ catalog: res.models });

  console.log("");
  console.log(
    `  ${"MODEL".padEnd(28)} ${"KIND".padEnd(6)} ${"VENDOR".padEnd(12)} ${"PROMPT".padStart(11)} ${"ANSWER".padStart(11)}  PRIVACY`,
  );
  console.log(
    `  ${"".padEnd(28)} ${"".padEnd(6)} ${"".padEnd(12)} ${"SCRAI per 1,000 tokens".padStart(23)}`,
  );
  console.log(`  ${"─".repeat(28)} ${"─".repeat(6)} ${"─".repeat(12)} ${"─".repeat(11)} ${"─".repeat(11)}  ${"─".repeat(15)}`);
  for (const m of res.models) {
    const active = m.model === c.model ? " ←" : "";
    const privacy = m.trainsOnInput ? "trains on input" : "zero-retention";
    const [pin, pout] = perThousand(m.rate);
    console.log(
      `  ${m.model.padEnd(28)} ${m.kind.padEnd(6)} ${m.vendor.padEnd(12)} ${pin.padStart(11)} ${pout.padStart(11)}  ${privacy}${active}`,
    );
  }
  console.log(`\n  choose one:  npm run client -- model <name>\n`);
}

/**
 * Write generated images and report where they landed.
 *
 * On timing: `tookMs` is the whole round trip — nym-client startup, the trip
 * out, the model actually drawing, and the trip back. Dividing bytes by that
 * would produce a "throughput" number dominated by startup and generation, and
 * measurement showed exactly that: 5.6x the payload cost only 15% more wall
 * time. So we report the duration and say what is in it, rather than a rate
 * that would be read as transfer speed and be wrong.
 *
 * A true transfer rate needs a window that contains only transfer, which we
 * have when a reply arrives as several frames — hence `transferMs`.
 */
function saveImages(
  images: import("../types.js").GeneratedImage[],
  tookMs?: number,
  transferMs?: number,
): void {
  const c = cfg.load();
  const dir = resolvePath(process.cwd(), c.imageDir);
  mkdirSync(dir, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
  let total = 0;

  images.forEach((img, i) => {
    const ext = (img.mimeType.split("/")[1] ?? "png").replace("jpeg", "jpg");
    const name = images.length > 1 ? `${stamp}-${i + 1}.${ext}` : `${stamp}.${ext}`;
    const file = join(dir, name);
    const bytes = Buffer.from(img.data, "base64");
    total += bytes.length;
    writeFileSync(file, bytes);
    console.log(`  ▸ ${join(c.imageDir, name)}  (${humanBytes(bytes.length)} ${img.mimeType})`);
  });

  if (tookMs && tookMs > 0) {
    const line = `  ${humanBytes(total)} · ${(tookMs / 1000).toFixed(1)}s end to end (startup + generation + transfer)`;
    // Only claim a rate when the window really was transfer, i.e. between the
    // first and last frame of a multi-frame reply.
    if (transferMs && transferMs > 250) {
      console.log(`${line}\n  transfer alone: ${humanBytes(Math.round(total / (transferMs / 1000)))}/s`);
    } else {
      console.log(line);
    }
  }
}

/**
 * What this turn can cost at most, in whole SCRAI.
 *
 * An exact price is impossible before the fact — nobody knows how long the
 * answer will be — so this is the ceiling: the whole prompt plus a full
 * maxTokens of output, at the rates the server quoted. Real answers are almost
 * always shorter and therefore cheaper.
 *
 * Advisory only. The server prices the exchange from its own table and enforces
 * that; this number exists so the user is not surprised, not to decide anything.
 */
function quote(promptChars: number, c: cfg.Config): number | null {
  const rate = c.modelRate;
  if (!rate) return null;
  const inTokens = Math.ceil(promptChars / 4); // same ~4 chars/token as the server
  const scrai = (inTokens * rate.in + c.maxTokens * rate.out) / 1_000_000;
  return Math.ceil(scrai);
}

/**
 * Attach payment to a chat request.
 *
 * The signature covers the body, so it authorises this exact request. The
 * counter is reserved before sending: if we crash mid-flight we burn a number
 * rather than reusing one, and a reused number is what the server calls a
 * replay.
 */
function signChat(req: Request & { kind: "chat" }): Request {
  const counter = wallet.nextCounter();
  const body = JSON.stringify({
    model: req.model,
    messages: req.messages,
    maxTokens: req.maxTokens ?? null,
  });
  return { ...req, sessionId: wallet.keys().sessionId, counter, sig: wallet.sign(counter, body) };
}

/**
 * Can this turn be afforded? Returns an explanation when it cannot.
 *
 * Checked against the CEILING, not the expected price — committing to a request
 * you might not be able to pay for is how you end up owing money mid-stream.
 * Free models bypass it entirely.
 *
 * This is the client protecting its own user, and it is the SERVER that enforces
 * the balance (see money/store reserve). Checked against AVAILABLE — the funded
 * session plus any held ecash — because held tokens are redeemed into the session
 * right before the request is sent (see ensureFunded).
 */
function affordable(ceiling: number | null): string | null {
  if (ceiling === null || ceiling <= 0) return null;
  const have = wallet.available();
  if (have >= ceiling) return null;
  return (
    `  Not enough SCRAI: ${wallet.format(have)}\n` +
    `  This request costs up to ${wallet.fmt(ceiling)} SCRAI (USD ${wallet.usd(ceiling)}).\n` +
    `  Top up with:  /credit 10        (10 USD = 1,000,000 SCRAI)`
  );
}

/**
 * The billing line under an answer: what it cost, in both units, and what is
 * left. Charging happens here too — the price the SERVER sent is what gets
 * booked, never a number this side computed.
 */
function footer(usage: import("../types.js").TokenUsage, indent = "  ", serverBalance?: number): string {
  const f = usage.billing;
  const notes = [f?.estimated ? "estimated" : null, f?.fallbackPrice ? "unlisted model" : null].filter(Boolean);
  if (!f) return `${indent}—  ·  ${formatTokensCli(usage)}`;

  // A server on an older build sends a frame with different field names, and
  // reading a missing one yields NaN — which formats as "NaN SCRAI" and books
  // nothing, i.e. a silent free ride. Say what happened instead.
  if (typeof f.priceScrai !== "number" || !Number.isFinite(f.priceScrai)) {
    return (
      `${indent}price missing from the server's billing frame — it is probably running an older build\n` +
      `${indent}${formatTokensCli(usage)}`
    );
  }

  // The server settled and told us the balance; we only mirror it.
  const w = wallet.syncBalance(serverBalance ?? wallet.read().balance, f.priceScrai);
  const price =
    f.priceScrai === 0
      ? "free"
      : `${wallet.fmt(f.priceScrai)} SCRAI · USD ${wallet.usd(f.priceScrai)}`;

  return (
    `${indent}${price}  ·  ${formatTokensCli(usage)}` +
    (notes.length ? `  (${notes.join(", ")})` : "") +
    `\n${indent}Balance: ${wallet.format(w.balance)}`
  );
}

async function cmdChat(prompt: string): Promise<void> {
  const c = cfg.load();
  if (!c.model) fail("no model chosen.\n  list them:  npm run client -- models\n  then:       npm run client -- model <name>");

  const messages: ChatMessage[] = [{ role: "user", content: prompt }];
  const isImage = c.modelKind === "image";
  // Image models expose generateContent only — there is nothing to stream, and
  // asking for it would just add frames around a single blob.
  const streaming = !hasFlag("no-stream") && !isImage;

  const max = quote(prompt.length, c);
  const blocked = affordable(max);
  if (blocked) fail(blocked);
  if (max !== null) {
    process.stderr.write(
      max === 0
        ? "  free\n"
        : `  up to ${wallet.fmt(max)} SCRAI (USD ${wallet.usd(max)})\n`,
    );
  }

  // Redeem any held ecash into the session first — its own round trip, kept
  // apart in time from the withdrawal so the two cannot be linked. Also what
  // first brings the session into existence, which even a free model needs.
  await ensureFunded();

  const prog = new Progress("starting nym-client");

  const req: Request = {
    v: PROTOCOL_VERSION,
    kind: "chat",
    id: newId(),
    model: c.model,
    messages,
    maxTokens: c.maxTokens,
    stream: streaming,
  };

  const signed = signChat(req as Request & { kind: "chat" });

  if (!streaming) {
    const res = (await roundTrip(signed, prog)) as {
      text: string;
      images?: import("../types.js").GeneratedImage[];
      usage: import("../types.js").TokenUsage;
      balance?: number;
    };
    const took = prog.elapsedMs;
    prog.stop();
    if (res.text) console.log(`${res.text}\n`);
    if (res.images?.length) saveImages(res.images, took);
    console.log(footer(res.usage, "  ", res.balance));
    return;
  }

  let firstDelta = true;
  const onDelta = (text: string) => {
    // Clear the indicator before the answer starts, so they never interleave.
    if (firstDelta) { prog.stop(); firstDelta = false; }
    process.stdout.write(text);
  };

  let result;
  try {
    result = await streamRoundTrip(signed, onDelta, prog);
  } catch (err) {
    if (!(err instanceof CounterDrift)) throw err;
    // syncCounter already ran; signing again picks up the corrected value.
    result = await streamRoundTrip(signChat(req as Request & { kind: "chat" }), onDelta, prog);
  }
  const { usage, images, transferMs, balance } = result;
  const took = prog.elapsedMs;
  prog.stop();
  console.log(`\n`);
  if (images?.length) saveImages(images, took, transferMs);
  console.log(footer(usage, "  ", balance));
}

// ---- repl ------------------------------------------------------------------
// The session holds ONE mixnet client for its whole lifetime. That is both
// faster and, more importantly, what keeps cover traffic flowing between turns —
// see the note above withMixnet. A client the repl started is stopped on exit;
// one that was already running (a `daemon`) is left alone.

const REPL_HELP = `
  /ask <question>     ask something (or just start typing)
  /model              all models, numbered
  /model text         text models only
  /model img-gen      image models only
  /model <n>          switch to number n from the list you last saw
  /model <name>       switch by name
  /model refresh      re-fetch the list from the server
  /credit <usd>       buy SCRAI — fixed amounts: ${purchaseTiers().map((t) => `$${t}`).join(" ")}  (1 USD = 100,000 SCRAI)
  /redeem             redeem held credit into the session now
  /balance            show balance, held credit and spend
  /gateway            show the active entry gateway
  /clear              forget the conversation history
  /help               this list
  /exit               quit (or ctrl-d)
`;

/** Ask the server for the catalog and cache it. */
async function fetchCatalog(): Promise<ModelInfo[]> {
  const res = (await roundTrip({ v: PROTOCOL_VERSION, kind: "models", id: newId() })) as {
    models: ModelInfo[];
  };
  cfg.save({ catalog: res.models });
  return res.models;
}

/**
 * Input and output rates as two separate cells.
 *
 * They used to share one "33,000 / 275,000" cell, and the slash read as a
 * fraction or a range rather than two independent prices. Two labelled columns
 * cannot be misread.
 */
/**
 * Rates per 1,000 tokens rather than per million.
 *
 * Per-million figures are what providers publish, but they are six digits wide
 * and nobody sends a million tokens. Per-thousand lands in the range of an
 * actual prompt, so the numbers can be compared at a glance.
 */
function perThousand(rate?: { in: number; out: number }): [string, string] {
  if (!rate) return ["—", "—"];
  if (!rate.in && !rate.out) return ["free", "free"];
  const f = (v: number) => (v / 1000 < 1 ? (v / 1000).toFixed(3) : wallet.fmt(Math.round(v / 1000)));
  return [f(rate.in), f(rate.out)];
}

function priceCells(rate?: { in: number; out: number }): [string, string] {
  if (!rate) return ["—", "—"];
  if (!rate.in && !rate.out) return ["free", "free"];
  return [wallet.fmt(rate.in), wallet.fmt(rate.out)];
}

/** " · 33,000 / 275,000 SCRAI per 1M in/out" — or " · free" when both are zero. */
function rateLine(rate?: { in: number; out: number }): string {
  if (!rate) return "";
  if (!rate.in && !rate.out) return " · free";
  const [pin, pout] = perThousand(rate);
  return ` · ${pin} SCRAI per 1k prompt, ${pout} per 1k answer`;
}

function printModels(list: ModelInfo[], current?: string): void {
  console.log("");
  list.forEach((m, i) => {
    const mark = m.model === current ? "▸" : " ";
    const n = String(i + 1).padStart(2);
    const [pin, pout] = perThousand(m.rate);
    console.log(
      `  ${mark} ${n}. ${m.model.padEnd(28)} ${m.kind.padEnd(6)} ${m.vendor.padEnd(12)} ${pin.padStart(9)} ${pout.padStart(9)}`,
    );
  });
  console.log(`\n     SCRAI per 1,000 tokens: prompt, then answer.  /model <number> to switch\n`);
}

async function cmdRepl(): Promise<void> {
  let c = cfg.load();
  if (!c.serverAddress) {
    fail("no server configured.\n  npm run client -- server <nym-address>");
  }
  if (!nym.isInitialised(c.clientId)) {
    fail(`nym client "${c.clientId}" is not initialised.\n  npm run client:setup`);
  }

  // Bring the mixnet client up before the prompt appears, so the first question
  // does not pay for it — and so cover traffic starts flowing immediately.
  let held: nym.RunningClient | null = null;
  const alreadyUp = nym.findRunning(c.clientId);
  if (!alreadyUp) {
    const prog = new Progress("starting mixnet client");
    held = await nym.start(c.clientId, {
      port: WS_PORT,
      verbose: process.env.SCRAI_VERBOSE === "1",
      onPhase: (phase) => prog.phase(phase),
    });
    prog.stop();
  }

  // Pull the catalog — and with it the rates — once, before the prompt appears.
  // Rates only change when the operator edits the server's table, but a session
  // that starts with a stale copy quotes stale prices for its whole life, and
  // the round trip is already paid for by the client we just started.
  let rateNote = "";
  try {
    const status = await sessionStatus();
    if (status) rateNote = ` · ${wallet.fmt(status.balance)} SCRAI`;
    const fresh = await fetchCatalog();
    const chosen = fresh.find((m) => m.model === cfg.load().model);
    if (chosen) cfg.save({ modelKind: chosen.kind, modelRate: chosen.rate });
    rateNote += ` · rates for ${fresh.length} models`;
  } catch {
    rateNote = " · rates unavailable, using cache";
  }

  console.log("──────────────────────────────────────────────────────────────");
  console.log(`  scrai · mixnet client ${alreadyUp ? `already up (pid ${alreadyUp})` : "started"} · cover traffic flowing${rateNote}`);
  console.log(`  Model: ${c.model ?? "(none)"}`);
  const heldAtStart = wallet.heldEcashTotal();
  if (heldAtStart > 0) {
    console.log(`  Held credit: ${wallet.format(heldAtStart)} — redeems on your first message (/redeem to do it now)`);
  }
  console.log("  /help for commands, ctrl-d to quit");
  console.log("──────────────────────────────────────────────────────────────");

  const rl = createInterface({ input: process.stdin, output: process.stdout });
  const history: ChatMessage[] = [];
  let lastList: ModelInfo[] = cfg.load().catalog ?? [];

  const ask = async (question: string): Promise<void> => {
    c = cfg.load();
    if (!c.model) {
      console.log("  no model selected — /model\n");
      return;
    }
    history.push({ role: "user", content: question });
    // Image models expose generateContent only; there is nothing to stream.
    const streaming = c.modelKind !== "image";

    // The ceiling covers the whole thread, not just this line — the history is
    // resent every turn, so a long conversation costs more than a short one.
    const promptChars = history.reduce((n, m) => n + m.content.length, 0);
    const max = quote(promptChars, c);
    const blocked = affordable(max);
    if (blocked) {
      history.pop(); // the turn never happened
      console.log(blocked + "\n");
      return;
    }
    if (max !== null && max > 0) {
      process.stderr.write(`  up to ${wallet.fmt(max)} SCRAI (USD ${wallet.usd(max)})\n`);
    }

    try {
      // Redeem any held ecash into the session first — a separate round trip,
      // kept apart in time from the withdrawal so the two cannot be linked. Also
      // what first brings the session into existence, which even a free model needs.
      await ensureFunded();

      const req: Request = {
        v: PROTOCOL_VERSION,
        kind: "chat",
        id: newId(),
        model: c.model,
        messages: history,
        maxTokens: c.maxTokens,
        stream: streaming,
      };

      const signed = signChat(req as Request & { kind: "chat" });

      if (!streaming) {
        const prog = new Progress("generating");
        const res = (await roundTrip(signed, prog)) as {
          text: string;
          images?: import("../types.js").GeneratedImage[];
          usage: import("../types.js").TokenUsage;
          balance?: number;
        };
        const took = prog.elapsedMs;
        prog.stop();
        history.push({ role: "assistant", content: res.text || "(Bild)" });
        if (res.text) console.log(`\nai  › ${res.text}`);
        console.log("");
        if (res.images?.length) saveImages(res.images, took);
        console.log(footer(res.usage, "      ", res.balance) + "\n");
        return;
      }

      let answer = "";
      let first = true;
      const prog = new Progress("sending");
      const onDelta = (text: string) => {
        if (first) { prog.stop(); process.stdout.write("\nai  › "); first = false; }
        answer += text;
        process.stdout.write(text);
      };
      let streamed;
      try {
        streamed = await streamRoundTrip(signed, onDelta, prog);
      } catch (e) {
        if (!(e instanceof CounterDrift)) throw e;
        streamed = await streamRoundTrip(signChat(req as Request & { kind: "chat" }), onDelta, prog);
      }
      const { usage, images, transferMs, balance } = streamed;
      const took = prog.elapsedMs;
      prog.stop();
      history.push({ role: "assistant", content: answer });
      console.log(`\n`);
      if (images?.length) saveImages(images, took, transferMs);
      console.log(footer(usage, "      ", balance) + "\n");
    } catch (err) {
      history.pop(); // don't poison the thread with a turn that never happened
      console.error(`\n      error: ${err instanceof Error ? err.message : String(err)}\n`);
    }
  };

  // The catalog was fetched at startup, so filters reuse that copy. /model
  // refresh forces a new one for a server that changed mid-session.
  let refreshed = true;

  const handleModel = async (arg: string): Promise<void> => {
    if (!refreshed || arg === "refresh") {
      process.stderr.write("  fetching model list from the server …\r");
      try {
        lastList = await fetchCatalog();
        refreshed = true;
        process.stderr.write("\x1b[2K");
      } catch (err) {
        process.stderr.write("\x1b[2K");
        console.log(`  could not reach the server (${err instanceof Error ? err.message : String(err)}) — using cache\n`);
      }
    }

    const all = cfg.load().catalog ?? lastList;
    const current = cfg.load().model;

    if (!arg || arg === "refresh") { lastList = all; printModels(lastList, current); return; }

    if (arg === "text") { lastList = all.filter((m) => m.kind === "text"); printModels(lastList, current); return; }
    if (arg === "img-gen" || arg === "image" || arg === "img") {
      lastList = all.filter((m) => m.kind === "image");
      printModels(lastList, current);
      return;
    }

    // A bare number refers to the list most recently shown, which is what the
    // user is actually looking at.
    if (/^\d+$/.test(arg)) {
      const idx = Number(arg) - 1;
      const hit = lastList[idx];
      if (!hit) { console.log(`  no entry ${arg} in the list you last saw (1..${lastList.length})\n`); return; }
      cfg.save({ model: hit.model, modelKind: hit.kind, modelRate: hit.rate });
      console.log(`  Model: ${hit.model} (${hit.kind})${rateLine(hit.rate)}\n`);
      return;
    }

    const byName = all.find((m) => m.model === arg);
    if (!byName) { console.log(`  unknown model "${arg}" — /model for the list\n`); return; }
    cfg.save({ model: byName.model, modelKind: byName.kind });
    console.log(`  Model: ${byName.model} (${byName.kind})${rateLine(byName.rate)}\n`);
  };

  try {
    for (;;) {
      const model = cfg.load().model ?? "?";
      const line = (await rl.question(`you (${model}) › `)).trim();
      if (!line) continue;

      if (!line.startsWith("/")) { await ask(line); continue; }

      const [cmd, ...rest] = line.slice(1).split(/\s+/);
      const arg = rest.join(" ").trim();

      switch (cmd) {
        case "ask":
          if (!arg) { console.log("  /ask <frage>\n"); break; }
          await ask(arg);
          break;
        case "model":
        case "models":
          await handleModel(arg);
          break;
        case "gateway":
        case "gateways":
          console.log(nym.listGateways(cfg.load().clientId).split("\n").filter((l) => l.includes("ACTIVE")).join("\n") + "\n");
          break;
        case "account": {
      const sub = rest[0];
      const c = cfg.load();

      if (sub === "new") {
        if (c.mnemonic && !hasFlag("force")) {
          fail(
            `an account already exists (${fingerprint(c.accountId ?? "")}).\n` +
              `  Creating a new one abandons any balance on the old one.\n` +
              `  Show the phrase first:  npm run client -- account show\n` +
              `  Then, if you are sure:  npm run client -- account new --force`,
          );
        }
        const a = createAccount();
        cfg.save({ mnemonic: a.mnemonic, accountId: a.accountId });
        console.log("\n  WRITE THESE 24 WORDS DOWN. They are the only way back to your balance.\n");
        a.mnemonic.split(" ").forEach((w, i) => {
          process.stdout.write(`  ${String(i + 1).padStart(2)}. ${w.padEnd(12)}`);
          if ((i + 1) % 4 === 0) process.stdout.write("\n");
        });
        console.log(`\n  Account: ${fingerprint(a.accountId)}`);
        console.log("  Note the fingerprint too — it tells you a restore worked.\n");
        return;
      }

      if (sub === "restore") {
        const phrase = rest.slice(1).join(" ").trim();
        if (!phrase) fail('usage: account restore "word1 word2 … word24"');
        const a = fromMnemonic(phrase); // throws on a bad checksum
        // Clear any key from this machine: the phrase decides what we hold now.
        cfg.save({
          mnemonic: a.mnemonic,
          accountId: a.accountId,
          sessionPrivateKey: undefined,
          sessionPublicKey: undefined,
          sessionId: undefined,
          sessionIndex: 0,
          counter: 0,
          balance: 0,
        });
        console.log(`restored account ${fingerprint(a.accountId)}`);
        console.log("Compare that fingerprint with the one you noted. If it differs, a word is wrong.\n");

        console.log("  searching for funded sessions …");
        const found = await scanSessions(a.mnemonic, 3, (i, bal) =>
          process.stderr.write(`\r\x1b[2K  session ${i}: ${bal > 0 ? wallet.fmt(bal) + " SCRAI" : "empty"}`),
        );
        process.stderr.write("\r\x1b[2K");

        if (!found.length) {
          console.log("  no funded session found. If you paid but never collected:");
          console.log("    npm run client -- claim");
          return;
        }
        const best = found.reduce((a2, b) => (b.balance > a2.balance ? b : a2));
        cfg.save({ sessionIndex: best.index, sessionId: best.sessionId, balance: best.balance, counter: 0 });
        for (const f of found) {
          console.log(`  session ${f.index}: ${wallet.format(f.balance)}${f.index === best.index ? "  ← now active" : ""}`);
        }
        // The counter is server-side state we no longer have; sessionStatus
        // pulls it back so the first request is not refused as a replay.
        await sessionStatus();
        return;
      }

      if (sub === "show") {
        if (!c.mnemonic) fail("no account yet — npm run client -- account new");
        console.log(fromMnemonic(c.mnemonic).mnemonic);
        return;
      }

      if (!c.mnemonic) {
        console.log("no account yet.\n  create one:  npm run client -- account new");
        return;
      }
      console.log(`account:     ${fingerprint(c.accountId ?? "")}`);
      console.log(`phrase:      ${maskMnemonic(c.mnemonic)}  (full: account show)`);
      console.log(`balance:     ${wallet.format(wallet.read().balance)}`);
      if (wallet.isLegacyKey()) {
        console.log("");
        console.log("  ⚠ This balance sits on a RANDOM session key, created before keys were");
        console.log("    derived from the phrase. Your 24 words will NOT bring it back.");
        console.log(`    Until it is spent, back up ${cfg.configPath()}`);
        console.log("    Anything bought from now on is recoverable from the phrase alone.");
      } else {
        console.log(`session:     #${c.sessionIndex ?? 0}, derived from the phrase — recoverable`);
      }
      return;
    }

    case "credit": {
          const usd = Number(arg);
          if (!purchaseTiers().includes(usd)) {
            console.log(`  /credit <usd> — fixed amounts only: ${purchaseTiers().map((t) => `$${t}`).join(", ")}\n`);
            console.log(`  Everyone buys the same amounts, so a purchase reveals no per-user fingerprint.\n`);
            break;
          }
          try {
            const collected = await topUp(usd);
            console.log(`  +${wallet.fmt(collected)} SCRAI collected — held locally`);
            console.log(`  It funds your session on first use; redeeming later widens your anonymity set.`);
            console.log(`  Available: ${wallet.format(wallet.available())}\n`);
          } catch (err) {
            console.log(`  top-up failed: ${err instanceof Error ? err.message : String(err)}\n`);
          }
          break;
        }
        case "claim": {
      const collected = await claimEntitlement();
      if (collected === null) {
        console.log("  nothing to collect — no entitlement is outstanding\n");
        break;
      }
      console.log(`  collected ${wallet.fmt(collected)} SCRAI — held locally, funds your session on first use`);
      console.log(`  Available: ${wallet.format(wallet.available())}\n`);
      break;
    }

    case "redeem": {
      const held = wallet.heldEcashTotal();
      if (!held) { console.log("  nothing held to redeem\n"); break; }
      try {
        const bal = await redeemHeld();
        console.log(`  redeemed ${wallet.fmt(held)} SCRAI into the session. Balance: ${wallet.format(bal)}\n`);
      } catch (err) {
        console.log(`  redeem failed (tokens are still held): ${err instanceof Error ? err.message : String(err)}\n`);
      }
      break;
    }

    case "balance": {
          const w = wallet.read();
          console.log(`  Balance: ${wallet.format(w.balance)}`);
          const held = wallet.heldEcashTotal();
          if (held > 0) console.log(`  Held:    ${wallet.format(held)}  (redeems on next use)`);
          console.log(`  Spent:   ${wallet.format(w.spent)}\n`);
          break;
        }
        case "clear":
          history.length = 0;
          console.log("  history cleared\n");
          break;
        case "help":
          console.log(REPL_HELP);
          break;
        case "exit":
        case "quit":
          return;
        default:
          console.log(`  unknown command "/${cmd}" — /help\n`);
      }
    }
  } catch {
    // ctrl-d
  } finally {
    rl.close();
    if (held) {
      process.stderr.write("stopping mixnet client …\n");
      await held.stop();
    }
    console.log("\nbye — nothing was stored.");
  }
}

function usage(): void {
  const c = cfg.load();
  console.log(`
scrai-client — anonymous AI over the Nym mixnet

  install-nym              download the nym-client binary into ./bin
  setup [--gateway <id>]   initialise the local nym client
         [--latency]       ...picking the lowest-latency gateway (can fail)
  gateways                 list gateways this client knows
  gateway <id>             switch the entry gateway (no re-init needed)
  address                  show this client's own nym address

  server <nym-address>     point at a scrai-server
  models                   list models the server offers
  model <name>             choose the default model

  chat <prompt…>           one question, streamed answer
       [--no-stream]       ...delivered whole instead
  repl                     interactive session

  account                  show the account, fingerprint and recoverability
          new              create one — prints 24 words to write down
          restore <words>  rebuild from a phrase and find funded sessions
          show             print the phrase

  credit <usd>             buy SCRAI — fixed amounts only (${purchaseTiers().map((t) => `$${t}`).join(" ")}),
                           raises an invoice, waits, collects; held until first use
  claim                    collect SCRAI paid for but not yet withdrawn
  redeem                   redeem held credit into the session now
  balance                  show balance, held credit and spend

  daemon                   hold the mixnet client open (cover traffic)
  stop                     stop the held client

current
  nym client id   ${c.clientId}
  server          ${c.serverAddress ?? "(not set)"}
  model           ${c.model ?? "(not set)"}
  balance         ${wallet.format(wallet.read().balance)}
  reply surbs     ${c.replySurbs}
  mixnet client   ${(() => { const p = nym.findRunning(c.clientId); return p ? `running (pid ${p}) — cover traffic active` : "not running — started per request"; })()}
  config          ${cfg.configPath()}
`);
}

// ---- dispatch --------------------------------------------------------------

async function main(): Promise<void> {
  const [cmd, ...rest] = process.argv.slice(2);

  switch (cmd) {
    case "install-nym":
      if (!platformSupported()) fail(unsupportedPlatformMessage());
      console.log(await install());
      return;

    case "setup":
      return cmdSetup();

    case "gateways": {
      // Default: what this client has registered with. --available: the network.
      if (!hasFlag("available")) {
        console.log(nym.listGateways(cfg.load().clientId));
        console.log("\n  browse the network:  npm run client -- gateways --available [--cc DE]");
        return;
      }

      const all = await listAvailableGateways();
      const cc = flag("cc")?.toUpperCase();
      if (!cc) {
        console.log(`\n  ${all.length} entry gateways, by declared country:\n`);
        const rows = byCountry(all);
        for (let i = 0; i < rows.length; i += 8) {
          console.log("   " + rows.slice(i, i + 8).map(([c, n]) => `${c}:${n}`).join("  "));
        }
        console.log(`\n  list one country:  npm run client -- gateways --available --cc DE\n`);
        return;
      }

      const hits = all.filter((g) => g.country === cc);
      if (!hits.length) fail(`no entry gateway declares country "${cc}"`);
      console.log(`\n  ${hits.length} entry gateways in ${cc}:\n`);
      for (const g of hits.slice(0, 25)) console.log(`   ${g.identity}  ${g.host}`);
      if (hits.length > 25) console.log(`   … and ${hits.length - 25} more`);
      console.log(`\n  switch to one:  npm run client -- gateway <identity>\n`);
      return;
    }

    case "gateway": {
      const gw = rest[0];
      if (!gw) fail("usage: gateway <ed25519-identity>");
      console.log(nym.useGateway(cfg.load().clientId, gw));
      return;
    }

    case "address": {
      const c = cfg.load();
      const running = await nym.start(c.clientId, { port: WS_PORT });
      const sock = await NymSocket.connect(WS_PORT);
      console.log(await sock.selfAddress());
      sock.close();
      await running.stop();
      return;
    }

    case "reset": {
      const c = cfg.load();
      const dir = nym.configDir(c.clientId);
      if (!nym.isInitialised(c.clientId)) fail(`client "${c.clientId}" is not initialised — nothing to reset`);
      if (!hasFlag("yes")) {
        console.log(
          `This deletes ${dir} — the client's identity keys and gateway registration.\n` +
            `Its Nym address changes permanently and cannot be recovered.\n\n` +
            `  confirm with:  npm run client -- reset --yes`,
        );
        return;
      }
      rmSync(dir, { recursive: true, force: true });
      cfg.save({ modelKind: cfg.load().modelKind });
      console.log(`deleted ${dir}\n  now run:  npm run client:setup -- --gateway <identity>`);
      return;
    }

    case "server": {
      const addr = rest[0];
      if (!addr) fail("usage: server <nym-address>");
      cfg.save({ serverAddress: addr });
      console.log(`server set to ${addr}`);
      return;
    }

    case "model": {
      const m = rest[0];
      if (!m) fail("usage: model <name>");
      // Always re-fetch on a model change. The stored rate is what every price
      // estimate is built from, and a stale one silently quotes the wrong
      // number — which is worse than the round trip it costs to be right.
      let known = cfg.load().catalog ?? [];
      let hit = known.find((k) => k.model === m);
      try {
        known = await fetchCatalog();
        hit = known.find((k) => k.model === m);
      } catch {
        /* offline: fall back to whatever the cache knew */
      }
      if (known.length && !hit) {
        fail(`unknown model "${m}".\n  known: ${known.map((k) => k.model).join(", ")}`);
      }
      // Without a cached catalog we cannot know the kind, and guessing it wrong
      // means the wrong SURB budget. Say so rather than silently defaulting.
      cfg.save({ model: m, modelKind: hit?.kind, modelRate: hit?.rate });
      console.log(
        hit
          ? `default model is now ${m} (${hit.kind})`
          : `default model is now ${m} — run \`models\` first so the SURB budget can size itself`,
      );
      return;
    }

    case "daemon":
      return cmdDaemon();

    case "stop":
      return cmdStop();

    case "account": {
      const sub = rest[0];
      const c = cfg.load();

      if (sub === "new") {
        if (c.mnemonic && !hasFlag("force")) {
          fail(
            `an account already exists (${fingerprint(c.accountId ?? "")}).\n` +
              `  Creating a new one abandons any balance on the old one.\n` +
              `  Show the phrase first:  npm run client -- account show\n` +
              `  Then, if you are sure:  npm run client -- account new --force`,
          );
        }
        const a = createAccount();
        cfg.save({ mnemonic: a.mnemonic, accountId: a.accountId });
        console.log("\n  WRITE THESE 24 WORDS DOWN. They are the only way back to your balance.\n");
        a.mnemonic.split(" ").forEach((w, i) => {
          process.stdout.write(`  ${String(i + 1).padStart(2)}. ${w.padEnd(12)}`);
          if ((i + 1) % 4 === 0) process.stdout.write("\n");
        });
        console.log(`\n  Account: ${fingerprint(a.accountId)}`);
        console.log("  Note the fingerprint too — it tells you a restore worked.\n");
        return;
      }

      if (sub === "restore") {
        const phrase = rest.slice(1).join(" ").trim();
        if (!phrase) fail('usage: account restore "word1 word2 … word24"');
        const a = fromMnemonic(phrase); // throws on a bad checksum
        // Clear any key from this machine: the phrase decides what we hold now.
        cfg.save({
          mnemonic: a.mnemonic,
          accountId: a.accountId,
          sessionPrivateKey: undefined,
          sessionPublicKey: undefined,
          sessionId: undefined,
          sessionIndex: 0,
          counter: 0,
          balance: 0,
        });
        console.log(`restored account ${fingerprint(a.accountId)}`);
        console.log("Compare that fingerprint with the one you noted. If it differs, a word is wrong.\n");

        console.log("  searching for funded sessions …");
        const found = await scanSessions(a.mnemonic, 3, (i, bal) =>
          process.stderr.write(`\r\x1b[2K  session ${i}: ${bal > 0 ? wallet.fmt(bal) + " SCRAI" : "empty"}`),
        );
        process.stderr.write("\r\x1b[2K");

        if (!found.length) {
          console.log("  no funded session found. If you paid but never collected:");
          console.log("    npm run client -- claim");
          return;
        }
        const best = found.reduce((a2, b) => (b.balance > a2.balance ? b : a2));
        cfg.save({ sessionIndex: best.index, sessionId: best.sessionId, balance: best.balance, counter: 0 });
        for (const f of found) {
          console.log(`  session ${f.index}: ${wallet.format(f.balance)}${f.index === best.index ? "  ← now active" : ""}`);
        }
        // The counter is server-side state we no longer have; sessionStatus
        // pulls it back so the first request is not refused as a replay.
        await sessionStatus();
        return;
      }

      if (sub === "show") {
        if (!c.mnemonic) fail("no account yet — npm run client -- account new");
        console.log(fromMnemonic(c.mnemonic).mnemonic);
        return;
      }

      if (!c.mnemonic) {
        console.log("no account yet.\n  create one:  npm run client -- account new");
        return;
      }
      console.log(`account:     ${fingerprint(c.accountId ?? "")}`);
      console.log(`phrase:      ${maskMnemonic(c.mnemonic)}  (full: account show)`);
      console.log(`balance:     ${wallet.format(wallet.read().balance)}`);
      if (wallet.isLegacyKey()) {
        console.log("");
        console.log("  ⚠ This balance sits on a RANDOM session key, created before keys were");
        console.log("    derived from the phrase. Your 24 words will NOT bring it back.");
        console.log(`    Until it is spent, back up ${cfg.configPath()}`);
        console.log("    Anything bought from now on is recoverable from the phrase alone.");
      } else {
        console.log(`session:     #${c.sessionIndex ?? 0}, derived from the phrase — recoverable`);
      }
      return;
    }

    case "credit": {
      const usd = Number(rest[0]);
      if (!purchaseTiers().includes(usd)) {
        fail(`credit takes a fixed amount: ${purchaseTiers().map((t) => `$${t}`).join(", ")}   e.g. credit 10`);
      }
      const collected = await topUp(usd);
      console.log(`+${wallet.fmt(collected)} SCRAI collected — held locally.`);
      console.log(`It funds your session on first use; redeeming later widens your anonymity set.`);
      console.log(`Available: ${wallet.format(wallet.available())}`);
      return;
    }

    case "claim": {
      const collected = await claimEntitlement();
      if (collected === null) {
        console.log("nothing to collect — no entitlement is outstanding");
        return;
      }
      console.log(`collected ${wallet.fmt(collected)} SCRAI — held locally, funds your session on first use`);
      console.log(`Available: ${wallet.format(wallet.available())}`);
      return;
    }

    case "redeem": {
      const held = wallet.heldEcashTotal();
      if (!held) { console.log("nothing held to redeem"); return; }
      const bal = await redeemHeld();
      console.log(`redeemed ${wallet.fmt(held)} SCRAI into the session. Balance: ${wallet.format(bal)}`);
      return;
    }

    case "balance": {
      const w = wallet.read();
      console.log(`Balance: ${wallet.format(w.balance)}`);
      const held = wallet.heldEcashTotal();
      if (held > 0) console.log(`Held:    ${wallet.format(held)}  (redeems on next use)`);
      console.log(`Spent:   ${wallet.format(w.spent)}`);
      return;
    }

    case "models":
      return cmdModels();

    case "chat": {
      const prompt = rest.filter((a) => !a.startsWith("--")).join(" ").trim();
      if (!prompt) fail('usage: chat "your question"');
      return cmdChat(prompt);
    }

    case "repl":
      return cmdRepl();

    default:
      usage();
      if (cmd && !hasFlag("help")) process.exitCode = 1;
  }
}

main().catch((err) => fail(err instanceof Error ? err.message : String(err)));
