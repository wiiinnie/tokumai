// ---------------------------------------------------------------------------
// issuer.ts — turns a payment into spendable TOKU.
//
// Three steps, and the order of the middle two is what protects the user:
//
//   1. RAISE     an invoice against an account. The account is known here; it
//                has to be, or a payment could not be credited to anyone.
//   2. SETTLE    the gateway says it was paid -> the account gains an
//                ENTITLEMENT. Still fully linkable, still just bookkeeping.
//   3. WITHDRAW  the account trades entitlement for bearer tokens. Today those
//                are HMAC-signed and the issuer sees their serials; with blind
//                signatures it will sign values it cannot read, and the link
//                between account and token disappears for good.
//
// Step 3 is the only one that changes when the real crypto lands, and it
// changes inside `token.ts` — nothing here moves.
//
// WHY THE ISSUER MUST EVENTUALLY BE ITS OWN SERVICE: a payment gateway confirms
// by webhook, over public HTTP. The scrai-server has no public address by
// design — that is the entire point of the mixnet model. So the issuer is the
// piece that faces the internet, and the AI server stays unreachable. Today
// they share a process for convenience; the module boundary is here so that
// splitting them is a deployment change, not a rewrite.
// ---------------------------------------------------------------------------

import { randomUUID } from "node:crypto";
import { SCRAI_PER_USD, purchaseTiers } from "../billing.js";
import type { MoneyStore } from "./store.js";
import type { Invoice, InvoiceStatus, PaymentGateway, WatchState } from "./gateway.js";
import type { Mint, BlindedOutput, SignedOutput } from "./token.js";

export class Issuer {
  private readonly gateways: Record<string, PaymentGateway>;
  private readonly defaultMethod: string;

  constructor(
    private readonly store: MoneyStore,
    // A single gateway (the Bitcoin default) OR a method->gateway map. The map
    // form is how NYM is added alongside Bitcoin: both are live at once, and each
    // invoice remembers which one raised it via a "<method>:" prefix on its
    // provider_ref, so status/sweep route back to the right chain with no schema
    // change to the store.
    gateway: PaymentGateway | Record<string, PaymentGateway>,
    private readonly mint: Mint,
  ) {
    if (isGateway(gateway)) {
      this.gateways = { btc: gateway };
      this.defaultMethod = "btc";
    } else {
      this.gateways = gateway;
      this.defaultMethod = gateway.btc ? "btc" : Object.keys(gateway)[0];
    }
  }

  private gw(method: string): PaymentGateway {
    const g = this.gateways[method];
    if (!g) throw new Error(`payment method "${method}" is not available on this server`);
    return g;
  }

  /** Which payment methods this server can raise invoices for ("btc", "nyx"). */
  availableMethods(): string[] {
    return Object.keys(this.gateways);
  }

  get gatewayName(): string {
    return this.gw(this.defaultMethod).name;
  }
  get isFake(): boolean {
    return this.gw(this.defaultMethod).isFake;
  }

  /**
   * Raise an invoice. The TOKU amount is fixed HERE, not at settlement, so a
   * user always receives exactly what they were quoted regardless of what the
   * exchange rate does while they are paying.
   */
  async createInvoice(accountId: string, amountUsd: number, method = this.defaultMethod): Promise<Invoice> {
    const tiers = purchaseTiers();
    if (!tiers.includes(amountUsd)) {
      // Fixed amounts only, so every purchase looks like everyone else's — see
      // DEFAULT_PURCHASE_TIERS. A free-form amount would be a fingerprint.
      throw new Error(`purchases must be one of: ${tiers.map((t) => `$${t}`).join(", ")}`);
    }
    const id = randomUUID();
    const raised = await this.gw(method).createInvoice(amountUsd, id);
    const amountScrai = Math.floor(amountUsd * SCRAI_PER_USD);

    // provider_ref stays the gateway's OWN id; the method is recorded alongside
    // it so status()/sweep() route each invoice back to the chain that raised it.
    this.store.createInvoice({
      id,
      providerRef: raised.providerRef,
      accountId,
      amountUsd,
      amountScrai,
      payTo: raised.payTo,
      method,
      expiresAt: raised.expiresAt,
    });

    return { ...raised, id, amountScrai };
  }

  /**
   * Where is this invoice?
   *
   * Asks the gateway as well as our own record. A webhook that never arrived
   * must not leave a paying customer stuck, so polling can settle it too — and
   * because settlement is idempotent, both paths racing is harmless.
   */
  async status(
    invoiceId: string,
  ): Promise<{ status: InvoiceStatus; entitlement: number; watch?: WatchState } | null> {
    const inv = this.store.getInvoice(invoiceId);
    if (!inv) return null;

    const gateway = this.gw(inv.method);
    if (inv.status === "pending") {
      const remote = await gateway.checkStatus(inv.provider_ref).catch(() => "pending" as const);
      if (remote === "paid") this.store.settleInvoice(inv.provider_ref);
    }

    const now = this.store.getInvoice(invoiceId)!;
    // Live chain-watch health, when the gateway watches a chain (NYM).
    const watch = gateway.watchState?.();
    return {
      status: now.status,
      entitlement: this.store.entitlement(now.account_id),
      ...(watch ? { watch } : {}),
    };
  }

  /** Called by the webhook, and by polling. Safe to call repeatedly. */
  settle(providerRef: string): { credited: number; alreadySettled: boolean } | null {
    return this.store.settleInvoice(providerRef);
  }

  /**
   * Cancel a still-pending invoice the user abandoned. Marks it expired in the
   * store and tells the gateway to stop watching it. Idempotent and safe: a paid
   * invoice is left untouched (cancelInvoice only affects a pending row).
   */
  cancel(invoiceId: string): boolean {
    const inv = this.store.getInvoice(invoiceId);
    if (!inv || inv.status !== "pending") return false;
    const ok = this.store.cancelInvoice(invoiceId);
    if (ok) this.gw(inv.method).cancel?.(inv.provider_ref);
    return ok;
  }

  entitlement(accountId: string): number {
    return this.store.entitlement(accountId);
  }

  /**
   * Trade entitlement for blind-signed bearer tokens.
   *
   * The client sends BLINDED outputs; the issuer signs values it cannot read, so
   * the tokens it hands back cannot later be tied to this account. The account
   * signature (checked by the caller) authorises the TOTAL of the outputs.
   *
   * Order matters for the money invariant. Signing is pure — no state changes —
   * so we sign FIRST: a malformed output throws here, before any debit. Only if
   * every output signed do we debit, and only if the debit succeeds are the
   * signatures returned. So a failure can never both take entitlement AND
   * withhold the tokens, nor hand out tokens without taking entitlement.
   */
  withdraw(accountId: string, outputs: BlindedOutput[]): { keysetId: string; signatures: SignedOutput[] } {
    let total = 0;
    for (const o of outputs) {
      if (!this.mint.knows(o.amount)) throw new Error(`unknown denomination ${o.amount}`);
      total += o.amount;
    }
    if (!Number.isInteger(total) || total <= 0) throw new Error("nothing to withdraw");

    const signatures = outputs.map((o) => this.mint.sign(o)); // pure; throws before any debit
    if (!this.store.withdrawEntitlement(accountId, total)) {
      throw new Error(`not enough entitlement: account holds ${this.store.entitlement(accountId)} TOKU`);
    }
    return { keysetId: this.mint.keysetId(), signatures };
  }

  /** The public keyset, for the `keys` response. */
  publicKeys() {
    return { keysetId: this.mint.keysetId(), keys: this.mint.publicKeys() };
  }

  /** Verify one unblinded proof under the mint's key for its denomination. */
  verifyProof(proof: { amount: number; secret: string; C: string }): boolean {
    return this.mint.verify(proof);
  }

  /**
   * Re-check every pending invoice against the gateway.
   *
   * This is what makes a slow confirmation safe. BTCPay keeps watching an
   * invoice long after its rate-lock window closes, so a payment broadcast in
   * time will settle there whenever it confirms — but only if somebody asks.
   * The client stops asking when it gives up or is closed, so the server has to
   * take over. Without this, an on-chain payment that confirms after ten
   * minutes is money taken and never credited.
   *
   * Settlement is idempotent, so this racing with a polling client is harmless.
   */
  async sweep(): Promise<{ checked: number; settled: number }> {
    const pending = this.store.pendingInvoices();
    let settled = 0;
    for (const inv of pending) {
      const remote = await this.gw(inv.method).checkStatus(inv.provider_ref).catch(() => "pending" as const);
      if (remote === "paid") {
        const res = this.store.settleInvoice(inv.provider_ref);
        if (res && !res.alreadySettled) settled += 1;
      }
    }
    return { checked: pending.length, settled };
  }

  /** Housekeeping — pending invoices past their window are dead. */
  expireStale(): number {
    return this.store.expireInvoices();
  }
}

/** A PaymentGateway has a string `name`; a map of them does not. */
function isGateway(g: PaymentGateway | Record<string, PaymentGateway>): g is PaymentGateway {
  return typeof (g as PaymentGateway).name === "string";
}
