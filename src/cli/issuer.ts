#!/usr/bin/env node
// ---------------------------------------------------------------------------
// scrai-issuer — operator-side tools for the payment layer.
//
// THIS FILE IS THE DEBUG LAYER. `settle` exists only because no real money is
// moving yet: it does by hand what a BTCPay webhook will do by itself. When the
// real gateway lands, this command is deleted and nothing else changes — the
// issuer already treats "settled" as an event with one source.
//
// It talks to the same SQLite file the server uses, so it must be run on the
// same machine. That is fine for a dev tool and would not be for a real one.
// ---------------------------------------------------------------------------

import { MoneyStore } from "../money/store.js";
import { SCRAI_PER_USD } from "../billing.js";

const db = (process.env.MONEY_DB ?? process.env.SCRAI_MONEY_DB) ?? "./data/money.db";
const money = new MoneyStore(db);
const scrai = (n: number) => n.toLocaleString("en-US");

function usage(): void {
  console.log(`
scrai-issuer — payment layer, operator side

  invoices              list invoices and their state
  settle <provider-ref> mark one paid  ← DEV ONLY, stands in for a webhook
  accounts              entitlements not yet withdrawn
  pending               invoices still awaiting payment or confirmation
  expire                mark overdue invoices expired

Database: ${db}
`);
}

const [cmd, arg] = process.argv.slice(2);

switch (cmd) {
  case "invoices": {
    const rows = money.listInvoices(25);
    if (!rows.length) {
      console.log("no invoices yet");
      break;
    }
    console.log("");
    console.log(`  ${"STATUS".padEnd(9)} ${"USD".padStart(8)} ${"TOKU".padStart(12)}  ${"ACCOUNT".padEnd(10)} PROVIDER REF`);
    console.log(`  ${"─".repeat(9)} ${"─".repeat(8)} ${"─".repeat(12)}  ${"─".repeat(10)} ${"─".repeat(28)}`);
    for (const r of rows) {
      console.log(
        `  ${r.status.padEnd(9)} ${r.amount_usd.toFixed(2).padStart(8)} ${scrai(r.amount_scrai).padStart(12)}` +
          `  ${r.account_id.slice(0, 8).padEnd(10)} ${r.provider_ref}`,
      );
    }
    console.log("");
    break;
  }

  case "settle": {
    if (!arg) {
      console.error("usage: settle <provider-ref>   (see: issuer invoices)");
      process.exit(1);
    }
    const res = money.settleInvoice(arg);
    if (!res) {
      console.error(`no invoice with provider ref ${arg}`);
      process.exit(1);
    }
    if (res.alreadySettled) {
      // Not an error: a real webhook is retried, and settling twice must be a
      // no-op rather than a second credit. Saying so out loud is the point.
      console.log(`${arg} was already settled — nothing credited (this is correct)`);
      break;
    }
    console.log(`settled ${arg} — credited ${scrai(res.credited)} TOKU (USD ${(res.credited / SCRAI_PER_USD).toFixed(2)})`);
    break;
  }

  case "accounts": {
    const rows = money.listEntitlements();
    if (!rows.length) {
      console.log("no outstanding entitlements");
      break;
    }
    console.log("");
    for (const r of rows) {
      console.log(`  ${r.account_id.slice(0, 8)}…  ${scrai(r.scrai).padStart(12)} TOKU awaiting withdrawal`);
    }
    console.log("");
    break;
  }

  case "pending": {
    const rows = money.pendingInvoices();
    if (!rows.length) { console.log("nothing pending"); break; }
    console.log("");
    for (const r of rows) {
      const age = Math.round((Date.now() - r.created) / 60000);
      console.log(`  ${r.provider_ref}  ${r.amount_usd.toFixed(2)} USD  raised ${age}m ago`);
    }
    console.log("\n  The server re-checks these every 2 minutes; a slow on-chain\n  confirmation is credited whenever it lands.\n");
    break;
  }

  case "expire": {
    console.log(`${money.expireInvoices()} invoice(s) marked expired`);
    break;
  }

  default:
    usage();
}

money.close();
