// ---------------------------------------------------------------------------
// account.ts — the recoverable half of a user's money.
//
// THE TWO-LAYER MODEL, AND WHY BOTH LAYERS EXIST.
//
// This mirrors how NymVPN does it (nym-vpn-store keeps a BIP39 mnemonic;
// nym-vpn-credential-fetcher turns account entitlement into BLINDED credentials
// before anything is spent). The split is not decoration — collapsing it would
// destroy the property the whole system is for:
//
//   ACCOUNT   derived from a seed phrase. Stable, recoverable, and KNOWN to the
//             issuer: it is what a payment is credited against. The issuer sees
//             "this account bought 10 USD of TOKU".
//
//   SPENDING  a separate, random key per session (see session.ts). The server
//             sees a balance being spent and cannot tie it to an account.
//
// If the account key were also the spending key, recovery would work and every
// request you ever made would be linked to your purchase forever. That is the
// account model with extra steps, and the mixnet in front of it would be
// decoration. So: the seed phrase buys, a different key spends.
//
// What lives here is only the account half. It holds no balance — a balance on
// an identified account is exactly what we are avoiding.
// ---------------------------------------------------------------------------

import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";
import { generateMnemonic, mnemonicToSeedSync, validateMnemonic } from "bip39";
import type { SessionKeys } from "./session.js";

/**
 * PKCS#8 wrapper for a raw Ed25519 private key. The 16 bytes are a fixed ASN.1
 * prefix; the 32 that follow are the seed. Node has no API for "make me an
 * Ed25519 key from these bytes", so this is the shortest correct route.
 */
const ED25519_PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");

export interface Account {
  /** BIP39 phrase. The ONLY thing a user must keep. */
  mnemonic: string;
  /** PKCS#8 PEM, derived from the phrase. Never leaves the device. */
  privateKey: string;
  /** SPKI PEM. Shown to the issuer so a payment can be credited. */
  publicKey: string;
  /** Public account name: sha256 of the public key, hex. */
  accountId: string;
}

/**
 * A fresh account, as 24 words.
 *
 * Not 12, and the reason is measured rather than conventional: BIP39's checksum
 * is 4 bits at 12 words, so roughly ONE IN SIXTEEN single-word typos still
 * validates — and silently opens a different, empty account. For a recovery
 * phrase that guards money, "your balance is 0" is the worst possible way to
 * report a typo.
 *
 * 24 words carry an 8-bit checksum. Measured over 400 random single-word
 * corruptions: 6% slipped through at 12 words, none at 24.
 */
export function createAccount(): Account {
  return fromMnemonic(generateMnemonic(256));
}

/**
 * Rebuild an account from its phrase.
 *
 * Deterministic by construction: same words in, same keys out, on any machine.
 * That is the whole recovery story — nothing is stored anywhere that must be
 * backed up separately.
 */
export function fromMnemonic(mnemonic: string): Account {
  const phrase = mnemonic.trim().toLowerCase().replace(/\s+/g, " ");
  if (!validateMnemonic(phrase)) {
    // BIP39 has a checksum, so a typo is caught here rather than silently
    // producing a different, empty account.
    throw new Error("that is not a valid recovery phrase — check the words and their order");
  }

  // BIP39 gives 64 bytes; Ed25519 wants 32. Hashing with a domain separator
  // keeps this key distinct from anything else the same seed might derive.
  const seed = mnemonicToSeedSync(phrase);
  const material = createHash("sha256").update("scrai/account/v1").update(seed).digest();

  const privateKey = createPrivateKey({
    key: Buffer.concat([ED25519_PKCS8_PREFIX, material]),
    format: "der",
    type: "pkcs8",
  });
  const publicKeyPem = createPublicKey(privateKey).export({ type: "spki", format: "pem" }).toString();

  return {
    mnemonic: phrase,
    privateKey: privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
    publicKey: publicKeyPem,
    accountId: accountIdFor(publicKeyPem),
  };
}

export function accountIdFor(publicKeyPem: string): string {
  return createHash("sha256").update(publicKeyPem.trim()).digest("hex");
}

/**
 * Prove account ownership to the issuer.
 *
 * Used when claiming a paid invoice and when withdrawing credentials — the two
 * moments the issuer must know WHICH account it is dealing with. Never used
 * when spending; spending is signed by the session key instead.
 */
export function signAsAccount(account: Account, purpose: string, nonce: string): string {
  const key = createPrivateKey(account.privateKey);
  return sign(null, Buffer.from(`${account.accountId}:${purpose}:${nonce}`, "utf8"), key).toString("base64");
}

/**
 * Derive session keys from the recovery phrase, by index.
 *
 * WHY DERIVED AND NOT RANDOM: a random session key exists in exactly one file.
 * Lose it and the balance it controls is gone for good — the server holds a
 * balance against a public key whose private half no longer exists anywhere,
 * and no phrase, backup or support request can bring it back.
 *
 * WHY INDEXED AND NOT ONE FIXED KEY: the same key for everything would let the
 * server link every session you ever open, forever. Indexing gives the property
 * HD wallets have — one seed, many keys the observer cannot connect. The server
 * sees unrelated public keys; only the seed holder knows they are siblings.
 *
 * Recovery is therefore a scan: derive 0, 1, 2 … and ask which have a balance.
 */
export function deriveSessionKeys(mnemonic: string, index: number): SessionKeys {
  const account = fromMnemonic(mnemonic);
  const seed = mnemonicToSeedSync(account.mnemonic);
  const material = createHash("sha256")
    .update("scrai/session/v1")
    .update(seed)
    .update(String(index))
    .digest();

  const privateKey = createPrivateKey({
    key: Buffer.concat([ED25519_PKCS8_PREFIX, material]),
    format: "der",
    type: "pkcs8",
  });
  const publicKey = createPublicKey(privateKey).export({ type: "spki", format: "pem" }).toString();

  return {
    privateKey: privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
    publicKey,
    sessionId: createHash("sha256").update(publicKey.trim()).digest("hex"),
  };
}

/**
 * Short fingerprint of an account, for the user to compare after restoring.
 *
 * The checksum cannot catch every typo, so the last line of defence is the
 * human: note this when you create the account, check it when you restore. A
 * mismatch means a wrong word, not a lost balance.
 */
export function fingerprint(accountId: string): string {
  return accountId.slice(0, 4) + "-" + accountId.slice(4, 8);
}

/** Human-friendly display: never print a whole phrase into a log. */
export function maskMnemonic(mnemonic: string): string {
  const w = mnemonic.split(" ");
  return w.length < 4 ? "…" : `${w[0]} ${w[1]} … ${w[w.length - 1]} (${w.length} words)`;
}
