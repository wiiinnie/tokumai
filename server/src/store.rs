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

use rusqlite::{params, Connection};
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
        let _ = conn.execute("CREATE INDEX IF NOT EXISTS vouchers_by_invoice ON vouchers (invoice)", []);

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

    /// Refund path: void every unredeemed voucher of one invoice. Refuses a redeemed one —
    /// spent credit cannot be clawed back, the same rule the app follows.
    pub fn voucher_void_by_invoice(&self, invoice: &str, now: u64) -> VoucherVoid {
        let voided = self
            .conn
            .execute(
                "UPDATE vouchers SET void_at = ?2 \
                 WHERE invoice = ?1 AND redeemed_at IS NULL AND void_at IS NULL",
                params![invoice, now as i64],
            )
            .unwrap_or(0);
        if voided > 0 {
            return VoucherVoid::Voided(voided);
        }
        let known: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM vouchers WHERE invoice = ?1", params![invoice], |r| r.get(0))
            .unwrap_or(0);
        if known == 0 {
            VoucherVoid::Unknown
        } else {
            VoucherVoid::AlreadySpent
        }
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
