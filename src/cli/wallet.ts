// ---------------------------------------------------------------------------
// wallet.ts — the client half of the payment layer.
//
// WHAT CHANGED, AND WHY IT MATTERS: this used to be an integer in a JSON file
// that the client decremented itself. Editing the file was free money, and the
// server checked nothing. Now the balance lives on the SERVER, and what the
// client holds is the ed25519 private key that authorises spending from it.
//
// So the numbers here are a MIRROR, not a ledger. `balance` is whatever the
// server last reported. Editing it changes what the terminal prints and nothing
// else — every request is signed, and the server settles against its own record.
//
// The counter is the one piece of local state that must be treated carefully:
// it has to strictly increase, because the server refuses anything it has
// already seen. Losing it (deleting the config) means the server's counter is
// ahead, and the session can no longer be spent from — which is why a top-up
// re-opens rather than resets.
// ---------------------------------------------------------------------------

import { SCRAI_PER_USD } from "../billing.js";
import { generateSessionKeys, signRequest, type SessionKeys } from "../money/session.js";
import { deriveSessionKeys } from "../money/account.js";
import type { Proof } from "../protocol.js";
import * as cfg from "./config.js";

export interface WalletView {
  balance: number;
  spent: number;
  sessionId?: string;
}

/**
 * The session key pair that controls the balance.
 *
 * Derived from the recovery phrase at `sessionIndex`, so the phrase alone can
 * rebuild it on any machine. Indexed rather than fixed, so two sessions cannot
 * be linked by anyone without the seed.
 *
 * A key stored before this existed is kept and used as-is. Orphaning it would
 * strand whatever balance it holds — but it CANNOT be recovered from the
 * phrase, which is why `isLegacyKey()` exists to say so.
 */
export function keys(): SessionKeys {
  const c = cfg.load();

  if (c.sessionPrivateKey && c.sessionPublicKey && c.sessionId) {
    return { privateKey: c.sessionPrivateKey, publicKey: c.sessionPublicKey, sessionId: c.sessionId };
  }

  if (c.mnemonic) {
    const index = c.sessionIndex ?? 0;
    const k = deriveSessionKeys(c.mnemonic, index);
    cfg.save({ sessionIndex: index, sessionId: k.sessionId });
    return k;
  }

  // No account at all: fall back to a random key so the CLI still works, but
  // this balance is not recoverable and the caller is told.
  const k = generateSessionKeys();
  cfg.save({ sessionPrivateKey: k.privateKey, sessionPublicKey: k.publicKey, sessionId: k.sessionId, counter: 0 });
  return k;
}

/** True when the balance is held by a key the recovery phrase cannot rebuild. */
export function isLegacyKey(): boolean {
  const c = cfg.load();
  return Boolean(c.sessionPrivateKey);
}

/** Move to the next derived session — a fresh, unlinkable identity. */
export function nextSession(): SessionKeys {
  const c = cfg.load();
  if (!c.mnemonic) throw new Error("no account — npm run client -- account new");
  const index = (c.sessionIndex ?? 0) + 1;
  const k = deriveSessionKeys(c.mnemonic, index);
  cfg.save({
    sessionIndex: index,
    sessionId: k.sessionId,
    counter: 0,
    balance: 0,
    sessionPrivateKey: undefined,
    sessionPublicKey: undefined,
  });
  return k;
}

export function read(): WalletView {
  const c = cfg.load();
  return { balance: c.balance ?? 0, spent: c.spent ?? 0, sessionId: c.sessionId };
}

/** Next counter value. Reserved before sending, so a crash burns one number rather than reusing one. */
export function nextCounter(): number {
  const n = (cfg.load().counter ?? 0) + 1;
  cfg.save({ counter: n });
  return n;
}

/**
 * Adopt the server's counter.
 *
 * A client whose counter falls behind — config restored from a backup, the same
 * session key used from a second machine, a wiped config — would otherwise be
 * refused forever, because every number it offers has already been seen. The
 * server hands back where it is, and we move ahead of it.
 *
 * Only ever moves forward: taking a LOWER value from the server would hand an
 * attacker a way to rewind the counter and replay old requests.
 */
export function syncCounter(serverCounter: number): number {
  const local = cfg.load().counter ?? 0;
  const next = Math.max(local, serverCounter);
  if (next !== local) cfg.save({ counter: next });
  return next;
}

/** Signature proving key ownership for a read-only status query. No counter involved. */
export function signStatus(): string {
  return signRequest(keys(), 0, "status");
}

export function sign(counter: number, body: string): string {
  return signRequest(keys(), counter, body);
}

/** Record what the server said. The server is authoritative; this only displays. */
export function syncBalance(balance: number, spentNow = 0): WalletView {
  const c = cfg.load();
  const next = { balance, spent: (c.spent ?? 0) + Math.max(0, spentNow) };
  cfg.save(next);
  return { ...next, sessionId: c.sessionId };
}

export function reset(): void {
  cfg.save({ balance: 0, spent: 0, counter: 0 });
}

// ---- held ecash -----------------------------------------------------------
// Bearer tokens withdrawn but not yet redeemed into a session, grouped into
// PACKETS (one per purchase tier). Kept apart from the session balance on
// purpose: redeeming them LATER, one packet at a time, is what decouples the
// account-signed withdrawal from the anonymous spend AND keeps every
// redemption's denomination set a clean, shared tier.

export function heldPackets(): Proof[][] {
  return cfg.load().ecash ?? [];
}

export function heldEcashTotal(): number {
  return heldPackets().reduce((n, pkt) => n + pkt.reduce((m, p) => m + p.amount, 0), 0);
}

/** Append one freshly withdrawn packet, persisted immediately so a crash cannot lose it. */
export function storePacket(proofs: Proof[]): void {
  cfg.save({ ecash: [...(cfg.load().ecash ?? []), proofs] });
}

/** Overwrite the held packets — used to record redemption progress as it happens. */
export function setPackets(packets: Proof[][]): void {
  cfg.save({ ecash: packets });
}

export function clearEcash(): void {
  cfg.save({ ecash: [] });
}

/** What the user can actually spend: the funded session plus anything held. */
export function available(): number {
  return read().balance + heldEcashTotal();
}

export function scraiFor(usd: number): number {
  return Math.floor(usd * SCRAI_PER_USD);
}

/** "1,000,000 SCRAI (USD 10.00)" */
export function format(scrai: number): string {
  return `${fmt(scrai)} SCRAI (USD ${usd(scrai)})`;
}

/**
 * USD value of a SCRAI amount, at enough precision to stay honest.
 *
 * Two decimals is wrong in both directions: a single prompt costs a fraction of
 * a cent and would read as "0.00", and a balance of 999 916 after a 10 USD
 * purchase would read as "10.00", hiding the spend. So: cents when the amount is
 * round, full precision whenever the sub-cent digits carry information.
 */
export function usd(scrai: number): string {
  const v = scrai / SCRAI_PER_USD;
  if (v === 0) return "0.00";
  const cents = v.toFixed(2);
  return Number(cents) === v ? cents : v.toFixed(5).replace(/0+$/, "").replace(/\.$/, ".0");
}

export function fmt(n: number): string {
  return new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(n);
}
