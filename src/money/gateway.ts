// ---------------------------------------------------------------------------
// gateway.ts — where money actually comes from.
//
// THIS IS THE SEAM. Everything above it deals in "an invoice was raised" and
// "an invoice was paid" and knows nothing about Bitcoin, Lightning, chains or
// exchange rates. Swapping the fake for a real BTCPay Server therefore means
// implementing one interface and changing one env var — no protocol change, no
// issuer change, no client change.
//
// Two implementations live here:
//
//   FakeGateway    development. Raises invoices that can be settled by a
//                  command instead of a payment. No money moves, no network
//                  call leaves the machine. It refuses to load unless
//                  SCRAI_FAKE_PAYMENTS=1, so it cannot be switched on by
//                  accident in production.
//
//   BTCPayGateway  the real one. Deliberately left unimplemented rather than
//                  half-written: a payment integration that looks finished but
//                  is not is worse than one that says so.
//
// A note on what settles: BTCPay prices an invoice in USD and receives BTC.
// The operator therefore carries the exchange risk between payment and
// conversion, and that is a business decision this file cannot make. What it
// does guarantee is that the TOKU amount is fixed when the invoice is RAISED,
// so a user always gets what they were quoted.
// ---------------------------------------------------------------------------

export type InvoiceStatus = "pending" | "paid" | "expired";

/**
 * One way to pay the same invoice — Lightning or on-chain, usually both.
 *
 * These travel to the client OVER THE MIXNET and are rendered there. That is
 * the whole reason this type exists instead of a checkout URL: sending the user
 * to a hosted payment page would have their browser connect straight to our
 * BTCPay server, handing us their IP at the exact moment they are least
 * anonymous. Everything the mixnet protects would be undone at the till.
 */
export interface PaymentOption {
  /** "BTC-LightningNetwork", "BTC", … */
  method: string;
  /** The address or BOLT11 string to pay. */
  destination: string;
  /** A BIP21/lightning: URI, which is what belongs in a QR code. */
  uri: string;
  /** Amount in that method's own currency, as a string to avoid float drift. */
  amount: string;
  currency: string;
  /**
   * A destination tag the payment MUST carry to be credited — the Nyx-chain memo
   * for NYM. Absent for Bitcoin, where the address alone identifies the invoice.
   */
  memo?: string;
}

export interface Invoice {
  /** Our id, and what the client polls on. */
  id: string;
  /** The gateway's own id — a BTCPay invoice id; unused by the fake. */
  providerRef: string;
  /** Legacy single-destination field, kept for the fake gateway's instructions. */
  payTo: string;
  /** Human-readable instruction to show alongside the options. */
  instruction: string;
  /** Every way this invoice can be paid. Empty for the fake gateway. */
  options: PaymentOption[];
  amountUsd: number;
  amountScrai: number;
  expiresAt: number;
}

/** Liveness of a chain-watching gateway (NYM). connected = we would hear a tx. */
export interface WatchState {
  connected: boolean;
  height: number | null;
  lastBlockAgoMs: number | null;
  /** How the chain is being watched: live socket vs HTTP polling. */
  mode: "websocket" | "polling" | null;
}

export interface PaymentGateway {
  readonly name: string;
  /** True when settlement happens by command rather than by money. */
  readonly isFake: boolean;
  /** Live chain-watch health, for gateways that watch a chain (NYM). */
  watchState?(): WatchState;
  /** Optional: connect + pre-fetch at startup so the first invoice is fast. */
  warmup?(): void;
  /** Optional: stop watching a still-pending invoice (user cancelled it). */
  cancel?(providerRef: string): void;
  /** Raise an invoice for this many USD. */
  createInvoice(amountUsd: number, reference: string): Promise<Omit<Invoice, "id" | "amountScrai">>;
  /**
   * Ask the gateway whether it has been paid.
   *
   * Real gateways push a webhook as well; polling exists because a webhook that
   * is missed must not strand a paying customer.
   */
  checkStatus(providerRef: string): Promise<InvoiceStatus>;
}

// ---------------------------------------------------------------------------

/** Invoices settle when told to, not when paid. Development only. */
export class FakeGateway implements PaymentGateway {
  readonly name = "fake";
  readonly isFake = true;
  private settled = new Set<string>();

  constructor() {
    if (process.env.SCRAI_FAKE_PAYMENTS !== "1") {
      throw new Error(
        "the fake payment gateway needs SCRAI_FAKE_PAYMENTS=1 — it accepts money that does not exist",
      );
    }
  }

  async createInvoice(amountUsd: number, reference: string) {
    const providerRef = `fake-${reference}`;
    return {
      providerRef,
      payTo: providerRef,
      options: [] as PaymentOption[],
      instruction:
        `DEV MODE — no real payment. In ANOTHER terminal, run:\n\n` +
        `      npm run issuer -- settle ${providerRef}\n\n` +
        `  This window picks it up automatically within 5 seconds.`,
      amountUsd,
      expiresAt: Date.now() + 60 * 60_000, // 60 min — realistic for on-chain (~20 min blocks)
    };
  }

  async checkStatus(providerRef: string): Promise<InvoiceStatus> {
    return this.settled.has(providerRef) ? "paid" : "pending";
  }

  /** The fake half: what a real payment would have done. */
  markPaid(providerRef: string): void {
    this.settled.add(providerRef);
  }
}

// ---------------------------------------------------------------------------

/**
 * BTCPay Server, via its Greenfield API.
 *
 * Left as a stub on purpose. What it will need:
 *   - BTCPAY_URL, BTCPAY_STORE_ID, BTCPAY_API_KEY
 *   - POST /api/v1/stores/{storeId}/invoices  with the amount in USD
 *   - a PUBLIC webhook endpoint for InvoiceSettled — which the mixnet-only
 *     scrai-server deliberately does not have, so the issuer is the piece that
 *     must be publicly reachable
 *   - webhook signature verification, and idempotent settlement, because a
 *     webhook can and will arrive more than once
 */
export class BTCPayGateway implements PaymentGateway {
  readonly name = "btcpay";
  readonly isFake = false;

  constructor(
    private readonly baseUrl: string,
    private readonly storeId: string,
    private readonly apiKey: string,
  ) {
    if (!baseUrl || !storeId || !apiKey) {
      throw new Error("BTCPay needs BTCPAY_URL, BTCPAY_STORE_ID and BTCPAY_API_KEY");
    }
    this.baseUrl = baseUrl.replace(/\/+$/, "");
  }

  private async call<T>(path: string, init?: RequestInit): Promise<T> {
    const res = await fetch(`${this.baseUrl}${path}`, {
      ...init,
      headers: {
        // BTCPay's own scheme, not Bearer.
        authorization: `token ${this.apiKey}`,
        "content-type": "application/json",
        ...(init?.headers ?? {}),
      },
      signal: AbortSignal.timeout(20_000),
    });
    if (!res.ok) {
      const body = await res.text().catch(() => "");
      throw new Error(explainBTCPay(res.status, body, path));
    }
    return (await res.json()) as T;
  }

  async createInvoice(amountUsd: number, reference: string) {
    const inv = await this.call<{ id: string; expirationTime?: number; checkoutLink?: string }>(
      `/api/v1/stores/${this.storeId}/invoices`,
      {
        method: "POST",
        body: JSON.stringify({
          amount: amountUsd.toFixed(2),
          currency: "USD",
          // Our own id, so a webhook or a support question can be traced back
          // to an invoice without BTCPay knowing anything about the account.
          metadata: { orderId: reference },
          checkout: { redirectAutomatically: false },
        }),
      },
    );

    const options = await this.paymentOptions(inv.id);

    return {
      providerRef: inv.id,
      payTo: options[0]?.destination ?? "",
      instruction:
        options.length > 1
          ? "Pay with any of the options below — Lightning settles instantly."
          : "Pay to the destination below.",
      options,
      amountUsd,
      // BTCPay reports expiry in seconds; a missing value means the default
      // 15-minute window, which is also how long the rate is held.
      // Honour BTCPay's own expiry (set on the store); fall back to 60 min so an
      // on-chain payment (~20 min blocks) has a realistic window to confirm.
      expiresAt: inv.expirationTime ? inv.expirationTime * 1000 : Date.now() + 60 * 60_000,
    };
  }

  /**
   * The actual destinations, fetched separately.
   *
   * Creating an invoice does not return them — BTCPay exposes them through
   * /payment-methods, and with lazy payments enabled a method may need
   * activating before it has one. An unactivated method has no destination, so
   * it is filtered out rather than shown as an empty box.
   */
  private async paymentOptions(invoiceId: string): Promise<PaymentOption[]> {
    const methods = await this.call<
      Array<{
        paymentMethodId?: string;
        currency?: string;
        destination?: string;
        paymentLink?: string;
        amount?: string;
        activated?: boolean;
      }>
    >(`/api/v1/invoices/${invoiceId}/payment-methods`);

    return methods
      .filter((m) => m.destination && m.activated !== false)
      .map((m) => ({
        method: m.paymentMethodId ?? m.currency ?? "BTC",
        destination: m.destination!,
        uri: m.paymentLink ?? m.destination!,
        amount: m.amount ?? "",
        currency: m.currency ?? "BTC",
      }));
  }

  /**
   * BTCPay statuses map onto ours as follows:
   *
   *   Settled     paid and confirmed to the store's satisfaction -> paid
   *   Processing  seen on the network, not yet confirmed         -> pending
   *   New         nothing has arrived                            -> pending
   *   Expired     the window closed                              -> expired
   *   Invalid     paid too little, too late, or reversed         -> expired
   *
   * Processing deliberately does NOT count as paid. How many confirmations are
   * required is a BTCPay setting; honouring "Settled" means honouring whatever
   * the operator configured, rather than second-guessing it here.
   */
  async checkStatus(providerRef: string): Promise<InvoiceStatus> {
    const inv = await this.call<{ status: string }>(`/api/v1/invoices/${providerRef}`);
    switch (inv.status) {
      case "Settled":
        return "paid";
      case "Expired":
      case "Invalid":
        return "expired";
      default:
        return "pending";
    }
  }
}

// ---------------------------------------------------------------------------

/**
 * Turn a BTCPay error into something an operator can act on.
 *
 * These are all setup problems rather than bugs, and each has exactly one fix.
 * Passing the raw JSON through means reading a stack trace to learn that a
 * checkbox was not ticked.
 */
function explainBTCPay(status: number, body: string, path: string): string {
  const msg = (() => {
    try {
      return String((JSON.parse(body) as { message?: string }).message ?? body);
    } catch {
      return body;
    }
  })();

  if (/no wallet has been linked/i.test(msg)) {
    return (
      "BTCPay has no wallet linked to this store, so it cannot take payments.\n" +
      "  Fix it in BTCPay: Store -> Settings -> Wallets -> Bitcoin -> Setup.\n" +
      "  On testnet you can let it generate a new hot wallet; on mainnet connect\n" +
      "  your own xpub so BTCPay can watch for payments but never spend."
    );
  }
  if (status === 401 || status === 403) {
    return (
      "BTCPay rejected the API key.\n" +
      "  Check BTCPAY_API_KEY, and that it carries both\n" +
      "    btcpay.store.cancreateinvoice\n" +
      "    btcpay.store.canviewinvoices"
    );
  }
  if (status === 404) {
    return `BTCPay does not know this store or invoice — check BTCPAY_STORE_ID (${path})`;
  }
  return `BTCPay ${path} -> ${status} ${msg.slice(0, 200)}`;
}

/**
 * Pick the gateway from the environment.
 *
 * Fails loudly when nothing is configured rather than quietly falling back to
 * the fake — an issuer that hands out TOKU for imaginary money should never be
 * the default.
 */
export function selectGateway(): PaymentGateway {
  if (process.env.SCRAI_FAKE_PAYMENTS === "1") {
    // Say so when the fake wins despite real credentials being present. Silently
    // ignoring a configured BTCPay is exactly the kind of surprise that costs an
    // afternoon: the operator sees "gateway=fake", checks their keys, finds them
    // correct, and has nothing to go on.
    if (process.env.BTCPAY_URL || process.env.BTCPAY_STORE_ID || process.env.BTCPAY_API_KEY) {
      console.warn(
        "[gateway] BTCPay is configured but SCRAI_FAKE_PAYMENTS=1 takes precedence.\n" +
          "          Comment that line out in .env to use BTCPay.",
      );
    }
    return new FakeGateway();
  }
  return new BTCPayGateway(
    process.env.BTCPAY_URL ?? "",
    process.env.BTCPAY_STORE_ID ?? "",
    process.env.BTCPAY_API_KEY ?? "",
  );
}

/**
 * The NYM gateway, or null when it isn't configured.
 *
 * NYM is additive: it only appears as a payment method when the operator has set
 * a receive address and an RPC endpoint, so a server without them simply keeps
 * offering Bitcoin and nothing breaks. Imported lazily so the CosmJS dependency
 * only loads when NYM is actually switched on.
 */
export async function selectNyxGateway(): Promise<PaymentGateway | null> {
  const address = process.env.NYX_RECEIVE_ADDRESS;
  const ws = process.env.NYX_RPC_WS;
  const http = process.env.NYX_RPC_HTTP;
  // Need the address and at least one endpoint. WS is preferred (own node); HTTP
  // is the fallback that works against public RPCs, whose /websocket is usually
  // off. Either env alone is enough — the gateway derives the other.
  if (!address || (!ws && !http)) return null;
  const { NyxGateway } = await import("./nyx-gateway.js");
  const ttlMs = process.env.NYX_INVOICE_TTL_MS ? Number(process.env.NYX_INVOICE_TTL_MS) : undefined;
  return new NyxGateway(address, { ws, http }, { ttlMs });
}
