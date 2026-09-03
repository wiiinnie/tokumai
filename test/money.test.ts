/**
 * money.test.ts — double-spend, overdraft, replay, impersonation.
 *
 * These are the four ways the payment layer loses money or lets someone spend
 * what is not theirs. Each one is a race or a forgery, so each is exercised
 * against the real store rather than a mock — an in-memory SQLite database is
 * the same code path as the file-backed one.
 */

import assert from "node:assert/strict";
import { MoneyStore } from "../src/money/store.js";
import { Mint, type Proof } from "../src/money/token.js";
import { blind, unblind, verifyDleq, decompose, newSecret, toHex, fromHex } from "../src/money/blind.js";
import {
  generateSessionKeys,
  signRequest,
  verifyRequest,
  keyMatchesSession,
  sessionIdFor,
} from "../src/money/session.js";

/* ---- ecash: a full blind round trip ------------------------------------ */
// The deep forgery/blindness/DLEQ properties live in blind.test.ts; here we
// check the Mint wrapper and its integration with the store.

const mint = new Mint(Buffer.from("test-mint-seed-do-not-use-in-prod"));

/** Client-side: blind a fresh secret of one denomination, have the mint sign it
 *  blind, verify the DLEQ, and unblind into a spendable proof. */
function drawToken(amount: number): Proof {
  const secret = newSecret();
  const { B_, r } = blind(secret);
  const sig = mint.sign({ amount, B_: toHex(B_) });
  const A = fromHex(mint.publicKeys().find((k) => k.amount === amount)!.pubkey);
  assert.ok(
    verifyDleq(A, B_, { C_: fromHex(sig.C_), e: fromHex(sig.e), s: fromHex(sig.s) }),
    "the mint's DLEQ proof must verify",
  );
  return { amount, secret: toHex(secret), C: toHex(unblind(fromHex(sig.C_), r, A)) };
}

const ecashProof = drawToken(1024);
assert.ok(mint.verify(ecashProof), "an honestly minted proof must verify");
assert.equal(mint.verify({ ...ecashProof, amount: 512 }), false, "a token cannot claim a bigger denomination");
assert.equal(mint.verify({ ...ecashProof, secret: toHex(newSecret()) }), false, "a swapped secret must fail");
assert.throws(() => mint.sign({ amount: 3, B_: toHex(blind(newSecret()).B_) }), "3 is not a denomination");

/* ---- double spend ------------------------------------------------------ */

const store = new MoneyStore(":memory:");
assert.equal(store.spendSerial(ecashProof.secret), true, "first spend wins");
assert.equal(store.spendSerial(ecashProof.secret), false, "second spend must lose");

// Many callers racing on one serial: exactly one may succeed.
const racedSecret = toHex(newSecret());
const wins = Array.from({ length: 50 }, () => store.spendSerial(racedSecret)).filter(Boolean).length;
assert.equal(wins, 1, "a serial may only ever be burned once");

/* ---- atomic multi-proof redemption ------------------------------------- */
// Redeeming a set of proofs must be all-or-nothing: if any secret in the set was
// already spent, the WHOLE redemption rolls back — nothing burned, nothing
// credited — so a double-spend cannot slip the fresh tokens through alongside it.

{
  const rstore = new MoneyStore(":memory:");
  const sess = generateSessionKeys();
  const a = drawToken(512);
  const b = drawToken(256);
  const first = rstore.redeemProofs(sess.sessionId, sess.publicKey, [a.secret, b.secret], 512 + 256);
  assert.ok(first.ok && first.session.balance === 768, "fresh proofs credit their sum");

  const fresh = drawToken(128);
  const replay = rstore.redeemProofs(sess.sessionId, sess.publicKey, [fresh.secret, a.secret], 128 + 512);
  assert.equal(replay.ok, false, "a set containing an already-spent secret is rejected");
  assert.equal(rstore.getSession(sess.sessionId)!.balance, 768, "…and nothing from that set is credited");
  assert.equal(rstore.spendSerial(fresh.secret), true, "the fresh secret was rolled back, not burned");
  rstore.close();
}

/* ---- session identity -------------------------------------------------- */

const keys = generateSessionKeys();
assert.equal(keys.sessionId, sessionIdFor(keys.publicKey));
assert.ok(keyMatchesSession(keys.publicKey, keys.sessionId));

// An impostor's key must not match someone else's session id.
const other = generateSessionKeys();
assert.equal(keyMatchesSession(other.publicKey, keys.sessionId), false);

/* ---- request signatures ------------------------------------------------ */

const body = JSON.stringify({ model: "gemini", messages: [{ role: "user", content: "hi" }] });
const sig = signRequest(keys, 1, body);
assert.ok(verifyRequest(keys.publicKey, keys.sessionId, 1, body, sig));

// Every field is bound: changing any of them must break the check.
assert.equal(verifyRequest(keys.publicKey, keys.sessionId, 2, body, sig), false, "counter is bound");
assert.equal(
  verifyRequest(keys.publicKey, keys.sessionId, 1, body.replace("hi", "write me a novel"), sig),
  false,
  "body is bound — otherwise a captured signature authorises a costlier prompt",
);
assert.equal(verifyRequest(other.publicKey, keys.sessionId, 1, body, sig), false, "key is bound");
assert.equal(verifyRequest(keys.publicKey, keys.sessionId, 1, body, "bogus"), false);

/* ---- overdraft --------------------------------------------------------- */

store.openSession(keys.sessionId, keys.publicKey, 1000);
assert.equal(store.getSession(keys.sessionId)!.balance, 1000);

// Two concurrent requests each reserving 600 cannot both succeed on 1000.
assert.equal(store.reserve(keys.sessionId, 1, 600), "ok");
assert.equal(store.reserve(keys.sessionId, 2, 600), "insufficient");
assert.equal(store.getSession(keys.sessionId)!.balance, 400, "the failed reserve must not debit");

/* ---- replay ------------------------------------------------------------ */

// A counter is consumed only by a reservation that SUCCEEDED. Counter 1 went
// through, so it is burned:
assert.equal(store.reserve(keys.sessionId, 1, 10), "replay", "a used counter is refused");
assert.equal(store.getSession(keys.sessionId)!.balance, 400, "a refused replay must not debit");

// Counter 2, by contrast, was rejected for insufficient funds and therefore
// never consumed — so the client may retry it after topping up. Burning a
// counter on a failed attempt would strand the client one number ahead of the
// server with no way to find out which.
assert.equal(store.reserve(keys.sessionId, 2, 10), "ok", "a rejected counter stays usable");
assert.equal(store.getSession(keys.sessionId)!.balance, 390);
store.settle(keys.sessionId, 10, 0); // undo, so the numbers below still line up
assert.equal(store.getSession(keys.sessionId)!.balance, 400);

/* ---- settlement -------------------------------------------------------- */

// Reserve the ceiling, spend a little, get the rest back.
assert.equal(store.reserve(keys.sessionId, 3, 400), "ok");
assert.equal(store.getSession(keys.sessionId)!.balance, 0);
const after = store.settle(keys.sessionId, 400, 12);
assert.equal(after, 388, "the unused reservation returns");

// A failed provider call returns the whole reservation.
assert.equal(store.reserve(keys.sessionId, 4, 300), "ok");
assert.equal(store.refund(keys.sessionId, 300), 388);

/* ---- unknown session --------------------------------------------------- */

assert.equal(store.reserve("f".repeat(64), 1, 1), "unknown");

/* ---- top-up ------------------------------------------------------------ */

store.openSession(keys.sessionId, keys.publicKey, 1_000);
assert.equal(store.getSession(keys.sessionId)!.balance, 1_388, "a top-up adds, never resets");

/* ---- expiry ------------------------------------------------------------ */

assert.equal(store.expire(60_000), 0, "a fresh session survives");
assert.equal(store.expire(-1), 1, "an aged-out session is dropped");
assert.equal(store.getSession(keys.sessionId), null);

store.close();
console.log("all money checks passed (double-spend, overdraft, replay, impersonation)");

/* ---- account recovery -------------------------------------------------- */

import { createAccount, fromMnemonic, accountIdFor, signAsAccount } from "../src/money/account.js";
import { verify as verifySig, createPublicKey } from "node:crypto";

const acct = createAccount();
assert.equal(acct.mnemonic.split(" ").length, 24, "24 words — see createAccount for why not 12");

// The whole point: the phrase alone rebuilds the account, on any machine.
const restored = fromMnemonic(acct.mnemonic);
assert.equal(restored.accountId, acct.accountId, "same phrase must rebuild the same account");
assert.equal(restored.publicKey, acct.publicKey);
assert.equal(restored.privateKey, acct.privateKey);

// Case and spacing are how people actually type; neither may change the account.
assert.equal(fromMnemonic(`  ${acct.mnemonic.toUpperCase()}  `).accountId, acct.accountId);

// The checksum should catch a typo rather than silently opening a different,
// empty account. At 24 words it is 8 bits, so this is reliable — at 12 it would
// let roughly one in sixteen through, which is why createAccount uses 24.
let caught = 0;
for (const w of ["zebra", "abandon", "orbit", "vault", "ranch", "gospel", "kitten", "puzzle"]) {
  const typo = acct.mnemonic.replace(/^\S+/, w);
  if (typo === acct.mnemonic) continue;
  try { fromMnemonic(typo); } catch { caught++; }
}
assert.ok(caught >= 7, `a 24-word checksum must catch nearly every single-word typo (caught ${caught}/8)`);
assert.throws(() => fromMnemonic("not even close"), /not a valid recovery phrase/);

// Two accounts must not collide.
assert.notEqual(createAccount().accountId, createAccount().accountId);

// Account signatures prove ownership to the issuer.
const proof = signAsAccount(acct, "claim-invoice", "nonce-1");
assert.ok(
  verifySig(null, Buffer.from(`${acct.accountId}:claim-invoice:nonce-1`), createPublicKey(acct.publicKey), Buffer.from(proof, "base64")),
);
// Bound to purpose and nonce, so a captured proof cannot be reused elsewhere.
assert.equal(
  verifySig(null, Buffer.from(`${acct.accountId}:withdraw:nonce-1`), createPublicKey(acct.publicKey), Buffer.from(proof, "base64")),
  false,
);
assert.equal(accountIdFor(acct.publicKey), acct.accountId);

/* ---- the layers stay separate ------------------------------------------ */

// The account key must NOT be the spending key. If it were, every request would
// be linked to the purchase forever.
const spendKeys = generateSessionKeys();
assert.notEqual(spendKeys.sessionId, acct.accountId, "spending identity is independent of the account");

console.log("all account checks passed (recovery, typos, purpose binding, layer separation)");

/* ---- the purchase path ------------------------------------------------- */

import { Issuer } from "../src/money/issuer.js";
import { FakeGateway } from "../src/money/gateway.js";

process.env.SCRAI_FAKE_PAYMENTS = "1";
const shop = new MoneyStore(":memory:");
const gw = new FakeGateway();
const issuer = new Issuer(shop, gw, mint);

const buyer = createAccount();

/** Client-side withdrawal: decompose, blind, submit, verify DLEQ, unblind. */
function withdrawFrom(iss: Issuer, account: { accountId: string }, amount: number): Proof[] {
  const state = decompose(amount).map((denom) => {
    const secret = newSecret();
    const { B_, r } = blind(secret);
    return { amount: denom, secret, r, B_ };
  });
  const { signatures } = iss.withdraw(account.accountId, state.map((s) => ({ amount: s.amount, B_: toHex(s.B_) })));
  return signatures.map((sig, i) => {
    const st = state[i]!;
    const A = fromHex(mint.publicKeys().find((k) => k.amount === st.amount)!.pubkey);
    return { amount: st.amount, secret: toHex(st.secret), C: toHex(unblind(fromHex(sig.C_), st.r, A)) };
  });
}

// Raising an invoice must not credit anything — only paying does.
const inv = await issuer.createInvoice(buyer.accountId, 10);
assert.equal(inv.amountScrai, 1_000_000, "10 USD = 1,000,000 TOKU");
assert.equal(issuer.entitlement(buyer.accountId), 0, "an unpaid invoice credits nothing");

// Money arrives.
const first = issuer.settle(inv.providerRef);
assert.equal(first?.credited, 1_000_000);
assert.equal(issuer.entitlement(buyer.accountId), 1_000_000);

// A webhook is retried until acknowledged, so settlement WILL be called again.
// Crediting twice for one payment is the expensive bug this guards.
const second = issuer.settle(inv.providerRef);
assert.equal(second?.alreadySettled, true);
assert.equal(issuer.entitlement(buyer.accountId), 1_000_000, "a repeated webhook must not credit twice");
for (let i = 0; i < 20; i++) issuer.settle(inv.providerRef);
assert.equal(issuer.entitlement(buyer.accountId), 1_000_000, "…nor twenty times");

// Withdrawing draws the entitlement down and yields spendable blind tokens.
const proofs = withdrawFrom(issuer, buyer, 400_000);
assert.equal(proofs.reduce((n, p) => n + p.amount, 0), 400_000, "the tokens sum to the withdrawal");
assert.ok(proofs.every((p) => mint.verify(p)), "every withdrawn token verifies");
assert.equal(issuer.entitlement(buyer.accountId), 600_000);

// You cannot withdraw what you do not have. (Signing is pure and happens before
// the debit, so a refusal leaves the entitlement untouched and hands out nothing.)
assert.throws(() => withdrawFrom(issuer, buyer, 600_001), /not enough entitlement/);
assert.equal(issuer.entitlement(buyer.accountId), 600_000, "a refused withdrawal must not debit");

// A stranger's account has nothing, whatever they ask for.
assert.throws(() => withdrawFrom(issuer, createAccount(), 1), /not enough entitlement/);

// End to end: the blind tokens redeem into a session for their full value, and
// exactly once — the account never appears in this half.
{
  const spender = generateSessionKeys();
  assert.ok(proofs.every((p) => mint.verify(p)), "server-side: every proof verifies before redemption");
  const redeemed = shop.redeemProofs(spender.sessionId, spender.publicKey, proofs.map((p) => p.secret), 400_000);
  assert.ok(redeemed.ok && redeemed.session.balance === 400_000, "the session is funded with the tokens' full value");
  const again = shop.redeemProofs(spender.sessionId, spender.publicKey, proofs.map((p) => p.secret), 400_000);
  assert.equal(again.ok, false, "the same tokens cannot be redeemed twice");
}

// Fixed purchase tiers: only the allowed amounts are accepted.
await assert.rejects(() => issuer.createInvoice(buyer.accountId, 0), /one of/);
await assert.rejects(() => issuer.createInvoice(buyer.accountId, 999_999), /one of/);
await assert.rejects(() => issuer.createInvoice(buyer.accountId, 12.55), /one of/);

// Settling something that was never raised.
assert.equal(issuer.settle("fake-nonexistent"), null);

// The fake gateway refuses to exist without its explicit opt-in — it accepts
// money that does not exist, so it must never come up by accident.
delete process.env.SCRAI_FAKE_PAYMENTS;
assert.throws(() => new FakeGateway(), /SCRAI_FAKE_PAYMENTS=1/);
process.env.SCRAI_FAKE_PAYMENTS = "1";

shop.close();
console.log("all purchase checks passed (idempotent settlement, entitlement, limits)");

/* ---- the late confirmation ---------------------------------------------- */

// An on-chain payment routinely confirms long after the client has stopped
// polling. Before the sweep existed, that money was taken and never credited —
// the entitlement stayed at zero and `claim` reported nothing to collect.

const slow = new MoneyStore(":memory:");
const slowGw = new FakeGateway();
const slowIssuer = new Issuer(slow, slowGw, mint);
const payer = createAccount();

const lateInv = await slowIssuer.createInvoice(payer.accountId, 20);
assert.equal(slowIssuer.entitlement(payer.accountId), 0);

// The client gives up. Nothing is polling any more.
let swept = await slowIssuer.sweep();
assert.equal(swept.settled, 0, "nothing has been paid yet");
assert.equal(slowIssuer.entitlement(payer.accountId), 0);

// The block finally confirms — the gateway now reports it paid.
slowGw.markPaid(lateInv.providerRef);

// The server's sweep is what notices. Without it the money is simply lost.
swept = await slowIssuer.sweep();
assert.equal(swept.settled, 1, "a late confirmation must still be credited");
assert.equal(slowIssuer.entitlement(payer.accountId), 2_000_000);

// Sweeping again must not credit a second time.
swept = await slowIssuer.sweep();
assert.equal(swept.settled, 0);
assert.equal(slowIssuer.entitlement(payer.accountId), 2_000_000, "a repeated sweep must not double-credit");

slow.close();
console.log("all late-payment checks passed (sweep credits, never twice)");

/* ---- recovering a balance on another device ----------------------------- */

// The gap this closes: session keys used to be random and lived in one file.
// Losing that file lost the balance permanently — the server held it against a
// public key whose private half no longer existed anywhere.

import { deriveSessionKeys } from "../src/money/account.js";

const owner = createAccount();

// Same phrase, same index, same key — on any machine.
const s0 = deriveSessionKeys(owner.mnemonic, 0);
const s0again = deriveSessionKeys(owner.mnemonic, 0);
assert.equal(s0.sessionId, s0again.sessionId, "a phrase must rebuild the same session");
assert.equal(s0.privateKey, s0again.privateKey);

// Different indices are different identities. That is what keeps two sessions
// from being linkable: the server sees unrelated keys and cannot connect them
// without the seed.
const s1 = deriveSessionKeys(owner.mnemonic, 1);
assert.notEqual(s0.sessionId, s1.sessionId, "indices must not collide");
assert.notEqual(s0.privateKey, s1.privateKey);

// A different phrase must never reach someone else's session.
assert.notEqual(deriveSessionKeys(createAccount().mnemonic, 0).sessionId, s0.sessionId);

// The derived key genuinely controls the balance: it can sign for its session.
const recovered = new MoneyStore(":memory:");
recovered.openSession(s0.sessionId, s0.publicKey, 1_600_000);
const sessionProof = signRequest(s0, 1, "body");
assert.ok(verifyRequest(s0.publicKey, s0.sessionId, 1, "body", sessionProof), "the rebuilt key must sign for its session");
assert.equal(recovered.reserve(s0.sessionId, 1, 1_000), "ok");

// A sibling session's key must not work on it.
assert.equal(verifyRequest(s1.publicKey, s0.sessionId, 2, "body", signRequest(s1, 2, "body")), false);

recovered.close();
console.log("all recovery checks passed (rebuildable, indexed, unlinkable)");
