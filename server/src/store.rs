// store.rs — durable state for the server: session balances + double-spend records.
//
// A tiny key→JSON-blob table (bundled SQLite, no system dependency). The core stores
// (SessionStore, QuorumStore) already snapshot themselves to JSON; this just persists
// those blobs so a restart keeps balances AND the double-spend history. The server
// re-saves a blob only when its store's `revision()` advances, so the frequent path
// (chat charging one session) writes only the small session blob, never the quorum.
//
// This blob scheme is fine at bring-up scale. When the nullifier set grows large,
// swap the quorum blob for per-row tables (serials / offenders) without touching core.

use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;

/// What `voucher_burn` did. Each case is a different sentence to the buyer.
pub enum VoucherBurn {
    Burned { toku: u64 },
    /// Already redeemed by THIS account — a retry after a lost reply. Success.
    AlreadyYours,
    /// Redeemed by someone else. The code is spent.
    Spent,
    /// Refunded before it was used.
    Void,
    Unknown,
}

/// What `voucher_void_by_invoice` did.
pub enum VoucherVoid {
    Voided(usize),
    AlreadySpent,
    Unknown,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the state database.
    pub fn open(path: &Path) -> Result<Store, String> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
            [],
        )
        .map_err(|e| e.to_string())?;
        // Double-spend records, append-only: one row per accepted payment (its JSON incl.
        // the payment proof, needed to PROVE a later double-spend). `coins` is for the
        // admin's "coins redeemed". The small rest of the quorum lives in kv "quorum_meta".
        conn.execute(
            "CREATE TABLE IF NOT EXISTS quorum_records (\
               idx INTEGER PRIMARY KEY,\
               coins INTEGER NOT NULL DEFAULT 0,\
               v TEXT NOT NULL)",
            [],
        )
        .map_err(|e| e.to_string())?;
        // Per-UTC-day activity counters — the ONLY timestamped data the server keeps.
        // Aggregate-only (no account/session ids, no content): totals for the admin view.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS daily (\
               day TEXT PRIMARY KEY,\
               prompts   INTEGER NOT NULL DEFAULT 0,\
               spent     INTEGER NOT NULL DEFAULT 0,\
               purchases INTEGER NOT NULL DEFAULT 0,\
               purchased INTEGER NOT NULL DEFAULT 0,\
               cost      INTEGER NOT NULL DEFAULT 0)",
            [],
        )
        .map_err(|e| e.to_string())?;
        // Migration for tables created before the provider-cost column existed. `spent` is
        // retail (what users chatted); `cost` is the raw provider price we paid (no margin),
        // so profit = spent − cost. Ignore the error when the column is already there.
        let _ = conn.execute("ALTER TABLE daily ADD COLUMN cost INTEGER NOT NULL DEFAULT 0", []);
        // Peak number of distinct clients served in parallel that day (a MAX, not a sum) —
        // the capacity signal for the single Nym client / provider slots (inflight.rs).
        let _ = conn.execute("ALTER TABLE daily ADD COLUMN peak_clients INTEGER NOT NULL DEFAULT 0", []);
        // Kept so old rows still parse; no longer written or shown (2026-09-07). It counted
        // distinct SURB reply TAGS in a 60-second window, which is not a count of people: one
        // app gets several tags, and every catalog fetch, ping and invoice poll carried one
        // while never being a user. It regularly read higher than the day's user count.
        let _ = conn.execute("ALTER TABLE daily ADD COLUMN peak_1m INTEGER NOT NULL DEFAULT 0", []);
        // Most DIFFERENT paying SESSIONS seen within the same hour that day — the same
        // identity `users` counts, so the two are comparable by construction: `peak_1h` can
        // never exceed the day's `users`. This is "how busy was the busiest hour".
        let _ = conn.execute("ALTER TABLE daily ADD COLUMN peak_1h INTEGER NOT NULL DEFAULT 0", []);
        // Vouchers: credit bought on the website and carried into an app as a code.
        //
        // A REAL TABLE, not part of the pay snapshot, and for a specific reason: the snapshot
        // is held in memory and written whole, so a second process (scrai-admin voiding a
        // refunded code) could not edit it without the server overwriting the change on its
        // next save. Rows here are touched with SQL, and SQLite arbitrates.
        //
        // Only the HASH of the code is stored. The server can verify one and burn it; it
        // cannot produce one. So a lost code can be refunded but never recovered — and
        // nobody can talk a code out of support, because support has none either.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS vouchers (\
               hash TEXT PRIMARY KEY,\
               toku INTEGER NOT NULL,\
               invoice TEXT NOT NULL,\
               created_at INTEGER NOT NULL,\
               account TEXT,\
               redeemed_at INTEGER,\
               credited_at INTEGER,\
               void_at INTEGER)",
            [],
        )
        .map_err(|e| e.to_string())?;
        // Found by invoice when a refund is decided: a refund starts with somebody holding a
        // receipt, never with the code.
        // UNIQUE, not just an index: one voucher per invoice. A reloaded page must not be
        // able to mint a second code for money that was paid once.
        let _ = conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS vouchers_by_invoice ON vouchers (invoice)", []);

        // The channel between the faucet (clearnet, serves /pay) and the server (mixnet only,
        // owns the payment rails). A table rather than a port: no new listener on the box
        // that holds the mint, no second Mollie client, and an order survives either process
        // dying — it is simply still there on the next tick. See docs/vouchers.md.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS web_orders (\
               id TEXT PRIMARY KEY,\
               usd INTEGER NOT NULL,\
               method TEXT NOT NULL,\
               consent TEXT NOT NULL,\
               created_at INTEGER NOT NULL,\
               invoice TEXT,\
               pay_json TEXT,\
               paid_at INTEGER,\
               cancelled_at INTEGER,\
               error TEXT)",
            [],
        )
        .map_err(|e| e.to_string())?;
        // When the server last handed this order to the payment gateway. Without it the
        // one-second tick re-dispatches an order for as long as the raise takes, and the
        // LAST raise to answer overwrites the row — so a buyer who already had the FIRST
        // one's address on screen pays a memo the invoice no longer expects. See
        // `web_orders_pending`.
        let _ = conn.execute("ALTER TABLE web_orders ADD COLUMN raising_at INTEGER", []);
        // The voucher code IN THE CLEAR, from minting until the buyer confirms they have
        // written it down (or until `web_orders_forget_codes` sweeps it). We used to keep
        // only the fingerprint and hand the plaintext to exactly one HTTP response — which
        // is right against double-minting and fatal against a dropped reply: the buyer had
        // paid and nothing on earth could produce their code again. Holding it for minutes
        // trades "the customer loses their money" for "we held a bearer code briefly", and
        // that is the better trade in every direction (audit 2026-09-08, H2).
        let _ = conn.execute("ALTER TABLE web_orders ADD COLUMN code TEXT", []);
        let _ = conn.execute("ALTER TABLE web_orders ADD COLUMN code_at INTEGER", []);

        // Distinct paying sessions per UTC day ("users"): one row per (day, hashed session
        // id), so COUNT(*) per day is the number of different sessions that chatted. The
        // hash (sha256, 16 hex) keeps raw session ids out of the metrics table; rows older
        // than 90 days are pruned on write. `peak_clients` is a MAX of simultaneous
        // clients — this is the count of different ones over the whole day.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS daily_users (\
               day TEXT NOT NULL,\
               sid TEXT NOT NULL,\
               PRIMARY KEY (day, sid))",
            [],
        )
        .map_err(|e| e.to_string())?;
        // Per-UTC-day × model counters (which models are actually used, what they cost).
        // Same privacy shape as `daily`: aggregates only, no account/session ids, no content.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS daily_model (\
               day     TEXT NOT NULL,\
               model   TEXT NOT NULL,\
               prompts INTEGER NOT NULL DEFAULT 0,\
               spent   INTEGER NOT NULL DEFAULT 0,\
               cost    INTEGER NOT NULL DEFAULT 0,\
               PRIMARY KEY (day, model))",
            [],
        )
        .map_err(|e| e.to_string())?;
        Ok(Store { conn })
    }

    /// Add to today's per-model counters (upsert the day×model row). Best-effort like
    /// `bump_daily`: a metrics write must never break request handling.
    pub fn bump_daily_model(&self, day: &str, model: &str, prompts: u64, spent: u64, cost: u64) {
        let _ = self.conn.execute(
            "INSERT INTO daily_model (day, model, prompts, spent, cost) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(day, model) DO UPDATE SET \
               prompts = prompts + ?3, \
               spent   = spent   + ?4, \
               cost    = cost    + ?5",
            params![day, model, prompts as i64, spent as i64, cost as i64],
        );
    }

    /// Add to today's activity counters (upsert the day row). Cheap, best-effort:
    /// a metrics write must never break request handling, so failures are swallowed.
    pub fn bump_daily(&self, day: &str, prompts: u64, spent: u64, cost: u64, purchases: u64, purchased: u64) {
        let _ = self.conn.execute(
            "INSERT INTO daily (day, prompts, spent, cost, purchases, purchased) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(day) DO UPDATE SET \
               prompts   = prompts   + ?2, \
               spent     = spent     + ?3, \
               cost      = cost      + ?4, \
               purchases = purchases + ?5, \
               purchased = purchased + ?6",
            params![day, prompts as i64, spent as i64, cost as i64, purchases as i64, purchased as i64],
        );
    }

    /// The metrics identity of a session: sha256, first 8 bytes as hex. Raw session ids
    /// never reach the metrics tables, and the in-memory hour window keys on the same value
    /// so "users" and "busiest hour" count the same population.
    pub fn user_key(session_id: &str) -> String {
        scrai_core::auth::sha256(&[session_id.as_bytes()]).iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    /// Remember that `session_id` chatted on `day` (idempotent). Best-effort like `bump_daily`.
    pub fn note_user(&self, day: &str, session_id: &str) {
        let h = Self::user_key(session_id);
        let _ = self.conn.execute("INSERT OR IGNORE INTO daily_users (day, sid) VALUES (?1, ?2)", params![day, h]);
        // keep the table small: 90 days is plenty for the admin's 12-day view
        if let Some(cut) = day_minus(day, 90) {
            let _ = self.conn.execute("DELETE FROM daily_users WHERE day < ?1", params![cut]);
        }
    }

    /// Raise today's peak-simultaneous-clients mark to `n` if it is higher. Best-effort
    /// like `bump_daily`.
    pub fn bump_peak(&self, day: &str, n: usize) {
        let _ = self.conn.execute(
            "INSERT INTO daily (day, peak_clients) VALUES (?1, ?2) \
             ON CONFLICT(day) DO UPDATE SET peak_clients = MAX(peak_clients, ?2)",
            params![day, n as i64],
        );
    }

    /// Raise the day's busiest-hour session count (a MAX, like `bump_peak`).
    pub fn bump_hour_peak(&self, day: &str, n: usize) {
        let _ = self.conn.execute(
            "INSERT INTO daily (day, peak_1h) VALUES (?1, ?2) \
             ON CONFLICT(day) DO UPDATE SET peak_1h = MAX(peak_1h, ?2)",
            params![day, n as i64],
        );
    }

    // ---- web orders (the faucet ↔ server channel) ---------------------------------------

    /// The faucet books an order. Nothing is raised yet — the server picks it up.
    pub fn web_order_new(&self, id: &str, usd: u32, method: &str, consent: &str, now: u64) -> bool {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO web_orders (id, usd, method, consent, created_at) VALUES (?1,?2,?3,?4,?5)",
                params![id, usd as i64, method, consent, now as i64],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// How long a dispatched order stays claimed before another tick may retry it. Longer
    /// than any gateway call we are willing to wait for (QUEUE_WAIT + the provider timeout),
    /// short enough that a raise lost to a crash is retried while the buyer is still there.
    const RAISE_CLAIM_MS: u64 = 90_000;

    /// Orders the server has not answered yet, CLAIMED in the same call. Bounded: a tick
    /// must stay cheap.
    ///
    /// The claim is the point. The row is only cleared when the gateway answers, which can
    /// take seconds — so a bare `invoice IS NULL` re-dispatches the same order on every
    /// one-second tick. Each raise writes back over the row, and the page renders the first
    /// answer it sees: the buyer ends up looking at address/memo A while the invoice has
    /// been overwritten to expect B. They pay, nothing matches, and the code never arrives.
    /// One claim per order per `RAISE_CLAIM_MS` closes that; the timeout is what lets a
    /// raise that died with the process be retried at all.
    pub fn web_orders_pending(&self, max: usize) -> Vec<(String, u32, String, String)> {
        let mut out: Vec<(String, u32, String, String)> = Vec::new();
        let now = crate::pay::now_ms();
        let cutoff = now.saturating_sub(Self::RAISE_CLAIM_MS) as i64;
        if let Ok(mut st) = self.conn.prepare(
            "SELECT id, usd, method, consent FROM web_orders \
             WHERE invoice IS NULL AND error IS NULL AND cancelled_at IS NULL \
               AND (raising_at IS NULL OR raising_at < ?2) \
             ORDER BY created_at LIMIT ?1",
        ) {
            if let Ok(rows) = st.query_map(params![max as i64, cutoff], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as u32,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            }) {
                out = rows.flatten().collect();
            }
        }
        for (id, _, _, _) in &out {
            let _ = self
                .conn
                .execute("UPDATE web_orders SET raising_at = ?2 WHERE id = ?1", params![id, now as i64]);
        }
        out
    }

    /// The server answers: either an invoice and what to show, or why not.
    pub fn web_order_answer(&self, id: &str, invoice: Option<&str>, pay_json: Option<&str>, error: Option<&str>) {
        let _ = self.conn.execute(
            "UPDATE web_orders SET invoice = ?2, pay_json = ?3, error = ?4 WHERE id = ?1",
            params![id, invoice, pay_json, error],
        );
    }

    /// Orders with an invoice that has not been seen paid yet — the server reflects the
    /// paywall's own view into this table so the faucet never has to parse the pay snapshot.
    pub fn web_orders_awaiting_payment(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare(
            "SELECT id, invoice FROM web_orders WHERE invoice IS NOT NULL AND paid_at IS NULL",
        ) {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                out = rows.flatten().collect();
            }
        }
        out
    }

    /// The buyer pressed cancel. The invoice itself lives in the pay snapshot, so the
    /// server cancels it on its next beat — the faucet only records the wish.
    pub fn web_order_cancel(&self, id: &str, now: u64) -> bool {
        self.conn
            .execute(
                "UPDATE web_orders SET cancelled_at = ?2 \
                 WHERE id = ?1 AND cancelled_at IS NULL AND paid_at IS NULL",
                params![id, now as i64],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Cancelled orders whose invoice is still open. Cleared by setting `error`, which also
    /// stops the page from polling for something that will never come.
    pub fn web_orders_to_cancel(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare(
            "SELECT id, invoice FROM web_orders \
             WHERE cancelled_at IS NOT NULL AND invoice IS NOT NULL AND error IS NULL",
        ) {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                out = rows.flatten().collect();
            }
        }
        out
    }

    pub fn web_order_paid(&self, id: &str, now: u64) {
        let _ = self.conn.execute(
            "UPDATE web_orders SET paid_at = ?2 WHERE id = ?1 AND paid_at IS NULL",
            params![id, now as i64],
        );
    }

    /// What the page needs to render one order: (invoice, pay_json, paid_at, error).
    pub fn web_order(&self, id: &str) -> Option<(Option<String>, Option<String>, Option<i64>, Option<String>)> {
        self.conn
            .query_row(
                "SELECT invoice, pay_json, paid_at, error FROM web_orders WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
    }

    /// How long the plaintext code stays retrievable after minting. Long enough that a
    /// buyer who closed the tab can reopen `#order=…` and still get it; short enough that
    /// the window is a window and not a store.
    const CODE_HOLD_MS: u64 = 30 * 60_000;

    /// Remember the code we just minted, so a dropped reply is a retry rather than a loss.
    pub fn web_order_code_set(&self, id: &str, code: &str, now: u64) {
        let _ = self.conn.execute(
            "UPDATE web_orders SET code = ?2, code_at = ?3 WHERE id = ?1",
            params![id, code, now as i64],
        );
    }

    /// The held plaintext, if it is still within the window. Outside it, `None` — the row
    /// may still carry the column, and the sweep has simply not run yet.
    pub fn web_order_code(&self, id: &str, now: u64) -> Option<String> {
        let cutoff = now.saturating_sub(Self::CODE_HOLD_MS) as i64;
        self.conn
            .query_row(
                "SELECT code FROM web_orders WHERE id = ?1 AND code IS NOT NULL AND code_at >= ?2",
                params![id, cutoff],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// The buyer says they have it. This is the ONLY thing that makes the promise on the
    /// page true, so it happens the moment they press the button, not on a timer.
    pub fn web_order_code_ack(&self, id: &str) -> bool {
        self.conn
            .execute("UPDATE web_orders SET code = NULL WHERE id = ?1 AND code IS NOT NULL", params![id])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Forget every held code past the window, acknowledged or not. Unconditional on
    /// purpose: a buyer who never presses the button must not leave bearer money in the
    /// table for as long as the table exists — and nothing prunes `web_orders`.
    pub fn web_orders_forget_codes(&self, now: u64) -> usize {
        let cutoff = now.saturating_sub(Self::CODE_HOLD_MS) as i64;
        self.conn
            .execute("UPDATE web_orders SET code = NULL WHERE code IS NOT NULL AND code_at < ?1", params![cutoff])
            .unwrap_or(0)
    }

    /// Forget HOW an old order was to be paid: the address and the memo in `pay_json`, on
    /// the same link-window beat that strips the account off a settled invoice.
    ///
    /// The row stays — amount, rail, consent and timestamps are the order's own record, and
    /// `sales.csv` is what accounting reads anyway. What goes is the payment detail, which
    /// nothing needs once the invoice is settled or dead. Until this existed `web_orders`
    /// was the one new table with no retention rule at all: it kept a payment address for
    /// as long as the table existed, which is to say forever (audit 2026-09-08).
    pub fn web_orders_forget_pay(&self, now: u64) -> usize {
        let cutoff = now.saturating_sub(crate::pay::Pay::account_link_ms()) as i64;
        self.conn
            .execute(
                "UPDATE web_orders SET pay_json = NULL \
                 WHERE pay_json IS NOT NULL AND created_at < ?1",
                params![cutoff],
            )
            .unwrap_or(0)
    }

    // ---- vouchers ---------------------------------------------------------------------

    /// Mint one, at SETTLEMENT of a paid web invoice — never at checkout, or an unpaid
    /// invoice would hand out credit. Returns false if the invoice already minted one
    /// (a re-settle must not double it).
    pub fn voucher_mint(&self, hash: &str, toku: u64, invoice: &str, now: u64) -> bool {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO vouchers (hash, toku, invoice, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![hash, toku as i64, invoice, now as i64],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Burn a voucher for `account`, atomically. This is the ONLY place single-use is
    /// decided, and it is decided by the database rather than by a read followed by a
    /// write: two requests racing the same code produce one winner because only one
    /// UPDATE matches `redeemed_at IS NULL`.
    ///
    /// Idempotent for the same account on purpose. A reply lost on the way back over the
    /// mixnet makes the app retry with the same code; refusing that would take the credit
    /// from someone who did everything right. A DIFFERENT account is refused — that is a
    /// second person holding a spent code.
    pub fn voucher_burn(&self, hash: &str, account: &str, now: u64) -> VoucherBurn {
        let burned = self
            .conn
            .execute(
                "UPDATE vouchers SET redeemed_at = ?3, account = ?2 \
                 WHERE hash = ?1 AND redeemed_at IS NULL AND void_at IS NULL",
                params![hash, account, now as i64],
            )
            .unwrap_or(0);
        if burned > 0 {
            let toku = self
                .conn
                .query_row("SELECT toku FROM vouchers WHERE hash = ?1", params![hash], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as u64;
            return VoucherBurn::Burned { toku };
        }
        // Nothing changed: say precisely why, the three cases mean different things.
        let row: Option<(Option<String>, Option<i64>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT account, redeemed_at, void_at FROM vouchers WHERE hash = ?1",
                params![hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        match row {
            None => VoucherBurn::Unknown,
            Some((_, _, Some(_))) => VoucherBurn::Void,
            Some((who, Some(_), _)) if who.as_deref() == Some(account) => VoucherBurn::AlreadyYours,
            Some(_) => VoucherBurn::Spent,
        }
    }

    /// The entitlement landed. Separate from the burn on purpose — see `vouchers_to_credit`.
    pub fn voucher_credited(&self, hash: &str, now: u64) {
        let _ = self.conn.execute(
            "UPDATE vouchers SET credited_at = ?2 WHERE hash = ?1 AND credited_at IS NULL",
            params![hash, now as i64],
        );
    }

    /// Burned but never credited: the crash window between the two writes. The burn is SQL,
    /// the credit is a bump inside the pay snapshot, and nothing spans both — so the order
    /// is burn-then-credit (a crash then loses the credit, which is recoverable) rather than
    /// credit-then-burn (a crash then leaves a spent code valid, which is not).
    pub fn vouchers_to_credit(&self) -> Vec<(String, String, u64)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare(
            "SELECT hash, account, toku FROM vouchers \
             WHERE redeemed_at IS NOT NULL AND credited_at IS NULL AND account IS NOT NULL",
        ) {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64))
            }) {
                out = rows.flatten().collect();
            }
        }
        out
    }

    /// (hash, redeemed_at) of vouchers that still name the account that redeemed them.
    ///
    /// `credited_at IS NOT NULL` is not decoration: the account is the ONLY thing that says
    /// where an uncredited voucher's money has to go, and `vouchers_to_credit` skips a row
    /// without one. Scrubbing a voucher whose credit is still outstanding would destroy the
    /// repair and the evidence in the same statement, and the buyer's money with them.
    /// Privacy-wise nothing is given up: such a row is repaired within a tick, so it is only
    /// ever still here because something is wrong and somebody will have to look at it.
    pub fn voucher_links(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare(
            "SELECT hash, redeemed_at FROM vouchers \
             WHERE account IS NOT NULL AND redeemed_at IS NOT NULL AND credited_at IS NOT NULL",
        ) {
            if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))) {
                out = rows.flatten().collect();
            }
        }
        out
    }

    /// Drop the PURCHASE from a voucher after the same ACCOUNT_LINK_DAYS (seven by default) an invoice keeps its
    /// account: past that, a code is a code and nobody can say which payment it came from.
    /// The column is NOT NULL and UNIQUE, so it cannot simply be nulled — it becomes a
    /// tombstone made from the fingerprint, which satisfies both and links to nothing.
    /// Everything that looked a voucher up by invoice (refund by receipt, "who bought this")
    /// stops finding it; the code itself still resolves, by hash, and still voids.
    pub fn voucher_forget_invoices(&self, now: u64) -> usize {
        let cutoff = now.saturating_sub(crate::pay::Pay::account_link_ms()) as i64;
        self.conn
            .execute(
                "UPDATE vouchers SET invoice = ?1 || substr(hash, 1, 16) \
                 WHERE created_at < ?2 AND invoice NOT LIKE ?3",
                params![EXPIRED_LINK, cutoff, format!("{EXPIRED_LINK}%")],
            )
            .unwrap_or(0)
    }

    /// Drop the account from a redeemed voucher — the same link-window rule an invoice
    /// follows, so the two do not disagree about how long a purchase stays attributable.
    pub fn voucher_forget_account(&self, hashes: &[String]) -> usize {
        let mut n = 0;
        for h in hashes {
            n += self
                .conn
                .execute("UPDATE vouchers SET account = NULL WHERE hash = ?1", params![h])
                .unwrap_or(0);
        }
        n
    }

    /// Refund path: void every unredeemed voucher of one invoice. Refuses a redeemed one —
    /// spent credit cannot be clawed back, the same rule the app follows.
    pub fn voucher_void_by_invoice(&self, invoice: &str, now: u64) -> VoucherVoid {
        void_on(&self.conn, invoice, now)
    }

    /// The stored JSON blob for `key`, if any.
    pub fn load(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT v FROM kv WHERE k = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
    }

    /// Upsert several blobs in ONE transaction: either all land or none do. The
    /// redeem path credits a session AND records the burned coin's serial in the same
    /// request — persisting them atomically means a crash can never leave the coin
    /// recorded-as-spent while its credit is lost (or vice versa) (H2).
    pub fn save_many(&mut self, pairs: &[(&str, &str)]) -> Result<(), String> {
        self.save_batch(pairs, &[])
    }

    /// `save_many` plus new quorum record rows, all in the same transaction.
    pub fn save_batch(&mut self, pairs: &[(&str, &str)], records: &[(usize, usize, String)]) -> Result<(), String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for (k, v) in pairs {
            tx.execute(
                "INSERT INTO kv (k, v) VALUES (?1, ?2) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![k, v],
            )
            .map_err(|e| e.to_string())?;
        }
        for (idx, coins, v) in records {
            tx.execute(
                "INSERT INTO quorum_records (idx, coins, v) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(idx) DO UPDATE SET v = excluded.v, coins = excluded.coins",
                params![*idx as i64, *coins as i64, v],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// All quorum record rows in index order.
    pub fn load_quorum_records(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(mut st) = self.conn.prepare("SELECT v FROM quorum_records ORDER BY idx") {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// Remove one blob (used once, to retire the legacy whole-quorum snapshot).
    pub fn delete(&self, key: &str) {
        let _ = self.conn.execute("DELETE FROM kv WHERE k = ?1", params![key]);
    }
}

/// What `vouchers.invoice` becomes once the purchase link has expired. A prefix, not NULL:
/// the column is NOT NULL and UNIQUE, and a tombstone built from the fingerprint keeps both
/// true while pointing at nothing.
pub const EXPIRED_LINK: &str = "expired:";

// ---- the refund path, from a second process --------------------------------------------
//
// A refund is decided in `tokumai-admin`, which does not share the server's `Store`. These
// take a path and open their own connection — the same shape `scrai-faucet` already uses on
// this file.

/// The single-use guard, on whatever connection. Both processes need it and the rule it
/// encodes must not exist twice: spent credit cannot be clawed back, so a redeemed voucher
/// is never voidable — the same rule the app follows for coins.
fn void_on(conn: &Connection, invoice: &str, now: u64) -> VoucherVoid {
    let voided = conn
        .execute(
            "UPDATE vouchers SET void_at = ?2 \
             WHERE invoice = ?1 AND redeemed_at IS NULL AND void_at IS NULL",
            params![invoice, now as i64],
        )
        .unwrap_or(0);
    if voided > 0 {
        return VoucherVoid::Voided(voided);
    }
    let known: i64 = conn
        .query_row("SELECT COUNT(*) FROM vouchers WHERE invoice = ?1", params![invoice], |r| r.get(0))
        .unwrap_or(0);
    if known == 0 {
        VoucherVoid::Unknown
    } else {
        VoucherVoid::AlreadySpent
    }
}

fn ro(db: &std::path::Path) -> Option<Connection> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
}

/// What one voucher is: (TOKU, redeemed_at, void_at), by the invoice the buyer's receipt
/// number points at. `redeemed_at` is the whole decision — see `docs/vouchers.md`.
pub fn voucher_state(db: &std::path::Path, invoice: &str) -> Option<(u64, Option<u64>, Option<u64>)> {
    ro(db)?
        .query_row(
            "SELECT toku, redeemed_at, void_at FROM vouchers WHERE invoice = ?1",
            params![invoice],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                ))
            },
        )
        .ok()
}

/// What one voucher is, by its fingerprint — the lookup that outlives the purchase link.
pub fn voucher_state_by_hash(db: &std::path::Path, hash: &str) -> Option<(u64, Option<u64>, Option<u64>)> {
    ro(db)?
        .query_row(
            "SELECT toku, redeemed_at, void_at FROM vouchers WHERE hash = ?1",
            params![hash],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                ))
            },
        )
        .ok()
}

/// Has a code been issued for this web order — ever, held or not? The guard against
/// minting a SECOND voucher for a paid order. It used to be the UNIQUE index on
/// `vouchers.invoice`; once that link expires into a tombstone the index no longer knows
/// the order, and this does.
pub fn web_order_code_issued(db: &std::path::Path, order: &str) -> bool {
    ro(db)
        .and_then(|c| {
            c.query_row("SELECT code_at IS NOT NULL FROM web_orders WHERE id = ?1", params![order], |r| r.get::<_, bool>(0))
                .ok()
        })
        .unwrap_or(false)
}

/// Void by fingerprint — the way that still works after the purchase link has expired, and
/// the way a pasted code is voided regardless. Same guard: a redeemed one is never voidable.
pub fn void_voucher_by_hash(db: &std::path::Path, hash: &str, now: u64) -> Result<VoucherVoid, String> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("state.db: {e}"))?;
    let voided = conn
        .execute(
            "UPDATE vouchers SET void_at = ?2 WHERE hash = ?1 AND redeemed_at IS NULL AND void_at IS NULL",
            params![hash, now as i64],
        )
        .unwrap_or(0);
    if voided > 0 {
        return Ok(VoucherVoid::Voided(voided));
    }
    let known: i64 = conn
        .query_row("SELECT COUNT(*) FROM vouchers WHERE hash = ?1", params![hash], |r| r.get(0))
        .unwrap_or(0);
    Ok(if known == 0 { VoucherVoid::Unknown } else { VoucherVoid::AlreadySpent })
}

/// The invoice a code belongs to, by fingerprint. This is what makes "paste the code" the
/// strongest thing a buyer can show: it names the row directly, and holding the code is
/// what being entitled to it means.
pub fn voucher_invoice_for(db: &std::path::Path, hash: &str) -> Option<String> {
    ro(db)?
        .query_row("SELECT invoice FROM vouchers WHERE hash = ?1", params![hash], |r| r.get::<_, String>(0))
        .ok()
}

/// Void from the admin. Same guard, its own connection.
pub fn void_voucher(db: &std::path::Path, invoice: &str, now: u64) -> Result<VoucherVoid, String> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("state.db: {e}"))?;
    Ok(void_on(&conn, invoice, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_many_commits_all_keys_atomically() {
        let p = std::env::temp_dir().join(format!("scrai-store-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let mut s = Store::open(&p).unwrap();
        // A redeem-shaped write: session credit + quorum serial together.
        s.save_many(&[("sessions", "{\"bal\":5}"), ("quorum", "{\"serials\":[1]}")]).unwrap();
        assert_eq!(s.load("sessions").as_deref(), Some("{\"bal\":5}"));
        assert_eq!(s.load("quorum").as_deref(), Some("{\"serials\":[1]}"));
        // Upsert overwrites in place.
        s.save_many(&[("sessions", "{\"bal\":9}")]).unwrap();
        assert_eq!(s.load("sessions").as_deref(), Some("{\"bal\":9}"));
        assert_eq!(s.load("quorum").as_deref(), Some("{\"serials\":[1]}"));
        let _ = std::fs::remove_file(&p);
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("scrai-store-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// The bug this guards: the order row is only cleared when the gateway answers, so a
    /// one-second tick used to hand the SAME order to the gateway again and again. Every
    /// raise wrote back over the row, and the buyer paid whichever address the page had
    /// rendered first — not the one the invoice ended up expecting.
    #[test]
    fn an_order_is_handed_to_the_gateway_once_while_the_raise_is_in_flight() {
        let p = tmp("order-claim");
        let s = Store::open(&p).unwrap();
        assert!(s.web_order_new("ord1", 10, "card", "2026-09-07", 1_000));

        // First tick claims it — and carries the consent version, which is what reaches the
        // invoice and from there the sales ledger.
        let picked = s.web_orders_pending(8);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].3, "2026-09-07", "the confirmation must survive the trip to the invoice");
        // Every tick for the next minute and a half sees nothing — the raise is in flight.
        assert!(s.web_orders_pending(8).is_empty());
        assert!(s.web_orders_pending(8).is_empty());

        // The answer clears the row for good.
        s.web_order_answer("ord1", Some("ord1"), Some("{}"), None);
        assert!(s.web_orders_pending(8).is_empty());

        // A raise that died with the process is retried once the claim goes stale.
        assert!(s.web_order_new("ord2", 10, "nyx", "2026-09-07", 1_000));
        assert_eq!(s.web_orders_pending(8).len(), 1);
        let stale = (crate::pay::now_ms() - Store::RAISE_CLAIM_MS - 1_000) as i64;
        s.conn
            .execute("UPDATE web_orders SET raising_at = ?1 WHERE id = 'ord2'", params![stale])
            .unwrap();
        assert_eq!(s.web_orders_pending(8).len(), 1, "a claim older than the timeout is retried");

        // A cancelled order is never raised, even if the cancel beat the first tick.
        assert!(s.web_order_new("ord3", 10, "nyx", "2026-09-07", 1_000));
        assert!(s.web_order_cancel("ord3", 2_000));
        assert!(!s.web_orders_pending(8).iter().any(|(id, _, _, _)| id == "ord3"));
        let _ = std::fs::remove_file(&p);
    }

    /// The whole of H2 in one test: minting is a retry, not a one-shot. A dropped reply, a
    /// closed tab, two polls racing — all of them come back to the SAME code, until the
    /// buyer says they have it or the window closes.
    #[test]
    fn a_minted_code_survives_a_lost_reply_until_it_is_acknowledged() {
        let p = tmp("code-hold");
        let s = Store::open(&p).unwrap();
        let now = crate::pay::now_ms();
        assert!(s.web_order_new("ord1", 10, "nyx", "2026-09-07", now));

        // Nothing held yet.
        assert_eq!(s.web_order_code("ord1", now), None);

        // Minted and held: every retry in the window gets the same string back.
        s.web_order_code_set("ord1", "TOKU-AAAA-BBBB-CCCC", now);
        assert_eq!(s.web_order_code("ord1", now).as_deref(), Some("TOKU-AAAA-BBBB-CCCC"));
        assert_eq!(s.web_order_code("ord1", now + 60_000).as_deref(), Some("TOKU-AAAA-BBBB-CCCC"));

        // The buyer confirms — the copy goes at once, not on a timer.
        assert!(s.web_order_code_ack("ord1"));
        assert_eq!(s.web_order_code("ord1", now), None);
        assert!(!s.web_order_code_ack("ord1"), "acking twice is not a second deletion");

        // A buyer who never confirms is swept anyway: bearer money must not outlive the
        // window just because somebody closed a tab.
        assert!(s.web_order_new("ord2", 10, "nyx", "2026-09-07", now));
        s.web_order_code_set("ord2", "TOKU-DDDD-EEEE-FFFF", now);
        let past = now + Store::CODE_HOLD_MS + 1_000;
        assert_eq!(s.web_order_code("ord2", past), None, "outside the window it is not handed out");
        assert_eq!(s.web_orders_forget_codes(past), 1);
        assert_eq!(s.web_orders_forget_codes(past), 0, "the sweep is idempotent");
        let _ = std::fs::remove_file(&p);
    }

    /// `web_orders` was the one new table with no retention rule: it kept the payment
    /// address and memo of every order ever placed, for as long as the table existed.
    #[test]
    fn an_old_web_order_forgets_how_it_was_to_be_paid_but_stays_a_record() {
        let p = tmp("weborder-retention");
        let s = Store::open(&p).unwrap();
        let now = crate::pay::now_ms();
        let old = now - crate::pay::Pay::account_link_ms() - 1_000;

        assert!(s.web_order_new("fresh", 10, "nyx", "2026-09-07", now));
        assert!(s.web_order_new("stale", 10, "nyx", "2026-09-07", old));
        s.web_order_answer("fresh", Some("fresh"), Some("{\"memo\":\"abc\"}"), None);
        s.web_order_answer("stale", Some("stale"), Some("{\"memo\":\"xyz\"}"), None);

        assert_eq!(s.web_orders_forget_pay(now), 1, "only the one past the window");
        assert_eq!(s.web_orders_forget_pay(now), 0, "and only once");

        // What went is the payment detail. What stays is the order.
        let (inv, pay, _, _) = s.web_order("stale").expect("the row is still there");
        assert_eq!(inv.as_deref(), Some("stale"));
        assert_eq!(pay, None, "the address and memo are gone");
        let (_, fresh_pay, _, _) = s.web_order("fresh").expect("the fresh row is untouched");
        assert!(fresh_pay.is_some(), "a recent order still knows how to be paid");
        let _ = std::fs::remove_file(&p);
    }

    /// After ACCOUNT_LINK_DAYS (seven by default) a voucher stops saying which purchase it came from — the same
    /// window an invoice keeps its account — but stays a voucher: same value, same state,
    /// still voidable by fingerprint. And a paid web order can never mint twice, link or no
    /// link.
    #[test]
    fn the_purchase_link_on_a_voucher_expires_but_the_voucher_does_not() {
        let p = tmp("voucher-link");
        let s = Store::open(&p).unwrap();
        let now = crate::pay::now_ms();
        let old = now - crate::pay::Pay::account_link_ms() - 1_000;
        assert!(s.voucher_mint("h-old", 500_000, "inv-old", old));
        assert!(s.voucher_mint("h-new", 500_000, "inv-new", now));

        assert_eq!(s.voucher_forget_invoices(now), 1, "only the old one");
        assert_eq!(s.voucher_forget_invoices(now), 0, "and only once");
        assert!(voucher_state(&p, "inv-old").is_none(), "by invoice it is gone");
        assert!(voucher_state(&p, "inv-new").is_some(), "the fresh one is not");
        assert_eq!(voucher_state_by_hash(&p, "h-old"), Some((500_000, None, None)), "by hash it is intact");
        assert!(voucher_invoice_for(&p, "h-old").unwrap().starts_with(EXPIRED_LINK));

        // Still refundable — by the code, which is what a buyer holds.
        assert!(matches!(void_voucher_by_hash(&p, "h-old", now).unwrap(), VoucherVoid::Voided(1)));
        assert!(matches!(void_voucher_by_hash(&p, "h-old", now).unwrap(), VoucherVoid::AlreadySpent));

        // The web order remembers a code was issued even when the voucher no longer names it.
        assert!(s.web_order_new("ord", 5, "nyx", "2026-09-07", old));
        assert!(!web_order_code_issued(&p, "ord"));
        s.web_order_code_set("ord", "TOKU-XXXX", old);
        assert!(web_order_code_issued(&p, "ord"), "issued stays true after the plaintext is swept");
        s.web_orders_forget_codes(now);
        assert!(web_order_code_issued(&p, "ord"));
        let _ = std::fs::remove_file(&p);
    }

    /// The account on a redeemed voucher is what says where an uncredited one's money must
    /// go. Dropping it after ACCOUNT_LINK_DAYS (seven by default) is right for a voucher that HAS been credited and
    /// fatal for one that has not: `vouchers_to_credit` would never see it again.
    #[test]
    fn the_fourteen_day_scrub_never_touches_a_voucher_that_still_owes_its_credit() {
        let p = tmp("voucher-scrub");
        let s = Store::open(&p).unwrap();
        let long_ago = 1_000u64;

        assert!(s.voucher_mint("hash-credited", 500_000, "inv-a", long_ago));
        assert!(s.voucher_mint("hash-owed", 500_000, "inv-b", long_ago));
        assert!(matches!(s.voucher_burn("hash-credited", "acct-1", long_ago), VoucherBurn::Burned { .. }));
        assert!(matches!(s.voucher_burn("hash-owed", "acct-2", long_ago), VoucherBurn::Burned { .. }));
        s.voucher_credited("hash-credited", long_ago);

        // Only the settled one is offered to the scrub.
        let links = s.voucher_links();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, "hash-credited");

        // And the one that still owes a credit is still repairable.
        let owed = s.vouchers_to_credit();
        assert_eq!(owed.len(), 1);
        assert_eq!((owed[0].0.as_str(), owed[0].1.as_str(), owed[0].2), ("hash-owed", "acct-2", 500_000));
        let _ = std::fs::remove_file(&p);
    }
}

/// "YYYY-MM-DD" minus `days` (civil arithmetic on a proleptic Gregorian calendar).
fn day_minus(day: &str, days: i64) -> Option<String> {
    let mut it = day.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    // days-from-civil / civil-from-days (Howard Hinnant)
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let z = era * 146097 + doe - 719468 - days;
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

#[cfg(test)]
mod day_tests {
    use super::day_minus;
    #[test]
    fn civil_subtraction() {
        assert_eq!(day_minus("2026-08-29", 90).as_deref(), Some("2026-05-31"));
        assert_eq!(day_minus("2026-03-01", 1).as_deref(), Some("2026-02-28"));
        assert_eq!(day_minus("2024-03-01", 1).as_deref(), Some("2024-02-29"));
        assert_eq!(day_minus("2026-01-01", 1).as_deref(), Some("2025-12-31"));
        assert_eq!(day_minus("garbage", 1), None);
    }
}

#[cfg(test)]
mod voucher_tests {
    use super::*;

    fn store() -> Store {
        let p = std::env::temp_dir().join(format!("tokumai-v-{}.db", std::process::id() as u64 * 7 + rand_suffix()));
        Store::open(&p).expect("open")
    }
    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0)
    }

    #[test]
    fn a_voucher_burns_exactly_once_and_a_retry_is_not_punished() {
        let db = store();
        assert!(db.voucher_mint("h1", 1_000_000, "inv1", 100), "minted");
        assert!(!db.voucher_mint("h1", 1_000_000, "inv1", 100), "a re-settle must not mint twice");

        // the winner
        assert!(matches!(db.voucher_burn("h1", "alice", 200), VoucherBurn::Burned { toku: 1_000_000 }));
        // alice again: a reply lost over the mixnet, not an attack
        assert!(matches!(db.voucher_burn("h1", "alice", 201), VoucherBurn::AlreadyYours));
        // somebody else holding the same string
        assert!(matches!(db.voucher_burn("h1", "mallory", 202), VoucherBurn::Spent));
        // never existed
        assert!(matches!(db.voucher_burn("nope", "alice", 203), VoucherBurn::Unknown));
    }

    #[test]
    fn the_crash_window_is_repairable_and_only_once() {
        let db = store();
        db.voucher_mint("h2", 500_000, "inv2", 100);
        db.voucher_burn("h2", "bob", 200);

        // burned, not yet credited: exactly what the boot pass looks for
        let pending = db.vouchers_to_credit();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], ("h2".into(), "bob".into(), 500_000));

        db.voucher_credited("h2", 300);
        assert!(db.vouchers_to_credit().is_empty(), "credited once, never replayed");
    }

    #[test]
    fn voiding_refuses_a_spent_voucher() {
        let db = store();
        db.voucher_mint("h3", 500_000, "inv3", 100);
        assert!(matches!(db.voucher_void_by_invoice("inv3", 200), VoucherVoid::Voided(1)));
        // and a voided code cannot then be redeemed
        assert!(matches!(db.voucher_burn("h3", "carol", 300), VoucherBurn::Void));

        db.voucher_mint("h4", 500_000, "inv4", 100);
        db.voucher_burn("h4", "dave", 200);
        assert!(matches!(db.voucher_void_by_invoice("inv4", 300), VoucherVoid::AlreadySpent),
            "spent credit cannot be clawed back — the same rule the app follows");
        assert!(matches!(db.voucher_void_by_invoice("inv-nope", 300), VoucherVoid::Unknown));
    }
}
