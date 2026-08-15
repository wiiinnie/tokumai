// ---------------------------------------------------------------------------
// ecash-client.ts — the client half of the blind-ecash dance, as pure helpers.
//
// These are the steps a WALLET performs: blind a packet of secrets, and later
// unblind the issuer's signatures back into spendable proofs (verifying the
// DLEQ on the way). No network, no storage — just the crypto — so both the CLI
// client and the dev UI backend can share one implementation, and the Tauri
// Rust core mirrors the same three calls.
// ---------------------------------------------------------------------------

import { blind, unblind, verifyDleq, decompose, newSecret, toHex, fromHex } from "./blind.js";
import type { BlindedOutput, SignedOutput, Proof, PublicKey } from "./token.js";
import { SCRAI_PER_USD, purchaseTiers } from "../billing.js";

/** Per-output secret state the client keeps between blinding and unblinding. */
export interface OutputState {
  amount: number;
  secret: Uint8Array;
  r: bigint;
  B_: Uint8Array;
}

/** Blind one packet (a tier's worth): a fresh secret per denomination. */
export function blindPacket(amountScrai: number): { outputs: BlindedOutput[]; state: OutputState[] } {
  const state: OutputState[] = decompose(amountScrai).map((denom) => {
    const secret = newSecret();
    const { B_, r } = blind(secret);
    return { amount: denom, secret, r, B_ };
  });
  return { outputs: state.map((s) => ({ amount: s.amount, B_: toHex(s.B_) })), state };
}

/**
 * Unblind the issuer's signatures into spendable proofs.
 *
 * Verifies the DLEQ against the PUBLISHED key first — an issuer that cannot
 * prove it used the public key is the one that might be tagging you, so a failed
 * proof is refused, not trusted.
 */
export function unblindPacket(state: OutputState[], signatures: SignedOutput[], keys: PublicKey[]): Proof[] {
  const pub = new Map(keys.map((k) => [k.amount, k.pubkey]));
  return signatures.map((sig, i) => {
    const st = state[i];
    if (!st || sig.amount !== st.amount) throw new Error("issuer returned mismatched signatures");
    const aHex = pub.get(st.amount);
    if (!aHex) throw new Error(`issuer has no key for denomination ${st.amount}`);
    const A = fromHex(aHex);
    const blindSig = { C_: fromHex(sig.C_), e: fromHex(sig.e), s: fromHex(sig.s) };
    if (!verifyDleq(A, st.B_, blindSig)) {
      throw new Error("the issuer's DLEQ proof failed — refusing the token (possible tagging)");
    }
    return { amount: st.amount, secret: toHex(st.secret), C: toHex(unblind(blindSig.C_, st.r, A)) };
  });
}

/**
 * Split an entitlement into purchase-tier packets (largest first).
 *
 * Every credit is a fixed tier, so the entitlement is a sum of tiers; breaking
 * it back into tier-sized packets means each withdrawal and redemption shows a
 * clean, shared tier rather than an odd total. A non-tier remainder (should not
 * happen once tiers are enforced) is carried as its own packet so no money is
 * stranded.
 */
export function tierPackets(entitlementScrai: number): number[] {
  const tiersScrai = purchaseTiers()
    .map((usd) => usd * SCRAI_PER_USD)
    .sort((a, b) => b - a);
  const out: number[] = [];
  let rem = entitlementScrai;
  for (const t of tiersScrai) while (rem >= t) { out.push(t); rem -= t; }
  if (rem > 0) out.push(rem);
  return out;
}
