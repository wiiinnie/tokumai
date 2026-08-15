/**
 * pricing.ts — the only place that knows what a token costs.
 *
 * Resolution order, best first:
 *   1. remote table  (PRICING_URL, fetched + cached in memory, TTL)
 *   2. local file    (PRICING_FILE, default ./pricing.json, hot-reloaded in dev)
 *   3. built-in      (BUILTIN below — conservative, so a broken deploy overcharges
 *                     rather than serving tokens for free)
 *
 * Prices are provider cost only. The margin lives in MARGIN (see billing.ts) and
 * is never part of this table, so the table can be published/shared without
 * leaking what we charge.
 */

import { readFileSync, watch } from "node:fs";
import { resolve as resolvePath } from "node:path";

export interface ModelPrice {
  label?: string;
  /** USD per 1M uncached input tokens (text/image/video). */
  in: number;
  /** USD per 1M output tokens. Includes thinking tokens on Gemini. */
  out: number;
  /** USD per 1M cache-hit input tokens. Falls back to `in` when absent. */
  cached_in?: number;
  /** USD per 1M audio input tokens. Falls back to `in` when absent. */
  audio_in?: number;
  /** Alias whose target Google may repoint at any time. */
  floating?: boolean;
  /** Set when this entry came from `default` rather than an exact match. */
  fallback?: boolean;
  note?: string;
}

export interface PricingTable {
  schema: string;
  version: string;
  source?: string;
  tier?: string;
  currency?: string;
  unit?: string;
  note?: string;
  default: ModelPrice;
  models: Record<string, ModelPrice>;
}

const SCHEMA = "scrambler/pricing@1";

/** Last-resort table. Deliberately expensive: matches the priciest flash tier. */
const BUILTIN: PricingTable = {
  schema: SCHEMA,
  version: "builtin",
  tier: "paid-standard",
  currency: "USD",
  unit: "per_1m_tokens",
  default: { label: "builtin fallback", in: 1.5, out: 9.0, cached_in: 0.15, fallback: true },
  models: {},
};

const FILE = process.env.PRICING_FILE ?? resolvePath(process.cwd(), "pricing.json");
const URL_ = process.env.PRICING_URL ?? "";
const TTL_MS = Number(process.env.PRICING_TTL_SEC ?? 3600) * 1000;
const WATCH = process.env.NODE_ENV !== "production";

let local: PricingTable = BUILTIN;
let remote: PricingTable | null = null;
let remoteEtag: string | null = null;
let remoteFetchedAt = 0;
let remoteInflight: Promise<void> | null = null;

function validate(raw: unknown): PricingTable {
  if (!raw || typeof raw !== "object") throw new Error("pricing: not an object");
  const t = raw as PricingTable;
  if (t.schema !== SCHEMA) throw new Error(`pricing: unexpected schema ${t.schema}`);
  if (!t.default || typeof t.default.in !== "number" || typeof t.default.out !== "number") {
    throw new Error("pricing: missing or invalid default entry");
  }
  if (!t.models || typeof t.models !== "object") throw new Error("pricing: missing models");
  for (const [id, p] of Object.entries(t.models)) {
    if (typeof p?.in !== "number" || typeof p?.out !== "number") {
      throw new Error(`pricing: model ${id} needs numeric in/out`);
    }
    if (p.in < 0 || p.out < 0) throw new Error(`pricing: model ${id} has negative price`);
  }
  return t;
}

function loadLocal(): void {
  try {
    local = validate(JSON.parse(readFileSync(FILE, "utf8")));
    console.log(`[pricing] local table ${local.version} (${Object.keys(local.models).length} models)`);
  } catch (err) {
    console.error(`[pricing] local table unusable, keeping previous/builtin: ${(err as Error).message}`);
  }
}

loadLocal();

if (WATCH) {
  try {
    // Dev convenience: edit pricing.json, next request uses it. No restart.
    watch(FILE, { persistent: false }, () => setTimeout(loadLocal, 50));
  } catch {
    /* file may not exist yet — builtin carries us */
  }
}

async function refreshRemote(): Promise<void> {
  if (!URL_) return;
  if (Date.now() - remoteFetchedAt < TTL_MS) return;
  if (remoteInflight) return remoteInflight;

  remoteInflight = (async () => {
    try {
      const res = await fetch(URL_, {
        headers: remoteEtag ? { "if-none-match": remoteEtag } : {},
        signal: AbortSignal.timeout(5000),
      });
      if (res.status === 304) {
        remoteFetchedAt = Date.now();
        return;
      }
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const table = validate(await res.json());
      remote = table;
      remoteEtag = res.headers.get("etag");
      remoteFetchedAt = Date.now();
      console.log(`[pricing] remote table ${table.version}`);
    } catch (err) {
      // Keep the stale remote copy if we have one; otherwise local/builtin serve.
      remoteFetchedAt = Date.now() - TTL_MS / 2; // retry sooner than a full TTL
      console.warn(`[pricing] remote refresh failed: ${(err as Error).message}`);
    } finally {
      remoteInflight = null;
    }
  })();

  return remoteInflight;
}

function table(): PricingTable {
  return remote ?? local;
}

/** Kick off a remote refresh without blocking the request path. */
export function warmPricing(): void {
  void refreshRemote();
}

/**
 * How many days old the active table's `version` date is, or null when the
 * version is not a date.
 *
 * The table is maintained by hand: nothing fetches a provider's real prices,
 * because they publish them as documentation, not as an API. So when a provider
 * raises a rate, this file keeps charging the old one and the operator absorbs
 * the difference — silently, forever. An age is the only cheap signal that
 * something needs re-checking.
 */
export function pricingAgeDays(): number | null {
  const v = table().version;
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(v);
  if (!m) return null;
  const then = Date.UTC(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
  return Math.floor((Date.now() - then) / 86_400_000);
}

/** Table identifier that goes into the billing frame, for support/debugging. */
export function pricingVersion(): string {
  const t = table();
  return `${t.version}${remote ? "+remote" : ""}`;
}

/**
 * Price for a model id. Never throws and never returns undefined — an unknown id
 * gets the conservative default with `fallback: true` so it shows up in logs.
 */
/**
 * Does this model have an EXPLICIT price entry (not the fallback)? A model with
 * only the fallback price is a business risk — we might charge less than the
 * provider bills us — so it must never be offered or served. Note an explicit
 * `{in:0, out:0}` (a genuinely free model) counts as priced.
 */
export function hasPrice(model: string): boolean {
  return Object.prototype.hasOwnProperty.call(table().models, model);
}

export function priceFor(model: string): ModelPrice {
  void refreshRemote(); // fire and forget; this call uses whatever is loaded now
  const t = table();
  const hit = t.models[model];
  if (hit) return hit;
  console.warn(`[pricing] no entry for "${model}" — using default`);
  return { ...t.default, fallback: true };
}

/** Everything the /models endpoint needs to show a price hint per model. */
export function priceCatalog(): Array<{ model: string } & ModelPrice> {
  const t = table();
  return Object.entries(t.models).map(([model, p]) => ({ model, ...p }));
}
