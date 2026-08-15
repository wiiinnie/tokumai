// ---------------------------------------------------------------------------
// token.ts — the mint: the issuer's keyset, blind signing, and redemption.
//
// This replaces the old HMAC dev scheme. A funding token is now BEARER ECASH: a
// pair (secret, C) the issuer blind-signed under a per-denomination key, so the
// issuer can confirm a token is genuine without ever having seen the secret it
// signed — the account that bought and the session that spends can no longer be
// tied together. The primitives live in blind.ts; this file is the mint that
// owns the keys and speaks in whole tokens and amounts.
//
// KEY CUSTODY. The entire keyset is derived from one seed:
//
//   a_denomination = H("scrai/mint/v1" ‖ denomination ‖ seed)
//
// so the database never stores a minting secret — a leaked money.db exposes
// balances (bad enough) but not the power to mint. The seed comes from
// SCRAI_ISSUER_SECRET when set; otherwise a random one is generated ONCE and
// persisted, so tokens still survive a restart (the old ephemeral secret lost
// every in-flight token on restart). Set the env var in production to keep the
// minting keys out of the database entirely.
//
// AMOUNTS are carried by WHICH key signed a token, because a blind signature
// hides its message and the issuer therefore cannot police a value embedded in
// it. Each power-of-two denomination has its own key; a token verifies only
// under the key for its stated amount, so a 1-SCRAI token cannot be passed off
// as a million.
// ---------------------------------------------------------------------------

import {
  DENOMINATIONS,
  hashToScalar,
  mintKeypairFromScalar,
  signBlinded,
  verifyToken,
  fromHex,
  toHex,
  type MintKeypair,
  type BlindSignature,
} from "./blind.js";
import { createHash } from "node:crypto";

/** One blinded output the client asks to have signed. */
export interface BlindedOutput {
  amount: number;
  /** Blinded point B_, hex. */
  B_: string;
}

/** One blind signature the issuer returns for an output. */
export interface SignedOutput {
  amount: number;
  /** Signed blinded point C_, hex. */
  C_: string;
  /** DLEQ challenge and response, hex — proof the published key was used. */
  e: string;
  s: string;
}

/** An unblinded token the client later spends. `secret` is the nullifier. */
export interface Proof {
  amount: number;
  /** Token secret x, hex. Burned once spent. */
  secret: string;
  /** Unblinded signature C, hex. */
  C: string;
}

/** One denomination's public key, as published to clients. */
export interface PublicKey {
  amount: number;
  /** Public point A, hex. */
  pubkey: string;
}

const SEED_DOMAIN = Buffer.from("scrai/mint/v1", "utf8");

export class Mint {
  private readonly keys = new Map<number, MintKeypair>();

  constructor(seed: Buffer) {
    for (const denom of DENOMINATIONS) {
      const material = Buffer.concat([SEED_DOMAIN, Buffer.from(u32le(denom)), seed]);
      this.keys.set(denom, mintKeypairFromScalar(hashToScalar(new Uint8Array(material))));
    }
  }

  /** The public half of the keyset, for the client to blind against and pin. */
  publicKeys(): PublicKey[] {
    return [...this.keys.entries()].map(([amount, k]) => ({ amount, pubkey: toHex(k.A) }));
  }

  /**
   * A stable fingerprint of the whole public keyset. A client that pins this can
   * tell if the keyset it is handed ever changes — the first line of defence
   * against a server quietly swapping in per-user keys to tag withdrawals.
   */
  keysetId(): string {
    const h = createHash("sha256");
    for (const { amount, pubkey } of this.publicKeys()) h.update(`${amount}:${pubkey};`);
    return h.digest("hex").slice(0, 16);
  }

  /** Is this a denomination the mint actually issues? */
  knows(amount: number): boolean {
    return this.keys.has(amount);
  }

  /**
   * Blind-sign one output. Throws if the amount is not a real denomination, so
   * a client cannot conjure a key the mint never published.
   */
  sign(output: BlindedOutput): SignedOutput {
    const key = this.keys.get(output.amount);
    if (!key) throw new Error(`no key for denomination ${output.amount}`);
    const sig: BlindSignature = signBlinded(key.a, fromHex(output.B_));
    return { amount: output.amount, C_: toHex(sig.C_), e: toHex(sig.e), s: toHex(sig.s) };
  }

  /**
   * Is this proof genuine? Checks the unblinded signature under the key for its
   * stated denomination. Says nothing about whether it was already spent — that
   * is the nullifier store's job, kept separate so the two never entangle.
   */
  verify(proof: Proof): boolean {
    const key = this.keys.get(proof.amount);
    if (!key) return false;
    try {
      return verifyToken(key.a, fromHex(proof.secret), fromHex(proof.C));
    } catch {
      return false;
    }
  }
}

function u32le(n: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n >>> 0, true);
  return b;
}
