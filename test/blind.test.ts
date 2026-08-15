// ---------------------------------------------------------------------------
// blind.test.ts — the BDHKE ecash core: round-trip, blindness, forgery
// resistance, and the DLEQ anti-tagging proof.
//
// These are the properties the unlinkability rests on, so each is exercised
// against the real curve arithmetic rather than a mock.
// ---------------------------------------------------------------------------

import assert from "node:assert/strict";
import {
  blind,
  signBlinded,
  unblind,
  verifyToken,
  verifyDleq,
  hashToCurve,
  generateMintScalar,
  mintKeypairFromHex,
  mintScalarToHex,
  newSecret,
  decompose,
  DENOMINATIONS,
  toHex,
} from "../src/money/blind.js";

/* ---- full round trip --------------------------------------------------- */

const a = generateMintScalar();
const { A } = mintKeypairFromHex(mintScalarToHex(a));

const secret = newSecret();
const { B_, r } = blind(secret);
const sig = signBlinded(a, B_);

// The client checks the DLEQ against the PUBLISHED A before trusting the sig.
assert.ok(verifyDleq(A, B_, sig), "DLEQ must verify for an honest signer");

const C = unblind(sig.C_, r, A);
assert.ok(verifyToken(a, secret, C), "an honestly minted token must verify");

/* ---- amount / message integrity ---------------------------------------- */

// A different secret than the one signed must not verify against C.
assert.equal(verifyToken(a, newSecret(), C), false, "wrong secret must fail");

// A token minted by a DIFFERENT key must not verify under this key.
const a2 = generateMintScalar();
assert.equal(verifyToken(a2, secret, C), false, "wrong mint key must fail");

// A forged C (an unrelated valid curve point) must not verify.
const forged = hashToCurve(newSecret()).toBytes();
assert.equal(verifyToken(a, secret, forged), false, "a forged signature must fail");

// Garbage bytes must fail closed, not throw.
assert.equal(verifyToken(a, secret, new Uint8Array(33)), false, "garbage C must fail");

/* ---- blindness (the unlinkability property) ---------------------------- */
//
// The SAME secret, blinded twice with fresh randomness, produces two DIFFERENT
// blinded points B_ — so the issuer's view carries no fingerprint of the secret
// — yet both unblind to the SAME token C, because C = a·Y is independent of the
// blinding factor. That gap between "what the issuer signed" and "what gets
// spent" is exactly what it cannot bridge.

const s1 = newSecret();
const b1 = blind(s1);
const b2 = blind(s1);
assert.notEqual(toHex(b1.B_), toHex(b2.B_), "two blindings of one secret must differ (issuer sees no pattern)");

const c1 = unblind(signBlinded(a, b1.B_).C_, b1.r, A);
const c2 = unblind(signBlinded(a, b2.B_).C_, b2.r, A);
assert.equal(toHex(c1), toHex(c2), "both must unblind to the same token regardless of blinding");
assert.ok(verifyToken(a, s1, c1) && verifyToken(a, s1, c2), "both must be valid tokens");

/* ---- DLEQ catches a tagging issuer ------------------------------------- */
//
// A malicious issuer signs with its real key a, but the client was handed a
// DIFFERENT public A' (a per-user key used to tag). The DLEQ must reject it,
// because the proof cannot tie a·B_ to a key the issuer did not actually use.

const { A: Aother } = mintKeypairFromHex(mintScalarToHex(a2));
assert.equal(verifyDleq(Aother, B_, sig), false, "DLEQ must reject a mismatched public key (tagging)");

// A tampered DLEQ (flipped response) must also fail.
const tampered = { ...sig, s: new Uint8Array(sig.s) };
tampered.s[0] ^= 0x01;
assert.equal(verifyDleq(A, B_, tampered), false, "a tampered DLEQ must fail");

// And a C_ that is not a·B_ must fail the DLEQ even with a matching A.
const wrongC = { ...sig, C_: signBlinded(a2, B_).C_ };
assert.equal(verifyDleq(A, B_, wrongC), false, "DLEQ must reject a C_ signed by another key");

/* ---- persistence round trip -------------------------------------------- */

const restored = mintKeypairFromHex(mintScalarToHex(a));
assert.equal(toHex(restored.A), toHex(A), "a key restored from hex must reproduce the same public point");
// And a full round trip under the restored key still verifies.
const rs = newSecret();
const rb = blind(rs);
assert.ok(
  verifyToken(restored.a, rs, unblind(signBlinded(restored.a, rb.B_).C_, rb.r, restored.A)),
  "a token minted under the restored key must verify",
);

/* ---- denominations ----------------------------------------------------- */

assert.deepEqual(decompose(1), [1]);
assert.deepEqual(decompose(1000), [512, 256, 128, 64, 32, 8]);
assert.equal(
  decompose(1000).reduce((n, d) => n + d, 0),
  1000,
  "denominations must sum to the amount",
);
for (const amt of [1, 2, 3, 7, 255, 100_000, 99_999, 1_000_000]) {
  const parts = decompose(amt);
  assert.equal(parts.reduce((n, d) => n + d, 0), amt, `decompose(${amt}) must sum back`);
  assert.ok(parts.every((d) => DENOMINATIONS.includes(d)), `all parts of ${amt} are real denominations`);
}
assert.throws(() => decompose(0), "zero has no decomposition");
assert.throws(() => decompose(-5), "negatives are rejected");
assert.throws(() => decompose(1.5), "non-integers are rejected");

console.log("all blind-signature checks passed (round-trip, blindness, forgery, DLEQ, denominations)");
