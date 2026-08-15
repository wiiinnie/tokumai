/**
 * billing.test.ts — the money layer: SCRAI maths, Gemini usageMetadata, and the
 * metering wrapper.
 *
 * Run from the repo root (`npm test`), not from inside test/: pricing.ts
 * resolves pricing.json through process.cwd(). From the wrong directory the
 * builtin fallback takes over and every expected value below is off.
 */

import assert from "node:assert/strict";
import { computeBilling, createMeter, ceilScrai, retailRate, EMPTY_USAGE } from "../src/billing.js";
import { parseGeminiUsage, mergeGeminiUsage } from "../src/adapters/gemini-usage.js";
import type { ChatChunk, TokenUsage } from "../src/types.js";

// billing.ts reads MARGIN when it is called, not at import time, so setting it
// here still takes effect despite ESM hoisting.
process.env.MARGIN = "1.4";
process.env.MIN_CHARGE_SCRAI = "1";

/* 1) Ordinary in/out on flash-lite:
      (1000×0.30 + 500×2.50)/1M = $0.00155 = 155 SCRAI */
const u1: TokenUsage = { ...EMPTY_USAGE, inputTokens: 1000, outputTokens: 500, totalTokens: 1500 };
const b1 = computeBilling("gemini-3.5-flash-lite", u1);
assert.equal(b1.costScrai, 155);
assert.equal(b1.priceScrai, 217); // ceil(155 × 1.4) = ceil(217)
assert.equal(b1.fallbackPrice, false);
assert.equal(b1.estimated, false);

/* 2) Cache hits are cheaper: 200 fresh + 800 cached + 500 out
      (200×0.30 + 800×0.03 + 500×2.50)/1M = (60 + 24 + 1250)/1M = 133.4 SCRAI */
const u2: TokenUsage = { ...EMPTY_USAGE, inputTokens: 200, cachedInputTokens: 800, outputTokens: 500 };
assert.equal(computeBilling("gemini-3.5-flash-lite", u2).costScrai, 133.4);

/* 3) An unknown model falls back to the expensive default and says so:
      (1000×1.50 + 500×9.00)/1M = $0.0060 = 600 SCRAI */
const b3 = computeBilling("gemini-9-imaginary", u1);
assert.equal(b3.fallbackPrice, true);
assert.equal(b3.costScrai, 600);
assert.ok(b3.priceScrai > b1.priceScrai);

/* 4) Zero usage is free; real usage never rounds down to free */
assert.equal(computeBilling("gemini-3.5-flash-lite", EMPTY_USAGE).priceScrai, 0);
const tiny = computeBilling("gemini-3.5-flash-lite", { ...EMPTY_USAGE, inputTokens: 1 });
assert.equal(tiny.priceScrai, 1);

/* 5) Gemini usageMetadata: thinking counts as output, and cached tokens are
      carved out of the prompt */
const parsed = parseGeminiUsage({
  promptTokenCount: 1000,
  cachedContentTokenCount: 400,
  candidatesTokenCount: 300,
  thoughtsTokenCount: 700,
  totalTokenCount: 2000,
  promptTokensDetails: [{ modality: "TEXT", tokenCount: 1000 }],
})!;
assert.equal(parsed.inputTokens, 600); // 1000 − 400 cached
assert.equal(parsed.cachedInputTokens, 400);
assert.equal(parsed.outputTokens, 1000); // 300 candidates + 700 thoughts
assert.equal(parsed.thoughtTokens, 700);

// Audio input is separated out so it can bill at the audio rate
const audio = parseGeminiUsage({
  promptTokenCount: 500,
  candidatesTokenCount: 100,
  promptTokensDetails: [
    { modality: "TEXT", tokenCount: 200 },
    { modality: "AUDIO", tokenCount: 300 },
  ],
})!;
assert.equal(audio.audioInputTokens, 300);
assert.equal(audio.inputTokens, 200);

// Cumulative frames: the later total wins, and a partial frame must not shrink it
let acc = mergeGeminiUsage(null, { promptTokenCount: 10, totalTokenCount: 10 });
acc = mergeGeminiUsage(acc, { promptTokenCount: 10, candidatesTokenCount: 40, totalTokenCount: 50 });
acc = mergeGeminiUsage(acc, { promptTokenCount: 10, totalTokenCount: 10 });
assert.equal(acc!.totalTokens, 50);
assert.equal(acc!.outputTokens, 40);

/* 6) Meter: deltas pass through untouched, and the done chunk carries the
      billing frame — exactly once */
async function* stream(): AsyncGenerator<ChatChunk> {
  yield { delta: "Hallo ", done: false };
  yield { delta: "Welt", done: false };
  yield { delta: "", done: true, usage: u1 };
}
const meter1 = createMeter({ model: "gemini-3.5-flash-lite", promptChars: 40 });
const out: ChatChunk[] = [];
for await (const c of meter1.wrap(stream())) out.push(c);

assert.equal(out.length, 3);
assert.equal(out.map((c) => c.delta).join(""), "Hallo Welt");
assert.equal(out.filter((c) => c.usage?.billing).length, 1);
const frame1 = out.at(-1)!.usage!.billing!;
assert.equal(frame1.priceScrai, 217);
assert.equal(frame1.estimated, false);
assert.equal(out.at(-1)!.usage!.inputTokens, 1000); // the adapter's numbers survive

/* 7) No usage reported → estimate from characters, and the frame says so */
async function* noUsage(): AsyncGenerator<ChatChunk> {
  yield { delta: "x".repeat(400), done: false };
  yield { delta: "", done: true };
}
const meter2 = createMeter({ model: "gemini-3.5-flash-lite", promptChars: 400 });
const out2: ChatChunk[] = [];
for await (const c of meter2.wrap(noUsage())) out2.push(c);
const est = out2.at(-1)!.usage!.billing!;
assert.equal(est.estimated, true);
assert.equal(out2.at(-1)!.usage!.inputTokens, 100); // ~4 chars per token
assert.equal(out2.at(-1)!.usage!.outputTokens, 100);

/* 8) Adapter with no done chunk: the wrapper appends terminator + billing */
async function* noDone(): AsyncGenerator<ChatChunk> {
  yield { delta: "abgeschnitten", done: false };
}
const meter3 = createMeter({ model: "gemini-3.5-flash-lite", promptChars: 20 });
const out3: ChatChunk[] = [];
for await (const c of meter3.wrap(noDone())) out3.push(c);
assert.equal(out3.at(-1)!.done, true);
assert.ok(out3.at(-1)!.usage!.billing!.priceScrai >= 1);

/* 9) If the stream throws, the wrapper lets it through — the server then bills
      via meter.frame(), exactly as its catch block does */
async function* boom(): AsyncGenerator<ChatChunk> {
  yield { delta: "partial", done: false };
  throw new Error("provider hung up");
}
const meter4 = createMeter({ model: "gemini-3.5-flash-lite", promptChars: 8 });
const out4: ChatChunk[] = [];
let threw = false;
try {
  for await (const c of meter4.wrap(boom())) out4.push(c);
} catch {
  threw = true;
}
assert.ok(threw);
assert.equal(out4.length, 1); // just the delta, no done
const salvage = meter4.frame(); // what the server sends from its catch
assert.ok(salvage.priceScrai >= 1);
assert.equal(salvage.estimated, true);


/* 10) MIN_CHARGE_SCRAI=0: a free model stays free. A paid model still never
      rounds down to zero, because the price is formed with ceil(). */
process.env.MIN_CHARGE_SCRAI = "0";
const freeUse: TokenUsage = { ...EMPTY_USAGE, inputTokens: 57, outputTokens: 105 };
assert.equal(computeBilling("llama-3.3-70b-versatile", freeUse).priceScrai, 0);
assert.equal(computeBilling("llama-3.3-70b-versatile", freeUse).costScrai, 0);

const paidTiny = computeBilling("gemini-3.5-flash-lite", { ...EMPTY_USAGE, inputTokens: 1 });
assert.ok(paidTiny.costScrai > 0);
assert.equal(paidTiny.priceScrai, 1); // ceil() keeps it above zero

/* 11) Round up without inventing money out of floating-point noise.
      57 in × $0.30/1M computes to 27960.000000000004 — naive ceil() would turn
      that into 2.7961, a cost the provider never charged. */
assert.equal(ceilScrai(27960.000000000004 / 10_000), 2.796);
assert.equal(ceilScrai(2.7960001), 2.7961);   // a genuine fraction does round up
assert.equal(ceilScrai(0), 0);

/* 12) Retail rates carry the margin and round up */
process.env.MARGIN = "1.1";
const rate = retailRate("gemini-3.5-flash");
assert.equal(rate.in, 165000);   // 1.5 USD/1M × 100000 SCRAI/USD × 1.1
assert.equal(rate.out, 990000);  // 9.0 USD/1M × 100000 SCRAI/USD × 1.1
const freeRate = retailRate("llama-3.3-70b-versatile");
assert.equal(freeRate.in, 0);
assert.equal(freeRate.out, 0);

console.log("all billing checks passed (incl. min-charge, rounding, rates)");
