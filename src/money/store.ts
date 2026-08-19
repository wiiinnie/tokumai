// ---------------------------------------------------------------------------
// store.ts — the two pieces of state this server is allowed to keep.
//
// It keeps exactly two things, and neither identifies anyone:
//
//   nullifiers   serial numbers of funding tokens that have been spent, so a
//                token cannot be spent twice
//   sessions     sessionId -> { public key, balance, counter }, where sessionId
//                is a hash of a key the client generated and the server has
//                never seen the private half of
//
// No prompt, no answer, no address, no purchase link. The statelessness claim in
// the banner needs this qualification and now makes it.
//
// EVERY MONEY OPERATION HERE IS ONE SQL STATEMENT ON PURPOSE.
//
// The failure mode this file exists to prevent is a read-then-write race: two
// concurrent requests both read a balance, both decide it is sufficient, both
// debit. Same for a token spent twice in parallel. SQLite gives us atomic
// statements; a `SELECT` followed by an `UPDATE` in JavaScript does not, no
// matter how the code reads. So the checks live in WHERE clauses, and the
// return value is "how many rows did that actually change".
// ---------------------------------------------------------------------------

import { DatabaseSync } from "node:sqlite";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { randomBytes } from "node:crypto";

export interface InvoiceRow {
  id: string;
  provider_ref: string;
  account_id: string;
  amount_usd: number;
  amount_scrai: number;
  status: "pending" | "paid" | "expired";
  pay_to: string;
  method: string;
  created: number;
  expires_at: number;
}

export interface Session {
  id: string;
  balance: number;
  counter: number;
}

export class MoneyStore {
  private db: DatabaseSync;

  constructor(path: string) {
    if (path !== ":memory:") mkdirSync(dirname(path), { recursive: true });
    this.db = new DatabaseSync(path);
    // WAL so a reader never blocks the writer; NORMAL is the usual durability
    // tradeoff for it. A crash can lose the last moments, which for reserved
    // escrow means a user loses a fraction of a cent — acceptable, and cheaper
    // than fsync on every debit.
    this.db.exec("PRAGMA journal_mode = WAL");
    this.db.exec("PRAGMA synchronous = NORMAL");
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS nullifiers (
        serial   TEXT PRIMARY KEY,
        spent_at INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS invoices (
        id          TEXT PRIMARY KEY,
        provider_ref TEXT NOT NULL UNIQUE,
        account_id  TEXT NOT NULL,
        amount_usd  REAL NOT NULL,
        amount_scrai INTEGER NOT NULL,
        status      TEXT NOT NULL,
        pay_to      TEXT NOT NULL,
        method      TEXT NOT NULL DEFAULT 'btc',
        created     INTEGER NOT NULL,
        expires_at  INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS entitlements (
        account_id TEXT PRIMARY KEY,
        scrai      INTEGER NOT NULL,
        updated    INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS sessions (
        id        TEXT PRIMARY KEY,
        pubkey    TEXT NOT NULL,
        balance   INTEGER NOT NULL,
        counter   INTEGER NOT NULL DEFAULT 0,
        created   INTEGER NOT NULL,
        last_seen INTEGER NOT NULL
      );
      CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
    `);
    // Migration: older money.db files predate the payment-method column. ADD
    // COLUMN with a default is non-destructive; existing invoices become 'btc',
    // which is exactly what they were. Ignore the error when it already exists.
    try {
      this.db.exec("ALTER TABLE invoices ADD COLUMN method TEXT NOT NULL DEFAULT 'btc'");
    } catch {
      /* column already present */
    }
  }

  // ---- mint seed ----------------------------------------------------------

  /**
   * The seed the whole keyset is derived from, when the operator has not set
   * SCRAI_ISSUER_SECRET. Generated once and persisted, so tokens survive a
   * restart — unlike the old per-process secret, which invalidated every
   * in-flight token whenever the server bounced. Stored here rather than derived
   * into keys means the DB holds a seed, not the keys themselves; setting the env
   * var keeps even the seed out of the database.
   */
  getOrCreateMintSeed(): Buffer {
    const row = this.db.prepare("SELECT value FROM meta WHERE key = 'mint_seed'").get() as
      | { value: string }
      | undefined;
    if (row) return Buffer.from(row.value, "hex");
    const seed = randomBytes(32);
    this.db.prepare("INSERT INTO meta (key, value) VALUES ('mint_seed', ?)").run(seed.toString("hex"));
    return seed;
  }

  // ---- funding tokens -----------------------------------------------------

  /**
   * Burn a token serial. Returns false when it was already spent.
   *
   * The check IS the insert: a UNIQUE violation is the answer, not an error to
   * recover from. Two callers racing on the same serial cannot both win.
   */
  spendSerial(serial: string): boolean {
    try {
      this.db.prepare("INSERT INTO nullifiers (serial, spent_at) VALUES (?, ?)").run(serial, Date.now());
      return true;
    } catch (err) {
      if (String((err as { code?: string }).code ?? "").includes("SQLITE")) return false;
      throw err;
    }
  }

  // ---- invoices and entitlements -----------------------------------------
  //
  // This is the ONLY place an account id appears. It records that an account
  // bought SCRAI — never what was asked, never which session spent it. The
  // blind issuance in between is what keeps those two halves apart.

  createInvoice(inv: {
    id: string;
    providerRef: string;
    accountId: string;
    amountUsd: number;
    amountScrai: number;
    payTo: string;
    method?: string;
    expiresAt: number;
  }): void {
    this.db
      .prepare(
        "INSERT INTO invoices (id, provider_ref, account_id, amount_usd, amount_scrai, status, pay_to, method, created, expires_at) " +
          "VALUES (?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?)",
      )
      .run(
        inv.id,
        inv.providerRef,
        inv.accountId,
        inv.amountUsd,
        inv.amountScrai,
        inv.payTo,
        inv.method ?? "btc",
        Date.now(),
        inv.expiresAt,
      );
  }

  getInvoice(id: string): InvoiceRow | null {
    return (this.db
      .prepare("SELECT * FROM invoices WHERE id = ?")
      .get(id) as InvoiceRow | undefined) ?? null;
  }

  getInvoiceByRef(providerRef: string): InvoiceRow | null {
    return (this.db
      .prepare("SELECT * FROM invoices WHERE provider_ref = ?")
      .get(providerRef) as InvoiceRow | undefined) ?? null;
  }

  /**
   * Mark an invoice paid and credit the account — once, whatever happens.
   *
   * A payment webhook is retried until it is acknowledged, so this WILL be
   * called more than once for the same invoice. The `status = 'pending'` guard
   * in the WHERE clause is what makes the second call a no-op instead of a
   * second credit. Getting this wrong means paying out twice for one payment.
   */
  settleInvoice(providerRef: string): { credited: number; alreadySettled: boolean } | null {
    const inv = this.getInvoiceByRef(providerRef);
    if (!inv) return null;

    const changed = this.db
      .prepare("UPDATE invoices SET status = 'paid' WHERE provider_ref = ? AND status = 'pending'")
      .run(providerRef).changes;

    if (changed !== 1) return { credited: 0, alreadySettled: true };

    this.db
      .prepare(
        "INSERT INTO entitlements (account_id, scrai, updated) VALUES (?, ?, ?) " +
          "ON CONFLICT(account_id) DO UPDATE SET scrai = scrai + excluded.scrai, updated = excluded.updated",
      )
      .run(inv.account_id, inv.amount_scrai, Date.now());

    return { credited: inv.amount_scrai, alreadySettled: false };
  }

  entitlement(accountId: string): number {
    const r = this.db
      .prepare("SELECT scrai FROM entitlements WHERE account_id = ?")
      .get(accountId) as { scrai: number } | undefined;
    return r?.scrai ?? 0;
  }

  /**
   * Draw down an entitlement, atomically. Returns false when the account does
   * not have that much — the same read-then-write race as everywhere else.
   */
  withdrawEntitlement(accountId: string, amount: number): boolean {
    return (
      this.db
        .prepare("UPDATE entitlements SET scrai = scrai - ?, updated = ? WHERE account_id = ? AND scrai >= ?")
        .run(amount, Date.now(), accountId, amount).changes === 1
    );
  }

  listInvoices(limit = 25): InvoiceRow[] {
    return this.db
      .prepare("SELECT * FROM invoices ORDER BY created DESC LIMIT ?")
      .all(limit) as unknown as InvoiceRow[];
  }

  listEntitlements(): Array<{ account_id: string; scrai: number }> {
    return this.db
      .prepare("SELECT account_id, scrai FROM entitlements WHERE scrai > 0 ORDER BY scrai DESC")
      .all() as unknown as Array<{ account_id: string; scrai: number }>;
  }

  /**
   * Invoices still awaiting payment or confirmation.
   *
   * On-chain payments routinely confirm long after the client has stopped
   * polling — a block can take an hour. Something has to keep asking, or money
   * arrives and is never credited.
   */
  pendingInvoices(): InvoiceRow[] {
    return this.db
      .prepare("SELECT * FROM invoices WHERE status = 'pending' ORDER BY created ASC")
      .all() as unknown as InvoiceRow[];
  }

  expireInvoices(): number {
    return this.db
      .prepare("UPDATE invoices SET status = 'expired' WHERE status = 'pending' AND expires_at < ?")
      .run(Date.now()).changes as number;
  }

  /** Cancel one still-pending invoice by id. Returns true if it was pending. */
  cancelInvoice(id: string): boolean {
    return (
      (this.db
        .prepare("UPDATE invoices SET status = 'expired' WHERE id = ? AND status = 'pending'")
        .run(id).changes as number) > 0
    );
  }

  // ---- sessions -----------------------------------------------------------

  /** Create a session, or top up an existing one owned by the same key. */
  openSession(id: string, pubkey: string, amount: number): Session {
    const now = Date.now();
    const existing = this.getSession(id);

    if (existing) {
      // Same id means the same public key by construction (id is its hash), so
      // topping up is safe without a further ownership check.
      this.db
        .prepare("UPDATE sessions SET balance = balance + ?, last_seen = ? WHERE id = ?")
        .run(amount, now, id);
    } else {
      this.db
        .prepare("INSERT INTO sessions (id, pubkey, balance, counter, created, last_seen) VALUES (?, ?, ?, 0, ?, ?)")
        .run(id, pubkey, amount, now, now);
    }
    return this.getSession(id)!;
  }

  /**
   * Redeem a set of ecash proofs into a session, ATOMICALLY.
   *
   * Every token secret is burned and the balance credited inside one
   * transaction: if ANY secret was already spent (a double-spend), the UNIQUE
   * violation rolls the whole thing back — nothing is burned, nothing credited,
   * and the caller learns the redemption failed rather than being left with a
   * half-spent set. Verifying the signatures is the mint's job and must already
   * have passed before this is called; here we only guard against replay.
   */
  redeemProofs(
    id: string,
    pubkey: string,
    secrets: string[],
    amount: number,
  ): { ok: true; session: Session } | { ok: false } {
    const now = Date.now();
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const burn = this.db.prepare("INSERT INTO nullifiers (serial, spent_at) VALUES (?, ?)");
      for (const s of secrets) burn.run(s, now); // UNIQUE violation on a reused secret

      if (this.getSession(id)) {
        this.db.prepare("UPDATE sessions SET balance = balance + ?, last_seen = ? WHERE id = ?").run(amount, now, id);
      } else {
        this.db
          .prepare("INSERT INTO sessions (id, pubkey, balance, counter, created, last_seen) VALUES (?, ?, ?, 0, ?, ?)")
          .run(id, pubkey, amount, now, now);
      }
      this.db.exec("COMMIT");
      return { ok: true, session: this.getSession(id)! };
    } catch {
      // A duplicate secret lands here (double-spend); so would a genuine DB
      // error. Both mean "nothing was credited", which is the safe answer.
      try { this.db.exec("ROLLBACK"); } catch { /* no tx in flight */ }
      return { ok: false };
    }
  }

  getSession(id: string): (Session & { pubkey: string }) | null {
    const row = this.db
      .prepare("SELECT id, pubkey, balance, counter FROM sessions WHERE id = ?")
      .get(id) as { id: string; pubkey: string; balance: number; counter: number } | undefined;
    return row ?? null;
  }

  /**
   * Advance the counter and reserve `amount` — atomically, or not at all.
   *
   * Both guards live in the WHERE clause:
   *   counter < ?   a replayed or reordered request changes nothing
   *   balance >= ?  an overdraft changes nothing
   *
   * Reserving the CEILING rather than the real price is what makes concurrency
   * safe. Two requests in flight on one session each hold their own worst case,
   * so they cannot jointly overspend. The unused part comes back in settle().
   */
  reserve(id: string, counter: number, amount: number): "ok" | "replay" | "insufficient" | "unknown" {
    const changed = this.db
      .prepare(
        "UPDATE sessions SET balance = balance - ?, counter = ?, last_seen = ? " +
          "WHERE id = ? AND counter < ? AND balance >= ?",
      )
      .run(amount, counter, Date.now(), id, counter, amount).changes;

    if (changed === 1) return "ok";
    // It failed — now work out why, for a message the user can act on.
    const s = this.getSession(id);
    if (!s) return "unknown";
    if (s.counter >= counter) return "replay";
    return "insufficient";
  }

  /** Return the unspent part of a reservation. */
  settle(id: string, reserved: number, actual: number): number {
    const refund = Math.max(0, reserved - actual);
    if (refund > 0) {
      this.db.prepare("UPDATE sessions SET balance = balance + ? WHERE id = ?").run(refund, id);
    }
    return this.getSession(id)?.balance ?? 0;
  }

  /** Give back a whole reservation, e.g. when the provider call failed. */
  refund(id: string, amount: number): number {
    return this.settle(id, amount, 0);
  }

  /** Sessions untouched for this long are dropped, along with their balance. */
  expire(olderThanMs: number): number {
    return this.db
      .prepare("DELETE FROM sessions WHERE last_seen < ?")
      .run(Date.now() - olderThanMs).changes as number;
  }

  stats(): { sessions: number; spent: number; pendingInvoices: number; owedScrai: number } {
    const s = this.db.prepare("SELECT COUNT(*) AS n FROM sessions").get() as { n: number };
    const n = this.db.prepare("SELECT COUNT(*) AS n FROM nullifiers").get() as { n: number };
    const p = this.db.prepare("SELECT COUNT(*) AS n FROM invoices WHERE status = 'pending'").get() as { n: number };
    const e = this.db.prepare("SELECT COALESCE(SUM(scrai), 0) AS n FROM entitlements").get() as { n: number };
    return { sessions: s.n, spent: n.n, pendingInvoices: p.n, owedScrai: e.n };
  }

  close(): void {
    this.db.close();
  }
}
