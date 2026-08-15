// ---------------------------------------------------------------------------
// billing.ts — turns token counts into SCRAI.
//
// THE UNIT: 1 SCRAI = USD 0.00001, so 10 USD = 1 000 000 SCRAI. SCRAI is the
// only money unit in this codebase — there is no second one and no conversion.
//
// Why this fine: every exchange is priced with ceil(), so the unit size is the
// worst-case overcharge per request. At USD 0.0001 a one-token answer cost a
// full cent-hundredth more than it should; at USD 0.00001 that error is ten
// times smaller. Granularity is not cosmetic here, it is the rounding loss.
//
// The margin lives here and only here (MARGIN env), server-side, so a patched
// client cannot change what it pays. The provider's price list lives in
// pricing.ts, deliberately margin-free, so that table could be published.
//
// Nothing in this file persists anything: a frame is computed in-flight and dies
// with the response. The server stays as stateless as it claims to be.
// ---------------------------------------------------------------------------

import { priceFor, pricingVersion, type ModelPrice } from "./pricing.js";
import type { BillingFrame, ChatChunk, TokenUsage } from "./types.js";

/** 1 SCRAI = USD 0.00001, so 10 USD buys 1 000 000 SCRAI. */
export const SCRAI_PER_USD = 100_000;

/**
 * Fixed purchase amounts, in USD.
 *
 * Everyone buys the SAME handful of amounts, so the sum a purchase reveals — at
 * the invoice, at the withdrawal, and in the denomination set of a redemption —
 * is one of a few shared buckets rather than a per-user fingerprint. A $12.55
 * top-up would stand out; a $10 one hides among every other $10. The server
 * enforces this; SCRAI_PURCHASE_TIERS overrides the list operator-side.
 */
export const DEFAULT_PURCHASE_TIERS = [5, 10, 20, 50];

export function purchaseTiers(): number[] {
  const raw = process.env.SCRAI_PURCHASE_TIERS;
  if (!raw) return DEFAULT_PURCHASE_TIERS;
  const parsed = raw
    .split(",")
    .map((s) => Number(s.trim()))
    .filter((n) => Number.isFinite(n) && n > 0);
  return parsed.length ? [...new Set(parsed)].sort((a, b) => a - b) : DEFAULT_PURCHASE_TIERS;
}

export const EMPTY_USAGE: TokenUsage = {
  inputTokens: 0,
  outputTokens: 0,
  cachedInputTokens: 0,
  audioInputTokens: 0,
  thoughtTokens: 0,
  totalTokens: 0,
};

function margin(): number {
  const raw = Number(process.env.MARGIN ?? 1.1);
  if (!Number.isFinite(raw) || raw < 1) {
    console.warn(`[billing] MARGIN="${process.env.MARGIN}" invalid — falling back to 1.0`);
    return 1.0;
  }
  return raw;
}

/**
 * Floor per billed request, in whole SCRAI. Default 0: a model that costs us
 * nothing costs the user nothing. Set it above zero only if you want a request
 * that reached a provider to never be free — which would make every free-tier
 * model paid, so think before you do.
 */
function minCharge(): number {
  const raw = Number(process.env.MIN_CHARGE_SCRAI ?? 0);
  return Number.isFinite(raw) && raw >= 0 ? Math.floor(raw) : 0;
}

/** Provider cost in USD for one exchange. */
export function costUsd(usage: TokenUsage, price: ModelPrice): number {
  const cachedRate = price.cached_in ?? price.in;
  const audioRate = price.audio_in ?? price.in;
  return (
    (usage.inputTokens * price.in +
      (usage.cachedInputTokens ?? 0) * cachedRate +
      (usage.audioInputTokens ?? 0) * audioRate +
      usage.outputTokens * price.out) /
    1_000_000
  );
}

/**
 * Round a SCRAI amount UP, at 4 decimal places, without inventing money out of
 * floating-point noise.
 *
 * The naive `Math.ceil(x * 10_000) / 10_000` is wrong here: 57 input tokens at
 * $0.30/1M computes to 27960.000000000004, and ceiling that yields 2.7961
 * instead of 2.796 — a cost the provider never charged. Normalising the
 * significant digits first drops the noise and leaves genuine fractions intact.
 */
export function ceilScrai(scrai: number): number {
  const scaled = scrai * 10_000;
  return Math.ceil(Number(scaled.toPrecision(12))) / 10_000;
}

/** Rough token count when a provider reports no usage at all. ~4 chars/token. */
export function estimateTokens(chars: number): number {
  return Math.ceil(chars / 4);
}

/**
 * Build the frame. Cost keeps 4 decimals of a SCRAI so long conversations don't
 * accumulate rounding error; price is whole SCRAI, rounded up, with a floor —
 * a request that actually reached the provider is never free.
 */
export function computeBilling(
  model: string,
  usage: TokenUsage,
  opts: { estimated?: boolean } = {},
): BillingFrame {
  const price = priceFor(model);
  const cost = ceilScrai(costUsd(usage, price) * SCRAI_PER_USD);
  const billable =
    usage.inputTokens +
    (usage.cachedInputTokens ?? 0) +
    (usage.audioInputTokens ?? 0) +
    usage.outputTokens;

  return {
    model,
    costScrai: cost,
    priceScrai: billable > 0 ? Math.max(Math.ceil(cost * margin()), minCharge()) : 0,
    pricingVersion: pricingVersion(),
    estimated: opts.estimated ?? false,
    fallbackPrice: price.fallback === true,
  };
}

/**
 * Retail rate for a model, in SCRAI per 1M tokens, margin already applied.
 *
 * This is what the client needs to estimate a price before sending. Note the
 * consequence: shipping this makes the margin inferable by anyone comparing it
 * to the provider's public list. That is fine — the protection was never
 * secrecy. A client that knows the price still cannot set it; the server prices
 * every exchange from its own table and refuses anything that does not add up.
 */
export function retailRate(model: string): { in: number; out: number } {
  const p = priceFor(model);
  const m = margin();
  return {
    in: ceilScrai(p.in * SCRAI_PER_USD * m),
    out: ceilScrai(p.out * SCRAI_PER_USD * m),
  };
}

// ---------------------------------------------------------------------------
// Meter
//
// Wraps the adapter stream: every chunk passes through untouched except the
// final one, which gets `usage.billing` attached. The frame is also reachable
// via meter.frame() so the server's catch block can bill a failed stream —
// tokens already generated cost us whether or not the answer arrived.
// ---------------------------------------------------------------------------

export interface Meter {
  wrap(inner: AsyncIterable<ChatChunk>): AsyncGenerator<ChatChunk>;
  /** Usage seen so far, or a character estimate when the provider reported none. */
  usage(): TokenUsage;
  frame(): BillingFrame;
}

export function createMeter(opts: { model: string; promptChars?: number }): Meter {
  let reported: TokenUsage | null = null;
  let outChars = 0;

  const usage = (): TokenUsage => {
    if (reported) return reported;
    const inputTokens = estimateTokens(opts.promptChars ?? 0);
    const outputTokens = estimateTokens(outChars);
    return { ...EMPTY_USAGE, inputTokens, outputTokens, totalTokens: inputTokens + outputTokens };
  };

  const frame = (): BillingFrame =>
    computeBilling(opts.model, usage(), { estimated: reported === null });

  return {
    usage,
    frame,

    async *wrap(inner: AsyncIterable<ChatChunk>): AsyncGenerator<ChatChunk> {
      let billed = false;

      for await (const chunk of inner) {
        if (chunk.usage) reported = chunk.usage;
        if (chunk.delta) outChars += chunk.delta.length;

        if (chunk.done && !billed) {
          billed = true;
          yield { ...chunk, usage: { ...usage(), billing: frame() } };
          continue;
        }
        yield chunk;
      }

      // An adapter that ends without a done chunk still gets billed and the
      // client still gets its terminator.
      if (!billed) yield { delta: "", done: true, usage: { ...usage(), billing: frame() } };
    },
  };
}
