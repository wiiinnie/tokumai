// ---------------------------------------------------------------------------
// session.ts — proving you own a balance without handing over the thing that
// controls it.
//
// The naive design is to send the session id with every request and let the
// server debit it. That makes the id a bearer token travelling in the clear on
// every message: whoever reads it — from a log, a bug, a compromised server —
// can drain the balance. Mixnet payloads are end-to-end encrypted, so the wire
// is not the exposure; relying on that for AUTHORISATION is still the mistake.
//
// So the session is bound to an ed25519 key pair the client generates:
//
//   sessionId = sha256(public key)     public, and useless on its own
//   every request carries a signature over (sessionId ‖ counter ‖ body hash)
//
// Three properties fall out:
//   - knowing the id does not let you spend; the private key never leaves the client
//   - a compromised SERVER cannot spend user balances either — it only ever
//     stores public keys
//   - the counter must strictly increase, so a captured request cannot be replayed
//
// The counter also serialises a session's requests, which is why the store can
// reserve and settle without a lock.
// ---------------------------------------------------------------------------

import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";

export interface SessionKeys {
  /** PKCS#8 PEM. Never leaves the client. */
  privateKey: string;
  /** SPKI PEM. Sent once, when the session is opened. */
  publicKey: string;
  /** sha256 of the public key, hex. The session's public name. */
  sessionId: string;
}

export function generateSessionKeys(): SessionKeys {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const pub = publicKey.export({ type: "spki", format: "pem" }).toString();
  return {
    privateKey: privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
    publicKey: pub,
    sessionId: sessionIdFor(pub),
  };
}

export function sessionIdFor(publicKeyPem: string): string {
  return createHash("sha256").update(publicKeyPem.trim()).digest("hex");
}

/**
 * The exact bytes both sides sign over.
 *
 * The body hash is in here on purpose: without it a signature would authorise
 * "some request with this counter", and an attacker who caught one could swap
 * the prompt for a longer, costlier one. Binding the signature to the body means
 * a captured signature is only good for the exact request it came with.
 */
function signedBytes(sessionId: string, counter: number, body: string): Buffer {
  const bodyHash = createHash("sha256").update(body).digest("hex");
  return Buffer.from(`${sessionId}:${counter}:${bodyHash}`, "utf8");
}

export function signRequest(keys: SessionKeys, counter: number, body: string): string {
  const key = createPrivateKey(keys.privateKey);
  return sign(null, signedBytes(keys.sessionId, counter, body), key).toString("base64");
}

export function verifyRequest(
  publicKeyPem: string,
  sessionId: string,
  counter: number,
  body: string,
  signature: string,
): boolean {
  try {
    const key = createPublicKey(publicKeyPem);
    return verify(null, signedBytes(sessionId, counter, body), key, Buffer.from(signature, "base64"));
  } catch {
    return false; // malformed key or signature is simply a failed check
  }
}

/**
 * Does this public key really own this session id?
 *
 * The id is the hash of the key, so an impostor would have to find a second key
 * hashing to the same value. Cheap to check and it closes the door on a client
 * claiming someone else's session by sending its own key.
 */
export function keyMatchesSession(publicKeyPem: string, sessionId: string): boolean {
  return sessionIdFor(publicKeyPem) === sessionId;
}
