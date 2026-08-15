// ---------------------------------------------------------------------------
// billing.js — SCRAI on the client side.
//
// Pure helpers plus a ticketbook. All DOM lives in index.html, same as every
// other concern in this UI; this module never touches the document.
//
// Two things it deliberately does not do:
//   - it never computes a price from tokens. The server sends a finished price;
//     the margin is not on this side and cannot be reconstructed from here.
//   - it never displays costScrai. That number rides along in the frame for
//     our own accounting and stays out of the interface.
//
// The balance is bookkeeping, not a gate. Until zk-nym tickets replace the
// dev-ticket stub, a patched client can hand itself SCRAI — the real limit is
// that the server holds the API key.
// ---------------------------------------------------------------------------

const fmt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });

/** "1,204" — SCRAI amounts are whole numbers by construction. */
export function formatScrai(scrai) {
  return fmt.format(Math.max(0, Math.round(scrai || 0)));
}

/** "612 in · 208 out · 96 thinking · 400 cached" — the quiet half of the footer. */
export function formatTokens(usage) {
  if (!usage) return "";
  const input =
    (usage.inputTokens || 0) + (usage.cachedInputTokens || 0) + (usage.audioInputTokens || 0);
  const parts = [`${fmt.format(input)} in`, `${fmt.format(usage.outputTokens || 0)} out`];
  if (usage.thoughtTokens) parts.push(`${fmt.format(usage.thoughtTokens)} thinking`);
  if (usage.cachedInputTokens) parts.push(`${fmt.format(usage.cachedInputTokens)} cached`);
  return parts.join(" · ");
}

/**
 * Caveats worth showing next to a price, if any.
 *
 * "estimated"      → the provider reported no token counts and the server fell
 *                    back to a character estimate. Expect round numbers.
 * "unlisted model" → no entry in pricing.json, so the conservative default rate
 *                    applied. The price is real but higher than it should be.
 */
export function billingNotes(frame) {
  if (!frame) return "";
  const notes = [];
  if (frame.estimated) notes.push("estimated");
  if (frame.fallbackPrice) notes.push("unlisted model");
  return notes.length ? `(${notes.join(", ")})` : "";
}

/** Tooltip text: which model, which price table. Useful when a price looks off. */
export function billingDetail(frame) {
  if (!frame) return "";
  return `${frame.model}\npricing table ${frame.pricingVersion}`;
}

/**
 * In-memory ticketbook.
 *
 *   const tb = createTicketbook({ initial: 10000, onChange: renderBal });
 *   tb.charge(frame.priceScrai);
 *   tb.add(5000);
 *
 * No persistence: the balance is per-session, like the rest of the ticket stub.
 * When zk-nym tickets land, `add()` becomes "redeem a ticket" and the balance
 * moves behind the vault.
 */
export function createTicketbook({ initial = 0, onChange = null } = {}) {
  let balance = initial;
  let spent = 0;

  const notify = () => {
    if (onChange) onChange({ balance, spent });
  };

  return {
    get balance() {
      return balance;
    },
    get spent() {
      return spent;
    },

    /** Enough for one more turn? Cosmetic until tickets are enforced server-side. */
    canSpend(SCRAI = 1) {
      return balance >= SCRAI;
    },

    /** Book a finished turn. Never goes below zero. */
    charge(priceScrai) {
      const amount = Math.max(0, Math.round(priceScrai || 0));
      balance = Math.max(0, balance - amount);
      spent += amount;
      notify();
      return balance;
    },

    /** Dev refill today; redeeming a zk-nym ticket later. */
    add(SCRAI) {
      balance += Math.max(0, Math.round(SCRAI || 0));
      notify();
      return balance;
    },
  };
}
