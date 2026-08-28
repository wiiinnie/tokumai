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

    /// Remember that `session_id` chatted on `day` (idempotent). Best-effort like `bump_daily`.
    pub fn note_user(&self, day: &str, session_id: &str) {
        let h: String = scrai_core::auth::sha256(&[session_id.as_bytes()]).iter().take(8).map(|b| format!("{b:02x}")).collect();
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
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for (k, v) in pairs {
            tx.execute(
                "INSERT INTO kv (k, v) VALUES (?1, ?2) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![k, v],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
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
