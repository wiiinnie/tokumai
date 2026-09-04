import type { ChatRequest, ChatChunk } from "./types.js";

// ---------------------------------------------------------------------------
// The contract every provider adapter implements.
//
// An adapter does exactly one thing: take a neutral ChatRequest and yield
// neutral ChatChunks, translating the provider's wire format on the way in and
// out. It owns nothing else — no auth policy, no billing, no transport. That
// keeps each adapter tiny and keeps the server provider-agnostic.
// ---------------------------------------------------------------------------

/**
 * What a model produces. The client needs this before it sends anything:
 * an image answer is ~1000x the size of a text one, so the reply-SURB budget
 * has to be sized for it up front. Guessing from the model name would work
 * today and break the first time a vendor names something differently.
 */
export type ModelKind = "text" | "image";

export interface ModelAdapter {
  /** Neutral model ids this adapter serves, e.g. ["gemini-2.5-flash"]. */
  readonly models: string[];
  readonly vendor: string;
  readonly kind: ModelKind;
  /**
   * Env var holding this provider's credential, e.g. "GEMINI_API_KEY". The server
   * looks it up and injects the value — the adapter never reads the environment
   * for it, so a provider's secret can never leak into an adapter's own code.
   *
   * Omit for a provider that needs no credential. Keyed ones are skipped when
   * their var is unset, so an operator only ever offers what they can serve.
   */
  readonly apiKeyEnv?: string;
  /**
   * Whether this provider trains on inputs by default. Surfaced to the client
   * so it can show the "trains on input" badge — a first-class privacy signal,
   * not an afterthought.
   */
  readonly trainsOnInput: boolean;

  /**
   * Whether this model accepts file inputs (images / PDFs / text) — i.e. it is
   * multimodal on the INPUT side. Surfaced to the client so the "+" attach gate
   * is per-model capability, not a vendor guess. Defaults to false.
   */
  readonly acceptsImages?: boolean;

  /**
   * Optional: query the provider's live /models endpoint and return the model
   * ids this adapter should currently serve (already filtered to its kind). When
   * present, the server calls it at startup to REPLACE the static `models` list,
   * so deprecated ids drop off and new ones appear without a code change. All
   * returned models inherit this adapter's kind / acceptsImages / trainsOnInput.
   */
  discover?(apiKey: string): Promise<string[]>;

  /** Stream a completion. `apiKey` is injected by the server, never hard-coded here. */
  stream(req: ChatRequest, apiKey: string): AsyncGenerator<ChatChunk>;
}

// ---- registry -------------------------------------------------------------

const registry = new Map<string, ModelAdapter>();

export function register(adapter: ModelAdapter): void {
  for (const m of adapter.models) registry.set(m, adapter);
}

/**
 * Register only if this adapter's credential is actually present.
 *
 * Returns false when it was skipped, so the server can say which providers are
 * dormant and why — an empty model list with no explanation is a bad afternoon.
 */
export function registerIfAvailable(adapter: ModelAdapter): boolean {
  if (adapter.apiKeyEnv && !process.env[adapter.apiKeyEnv]) return false;
  register(adapter);
  return true;
}

/** The credential this adapter needs, or "" when it needs none. */
export function keyFor(adapter: ModelAdapter): string {
  return adapter.apiKeyEnv ? (process.env[adapter.apiKeyEnv] ?? "") : "";
}

/** Drop a model from the registry — used to self-heal when a provider reports it
 *  as gone (404 / not available), so it stops being offered and served. */
export function deregister(model: string): boolean {
  return registry.delete(model);
}

export function resolve(model: string): ModelAdapter {
  const a = registry.get(model);
  if (!a) throw new Error(`no adapter registered for model "${model}"`);
  return a;
}

/**
 * Replace each discoverable provider's static model list with what its API
 * currently serves — but ONLY models that carry an explicit price (`isPriced`),
 * because an unpriced model is a money-losing hole (we might charge less than the
 * provider bills us). Falls back to the static list on any error so the server
 * still starts if a provider is unreachable.
 */
export interface DiscoveryResult {
  provider: string;
  offered: string[];
  /** Discovered but skipped because they lack an explicit price (a paid model we
   *  cannot safely charge for). The operator can add these to pricing.json. */
  skippedNoPrice: string[];
  error?: string;
}

export async function refreshCatalog(
  providers: ModelAdapter[],
  isPriced: (model: string) => boolean,
): Promise<DiscoveryResult[]> {
  const report: DiscoveryResult[] = [];
  for (const a of providers) {
    if (!a.discover) continue;
    if (a.apiKeyEnv && !process.env[a.apiKeyEnv]) continue;
    try {
      const discovered = await a.discover(keyFor(a));
      const priced = discovered.filter(isPriced);
      const skipped = discovered.filter((m) => !isPriced(m));
      // Only swap in the live list when it actually yields priced models — else a
      // provider that momentarily lists nothing priced would wipe the static set.
      if (priced.length > 0) {
        for (const [m, ad] of [...registry]) if (ad === a) registry.delete(m);
        for (const m of priced) registry.set(m, a);
      }
      report.push({ provider: a.vendor, offered: priced, skippedNoPrice: skipped });
    } catch (e) {
      report.push({ provider: a.vendor, offered: [], skippedNoPrice: [], error: (e as Error).message });
    }
  }
  return report;
}

/**
 * Hard safety net: remove any registered model without an explicit price, so it
 * can neither be offered (catalog) nor served (resolve). Catches static models
 * too, not just discovered ones. Returns the ids it pruned.
 */
export function pruneUnpriced(isPriced: (model: string) => boolean): string[] {
  const pruned: string[] = [];
  for (const [m] of [...registry]) {
    if (!isPriced(m)) {
      registry.delete(m);
      pruned.push(m);
    }
  }
  return pruned;
}

/** For the client's model picker: everything currently wired up. */
export function catalog(): Array<{ model: string; vendor: string; kind: ModelKind; trainsOnInput: boolean; acceptsImages: boolean }> {
  const out: Array<{ model: string; vendor: string; kind: ModelKind; trainsOnInput: boolean; acceptsImages: boolean }> = [];
  for (const [model, a] of registry) {
    out.push({ model, vendor: a.vendor, kind: a.kind, trainsOnInput: a.trainsOnInput, acceptsImages: a.acceptsImages ?? false });
  }
  return out;
}
