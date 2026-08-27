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
        Ok(Store { conn })
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
