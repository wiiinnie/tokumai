//! The account book, inside the enclave. Two pockets per account:
//!
//! - the **allowance** of the plan's current period (set when a period begins, gone when it
//!   ends — the plan logic in `tokumai_core::subscription` decides when);
//! - **prepaid lots**, each valid three years from its purchase (decided 2026-09-22). Only
//!   possible now: the balance lives here, so what is unspent is known exactly.
//!
//! A request first **holds** its worst case, then **settles** at what it actually cost, and the
//! rest goes back where it came from. Holds are written down, so a request cut off by a
//! restart gives its hold back at the next start instead of keeping it.
//!
//! Accounts are stored under a keyed hash of their id (see `seal`): the database alone does
//! not say which accounts exist.

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

/// How long a prepaid lot is valid: three years from purchase, by the calendar.
pub const PREPAID_MONTHS: u32 = 36;

pub struct Ledger {
    conn: Connection,
    data_key: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Balance {
    pub allowance: u64,
    pub allowance_ends_ms: u64,
    /// (TOKU left, expires at) per prepaid lot, soonest first.
    pub prepaid: Vec<(u64, u64)>,
    pub total: u64,
}

/// What a hold took, and from where — so the unused part goes back to the same place.
#[derive(Debug)]
pub struct Hold {
    pub id: i64,
    pub account: String,
    pub amount: u64,
    parts: Vec<(Pocket, u64)>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum Pocket {
    Allowance,
    Lot(i64),
}

impl Ledger {
    pub fn open(path: &Path, data_key: [u8; 32]) -> Result<Ledger, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::with(conn, data_key)
    }

    pub fn in_memory(data_key: [u8; 32]) -> Result<Ledger, String> {
        Self::with(Connection::open_in_memory().map_err(|e| e.to_string())?, data_key)
    }

    fn with(conn: Connection, data_key: [u8; 32]) -> Result<Ledger, String> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS allowance (acct TEXT PRIMARY KEY, period INTEGER NOT NULL,
                 ends_ms INTEGER NOT NULL, granted INTEGER NOT NULL, left INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS lots (id INTEGER PRIMARY KEY AUTOINCREMENT, acct TEXT NOT NULL,
                 left INTEGER NOT NULL, expires_ms INTEGER NOT NULL, bought_ms INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS lots_acct ON lots (acct, expires_ms);
             CREATE TABLE IF NOT EXISTS holds (id INTEGER PRIMARY KEY AUTOINCREMENT, acct TEXT NOT NULL,
                 parts TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS nonces (nonce TEXT PRIMARY KEY, ts_ms INTEGER NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Ledger { conn, data_key })
    }

    fn key(&self, account_id: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"tokumai/ledger/acct/v1");
        h.update(self.data_key);
        h.update(account_id.as_bytes());
        hex::encode(h.finalize())
    }

    /// A prepaid purchase: a new lot, valid three years from `now_ms`.
    pub fn credit_prepaid(&self, account_id: &str, toku: u64, now_ms: u64) -> Result<(), String> {
        let expires = tokumai_core::subscription::add_months_ms(now_ms, PREPAID_MONTHS);
        self.conn
            .execute(
                "INSERT INTO lots (acct, left, expires_ms, bought_ms) VALUES (?1, ?2, ?3, ?4)",
                params![self.key(account_id), toku as i64, expires as i64, now_ms as i64],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// A plan period begins: its allowance is SET, never added to.
    pub fn grant_allowance(&self, account_id: &str, start_ms: u64, ends_ms: u64, toku: u64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO allowance (acct, period, ends_ms, granted, left) VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(acct) DO UPDATE SET period = excluded.period, ends_ms = excluded.ends_ms,
                     granted = excluded.granted, left = excluded.left",
                params![self.key(account_id), (start_ms / 1000) as i64, ends_ms as i64, toku as i64],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn balance(&self, account_id: &str, now_ms: u64) -> Result<Balance, String> {
        let k = self.key(account_id);
        let (allowance, ends) = self
            .conn
            .query_row("SELECT left, ends_ms FROM allowance WHERE acct = ?1", params![k], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
            })
            .optional()
            .map_err(|e| e.to_string())?
            .filter(|(_, ends)| *ends > now_ms)
            .unwrap_or((0, 0));
        let mut st = self
            .conn
            .prepare("SELECT left, expires_ms FROM lots WHERE acct = ?1 AND left > 0 AND expires_ms > ?2 ORDER BY expires_ms")
            .map_err(|e| e.to_string())?;
        let prepaid: Vec<(u64, u64)> = st
            .query_map(params![k, now_ms as i64], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        let total = allowance + prepaid.iter().map(|(l, _)| l).sum::<u64>();
        Ok(Balance { allowance, allowance_ends_ms: ends, prepaid, total })
    }

    /// Take `amount` off the table for one request: the allowance first (it lapses with its
    /// period), then the prepaid lots that expire soonest. All or nothing.
    pub fn hold(&mut self, account_id: &str, amount: u64, now_ms: u64) -> Result<Hold, String> {
        let k = self.key(account_id);
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut parts: Vec<(Pocket, u64)> = Vec::new();
        let mut need = amount;
        let allowance: Option<i64> = tx
            .query_row("SELECT left FROM allowance WHERE acct = ?1 AND ends_ms > ?2", params![k, now_ms as i64], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(left) = allowance {
            let take = need.min(left as u64);
            if take > 0 {
                tx.execute("UPDATE allowance SET left = left - ?2 WHERE acct = ?1", params![k, take as i64])
                    .map_err(|e| e.to_string())?;
                parts.push((Pocket::Allowance, take));
                need -= take;
            }
        }
        if need > 0 {
            let lots: Vec<(i64, i64)> = {
                let mut st = tx
                    .prepare("SELECT id, left FROM lots WHERE acct = ?1 AND left > 0 AND expires_ms > ?2 ORDER BY expires_ms")
                    .map_err(|e| e.to_string())?;
                let rows = st
                    .query_map(params![k, now_ms as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| e.to_string())?;
                rows.flatten().collect()
            };
            for (id, left) in lots {
                if need == 0 {
                    break;
                }
                let take = need.min(left as u64);
                tx.execute("UPDATE lots SET left = left - ?2 WHERE id = ?1", params![id, take as i64]).map_err(|e| e.to_string())?;
                parts.push((Pocket::Lot(id), take));
                need -= take;
            }
        }
        if need > 0 {
            // Dropping the transaction rolls every take back.
            return Err(format!("not enough credit: this request can cost up to {amount} TOKU, the account holds {}", amount - need));
        }
        let json = serde_json::to_string(&parts).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO holds (acct, parts) VALUES (?1, ?2)", params![k, json]).map_err(|e| e.to_string())?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(|e| e.to_string())?;
        Ok(Hold { id, account: account_id.to_string(), amount, parts })
    }

    /// The request is answered: keep `cost` (at most what was held) and give the rest back,
    /// last pocket first, so what was spent came out of the allowance first.
    pub fn settle(&mut self, hold: Hold, cost: u64) -> Result<u64, String> {
        let kept = cost.min(hold.amount);
        let mut refund = hold.amount - kept;
        let acct = self.key(&hold.account);
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for (pocket, took) in hold.parts.iter().rev() {
            if refund == 0 {
                break;
            }
            let back = refund.min(*took);
            Self::give_back(&tx, &acct, *pocket, back)?;
            refund -= back;
        }
        tx.execute("DELETE FROM holds WHERE id = ?1", params![hold.id]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(kept)
    }

    fn give_back(tx: &rusqlite::Transaction, acct: &str, pocket: Pocket, amount: u64) -> Result<(), String> {
        match pocket {
            Pocket::Allowance => tx.execute("UPDATE allowance SET left = left + ?2 WHERE acct = ?1", params![acct, amount as i64]),
            Pocket::Lot(id) => tx.execute("UPDATE lots SET left = left + ?2 WHERE id = ?1", params![id, amount as i64]),
        }
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    /// At start: holds left by requests that died with the last process go back in full.
    pub fn release_open_holds(&mut self) -> Result<usize, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let open: Vec<(i64, String, String)> = {
            let mut st = tx.prepare("SELECT id, acct, parts FROM holds").map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        for (id, acct, parts) in &open {
            let parts: Vec<(Pocket, u64)> = serde_json::from_str(parts).map_err(|e| e.to_string())?;
            for (pocket, took) in parts {
                Self::give_back(&tx, acct, pocket, took)?;
            }
            tx.execute("DELETE FROM holds WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(open.len())
    }

    /// Record a request nonce. False if it was seen before (a replay, or a resend).
    pub fn first_sight(&self, nonce: &str, now_ms: u64) -> Result<bool, String> {
        self.conn
            .execute("DELETE FROM nonces WHERE ts_ms < ?1", params![now_ms.saturating_sub(NONCE_KEEP_MS) as i64])
            .map_err(|e| e.to_string())?;
        let n = self
            .conn
            .execute("INSERT OR IGNORE INTO nonces (nonce, ts_ms) VALUES (?1, ?2)", params![nonce, now_ms as i64])
            .map_err(|e| e.to_string())?;
        Ok(n == 1)
    }
}

/// How long a request nonce is remembered — longer than the window a request's timestamp may
/// lie from the enclave's clock (`service::CLOCK_SKEW_MS`), so no nonce outlives its memory.
pub const NONCE_KEEP_MS: u64 = 20 * 60 * 1000;

#[cfg(test)]
mod tests {
    use super::*;
    const DAY: u64 = 86_400_000;

    fn ledger() -> Ledger {
        Ledger::in_memory([3u8; 32]).unwrap()
    }

    #[test]
    fn a_request_spends_the_allowance_before_prepaid_and_pays_only_what_it_cost() {
        let mut l = ledger();
        let now = 1_800_000_000_000;
        l.grant_allowance("a", now, now + 30 * DAY, 1_000).unwrap();
        l.credit_prepaid("a", 5_000, now).unwrap();
        let h = l.hold("a", 1_500, now).unwrap();
        assert_eq!(l.balance("a", now).unwrap().total, 4_500);
        assert_eq!(l.settle(h, 1_200).unwrap(), 1_200);
        let b = l.balance("a", now).unwrap();
        assert_eq!((b.allowance, b.total), (0, 4_800), "the allowance went first, the unused 300 went back to prepaid");
    }

    #[test]
    fn a_hold_the_account_cannot_cover_takes_nothing() {
        let mut l = ledger();
        let now = 1_800_000_000_000;
        l.credit_prepaid("a", 100, now).unwrap();
        assert!(l.hold("a", 101, now).is_err());
        assert_eq!(l.balance("a", now).unwrap().total, 100);
    }

    #[test]
    fn prepaid_lasts_three_years_from_purchase_and_the_soonest_to_expire_goes_first() {
        let mut l = ledger();
        let t0 = 1_800_000_000_000;
        l.credit_prepaid("a", 100, t0).unwrap();
        l.credit_prepaid("a", 100, t0 + 400 * DAY).unwrap();
        let h = l.hold("a", 150, t0 + 401 * DAY).unwrap();
        l.settle(h, 150).unwrap();
        let b = l.balance("a", t0 + 401 * DAY).unwrap();
        assert_eq!(b.prepaid.len(), 1, "the older lot is used up first");
        assert_eq!(b.prepaid[0].0, 50);
        // The second lot is still good just under three years after ITS purchase, and gone after.
        assert_eq!(l.balance("a", t0 + 400 * DAY + 3 * 365 * DAY - DAY).unwrap().total, 50);
        assert_eq!(l.balance("a", t0 + 400 * DAY + 3 * 366 * DAY).unwrap().total, 0);
    }

    #[test]
    fn an_allowance_is_set_not_added_and_lapses_with_its_period() {
        let l = ledger();
        let now = 1_800_000_000_000;
        l.grant_allowance("a", now, now + 30 * DAY, 700).unwrap();
        l.grant_allowance("a", now + 30 * DAY, now + 60 * DAY, 700).unwrap();
        assert_eq!(l.balance("a", now + 31 * DAY).unwrap().allowance, 700);
        assert_eq!(l.balance("a", now + 61 * DAY).unwrap().allowance, 0);
    }

    #[test]
    fn a_request_cut_off_by_a_restart_gives_its_hold_back() {
        let dir = std::env::temp_dir().join(format!("tk-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("l.db");
        let now = 1_800_000_000_000;
        {
            let mut l = Ledger::open(&path, [3; 32]).unwrap();
            l.credit_prepaid("a", 1_000, now).unwrap();
            let _h = l.hold("a", 600, now).unwrap(); // the process dies here
        }
        let mut l = Ledger::open(&path, [3; 32]).unwrap();
        assert_eq!(l.release_open_holds().unwrap(), 1);
        assert_eq!(l.balance("a", now).unwrap().total, 1_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_database_does_not_name_its_accounts() {
        let l = ledger();
        l.credit_prepaid("visible-account-id", 1, 0).unwrap();
        let stored: String = l.conn.query_row("SELECT acct FROM lots", [], |r| r.get(0)).unwrap();
        assert!(!stored.contains("visible"));
        assert_ne!(Ledger::in_memory([4; 32]).unwrap().key("visible-account-id"), stored, "another key, another name");
    }

    #[test]
    fn a_nonce_is_accepted_once() {
        let l = ledger();
        assert!(l.first_sight("n1", 1_000).unwrap());
        assert!(!l.first_sight("n1", 2_000).unwrap());
    }
}
