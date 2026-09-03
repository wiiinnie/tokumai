// ---------------------------------------------------------------------------
// blind.ts — the cryptographic core of the ecash: blind signatures over
// secp256k1 (the BDHKE scheme, as used by Cashu), plus a DLEQ proof.
//
// THIS IS THE FILE THAT DELIVERS UNLINKABILITY. The old token.ts used an HMAC
// the issuer both minted and verified, so it saw every serial and could chain
// account -> serial -> session. Here the issuer signs a value it CANNOT read:
//
//   1. the client picks a secret x (this becomes the nullifier/serial),
//      maps it to a curve point Y = H2C(x), and BLINDS it: B_ = Y + r·G
//   2. the issuer signs the blinded point:                C_ = a·B_
//      and proves, with a DLEQ, that it used the key a behind its PUBLISHED A
//   3. the client UNBLINDS:                               C  = C_ − r·A = a·Y
//
// The issuer only ever saw B_, which is Y masked by a fresh random r, so the
// (x, C) the client later spends is unlinkable to the withdrawal it came from.
//
// WHY THE DLEQ MATTERS: without it, a malicious issuer could sign each
// withdrawal with a DIFFERENT secret key and later recognise which key verified
// a spend — tagging the user despite the blinding. The DLEQ lets the client
// check that the issuer signed with the key matching the public A everyone was
// given, so it cannot single anyone out. Unlinkability that rests on trusting
// the operator is not unlinkability; this is what makes it hold anyway.
//
// The AMOUNT is bound by using a SEPARATE key per denomination (see token.ts /
// the keyset): a blind signature hides its message, so the issuer cannot see —
// and therefore cannot limit — a value embedded in it. Which key signed a token
// is what proves its worth, so a client cannot inflate a 1-SCRAI token into a
// million. Amounts are therefore split into power-of-two denominations.
//
// Everything here is pure and deterministic given its randomness inputs, so it
// is testable in isolation — see test/blind.test.ts.
// ---------------------------------------------------------------------------

import { secp256k1 } from "@noble/curves/secp256k1.js";
import { createHash, randomBytes } from "node:crypto";

const Point = secp256k1.Point;
type Pt = InstanceType<typeof Point>;

/** Generator, and the order of the scalar field. */
const G = Point.BASE;
const N = Point.Fn.ORDER;

// ---- small helpers --------------------------------------------------------

const sha256 = (b: Uint8Array): Uint8Array => new Uint8Array(createHash("sha256").update(b).digest());

function concat(...arrs: Uint8Array[]): Uint8Array {
  const total = arrs.reduce((n, a) => n + a.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const a of arrs) {
    out.set(a, off);
    off += a.length;
  }
  return out;
}

export const toHex = (b: Uint8Array): string => Buffer.from(b).toString("hex");
export const fromHex = (h: string): Uint8Array => new Uint8Array(Buffer.from(h, "hex"));

function bytesToScalar(b: Uint8Array): bigint {
  return BigInt("0x" + (Buffer.from(b).toString("hex") || "0"));
}

/** A scalar as 32 big-endian bytes — the on-the-wire form for e and s. */
function scalarToBytes(s: bigint): Uint8Array {
  return fromHex((s % N).toString(16).padStart(64, "0"));
}

/** A uniform nonzero scalar in [1, N−1]. */
function randScalar(): bigint {
  for (;;) {
    const s = bytesToScalar(new Uint8Array(randomBytes(32))) % N;
    if (s !== 0n) return s;
  }
}

/** A fresh 32-byte token secret. Becomes the nullifier once spent. */
export function newSecret(): Uint8Array {
  return new Uint8Array(randomBytes(32));
}

// ---- hash-to-curve --------------------------------------------------------
//
// Map an arbitrary secret to a curve point, the way Cashu's NUT-00 does it: try
// compressed points 0x02‖SHA256(msgHash‖counter) with an incrementing counter
// until one lands on the curve. Domain-separated so this hash cannot collide
// with any other use of SHA256 in the system.

const H2C_DOMAIN = Buffer.from("Secp256k1_HashToCurve_Cashu_", "utf8");

export function hashToCurve(secret: Uint8Array): Pt {
  const msgHash = sha256(concat(H2C_DOMAIN, secret));
  for (let counter = 0; counter < 0x10000; counter++) {
    const ctr = new Uint8Array(4);
    new DataView(ctr.buffer).setUint32(0, counter, true); // little-endian, per NUT-00
    const candidate = concat(Uint8Array.of(0x02), sha256(concat(msgHash, ctr)));
    try {
      return Point.fromBytes(candidate);
    } catch {
      // x-coordinate not on the curve — try the next counter
    }
  }
  // Astronomically unlikely: ~half of all 32-byte strings are valid x-coords.
  throw new Error("hashToCurve: no valid point in 65536 tries");
}

// ---- mint (issuer) keys ---------------------------------------------------

export interface MintKeypair {
  /** Private signing scalar. Never leaves the issuer. */
  a: bigint;
  /** Public point A = a·G, compressed. Published to clients. */
  A: Uint8Array;
}

export function generateMintScalar(): bigint {
  return randScalar();
}

/** Build the keypair from a private scalar. */
export function mintKeypairFromScalar(a: bigint): MintKeypair {
  const aa = ((a % N) + N) % N;
  if (aa === 0n) throw new Error("mint scalar must be nonzero");
  return { a: aa, A: G.multiply(aa).toBytes() };
}

/** Build the keypair from a stored private scalar (hex). */
export function mintKeypairFromHex(aHex: string): MintKeypair {
  return mintKeypairFromScalar(BigInt("0x" + aHex));
}

/**
 * Derive a scalar deterministically from key material — used to grow the whole
 * denomination keyset from one seed, so the keys survive a restart without the
 * database ever holding a minting secret. Retries past the vanishing chance of
 * landing on zero.
 */
export function hashToScalar(material: Uint8Array): bigint {
  for (let ctr = 0; ; ctr++) {
    const s = bytesToScalar(sha256(concat(material, Uint8Array.of(ctr & 0xff)))) % N;
    if (s !== 0n) return s;
  }
}

export function mintScalarToHex(a: bigint): string {
  return (a % N).toString(16).padStart(64, "0");
}

// ---- client: blinding -----------------------------------------------------

export interface Blinded {
  /** Blinded point B_ = Y + r·G, sent to the issuer. */
  B_: Uint8Array;
  /** Blinding factor. The client keeps it to unblind; nobody else ever sees it. */
  r: bigint;
}

export function blind(secret: Uint8Array): Blinded {
  const Y = hashToCurve(secret);
  const r = randScalar();
  return { B_: Y.add(G.multiply(r)).toBytes(), r };
}

// ---- issuer: blind signing (+ DLEQ) ---------------------------------------

export interface BlindSignature {
  /** Signed blinded point C_ = a·B_, compressed. */
  C_: Uint8Array;
  /** DLEQ challenge, 32 bytes. */
  e: Uint8Array;
  /** DLEQ response, 32 bytes. */
  s: Uint8Array;
}

/**
 * The DLEQ Fiat–Shamir challenge, hashing the two commitments and the two
 * public points. Sign and verify MUST hash the same bytes in the same order.
 */
function dleqChallenge(R1: Pt, R2: Pt, A: Pt, C_: Pt): bigint {
  return bytesToScalar(sha256(concat(R1.toBytes(), R2.toBytes(), A.toBytes(), C_.toBytes()))) % N;
}

/**
 * Sign a blinded point AND prove the key used matches the public A.
 *
 * The proof is a Schnorr equality-of-discrete-logs: it shows that the same a
 * satisfies A = a·G and C_ = a·B_, without revealing a. A client that verifies
 * it knows the issuer did not slip in a per-user key to tag the withdrawal.
 */
export function signBlinded(a: bigint, B_bytes: Uint8Array): BlindSignature {
  const B_ = Point.fromBytes(B_bytes);
  const C_ = B_.multiply(a);
  const A = G.multiply(a);

  const k = randScalar(); // proof nonce
  const R1 = G.multiply(k);
  const R2 = B_.multiply(k);
  const e = dleqChallenge(R1, R2, A, C_);
  const s = (k + e * a) % N;

  return { C_: C_.toBytes(), e: scalarToBytes(e), s: scalarToBytes(s) };
}

/**
 * Verify the DLEQ against the PUBLISHED A and the client's own B_.
 *
 * Recomputes the commitments from (s, e): R1 = s·G − e·A, R2 = s·B_ − e·C_, and
 * checks the challenge hashes back to e. True means the signer used the key
 * behind A — the anti-tagging guarantee. Returns false on any malformed input.
 */
export function verifyDleq(A_bytes: Uint8Array, B_bytes: Uint8Array, sig: BlindSignature): boolean {
  try {
    const A = Point.fromBytes(A_bytes);
    const B_ = Point.fromBytes(B_bytes);
    const C_ = Point.fromBytes(sig.C_);
    const e = bytesToScalar(sig.e) % N;
    const s = bytesToScalar(sig.s) % N;
    if (e === 0n || s === 0n) return false;

    const R1 = G.multiply(s).subtract(A.multiply(e));
    const R2 = B_.multiply(s).subtract(C_.multiply(e));
    return dleqChallenge(R1, R2, A, C_) === e;
  } catch {
    return false;
  }
}

// ---- client: unblinding ---------------------------------------------------

/**
 * Recover the unblinded signature C = C_ − r·A = a·Y.
 *
 * The blinding factor r cancels the r·A term the issuer unknowingly added, so
 * what remains is a·Y — a valid signature on Y, hence on the secret x, that the
 * issuer never saw in the clear.
 */
export function unblind(C_bytes: Uint8Array, r: bigint, A_bytes: Uint8Array): Uint8Array {
  const C_ = Point.fromBytes(C_bytes);
  const A = Point.fromBytes(A_bytes);
  return C_.subtract(A.multiply(r)).toBytes();
}

// ---- issuer: redemption check ---------------------------------------------

/**
 * Is (secret, C) a genuine token under key a? The issuer recomputes Y = H2C(x)
 * and checks a·Y == C. It never saw x during signing, so a valid check here is
 * proof of a real withdrawal without a link back to which one.
 */
export function verifyToken(a: bigint, secret: Uint8Array, C_bytes: Uint8Array): boolean {
  try {
    const C = Point.fromBytes(C_bytes);
    return hashToCurve(secret).multiply(a).equals(C);
  } catch {
    return false;
  }
}

// ---- denominations --------------------------------------------------------
//
// A blind signature hides its message, so value is carried by WHICH key signed
// the token. Amounts are therefore split into power-of-two denominations, each
// with its own key. 2^30 ≈ 1.07e9 TOKU (~USD 10.7k) is well above any single
// balance, so a normal withdrawal is a handful of tokens (the binary digits of
// the amount); only absurd amounts ever repeat the top denomination.

export const DENOMINATIONS: number[] = Array.from({ length: 31 }, (_, i) => 2 ** i);

/** Split a positive integer amount into denominations that sum to it. */
export function decompose(amount: number): number[] {
  if (!Number.isInteger(amount) || amount <= 0) {
    throw new Error("amount must be a positive whole number of TOKU");
  }
  const out: number[] = [];
  let rem = amount;
  for (let i = DENOMINATIONS.length - 1; i >= 0; i--) {
    const d = DENOMINATIONS[i]!;
    while (rem >= d) {
      out.push(d);
      rem -= d;
    }
  }
  // DENOMINATIONS includes 1, so the remainder always reaches exactly zero.
  if (rem !== 0) throw new Error(`could not decompose ${amount}`);
  return out;
}
