// ---------------------------------------------------------------------------
// store.rs — the "guest book" itself: a hardened, LOCAL SQLite database.
//
// This is the authoritative store for the seed-recoverable, cross-server value layer
// (redeemed session credit, keyed by the seed-derived sessionId). It is deliberately
// row-level SQL (not a whole-store JSON snapshot) so multiple servers can later share
// ONE database consistently instead of each snapshotting its own copy.
//
// HARDENING (defense-in-depth even though nothing but the local ledger-service process
// ever opens it — the service itself is reached over the mixnet, never a network port):
//   - the file is chmod 0600 on creation (owner-only);
//   - WAL + synchronous=FULL so a crash can't tear a credit;
//   - no network listener of any kind — it's a local file.
// At-rest ENCRYPTION is a deployment concern: run it on a LUKS-encrypted volume (baseline),
// and/or swap bundled SQLite for SQLCipher (a build-flag follow-up — the API here is
// unchanged). See docs/federation-shared-ledger.md §6c.
// ---------------------------------------------------------------------------

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use scrai_core::ledger::Ledger;

pub struct LedgerStore {
    // A `Mutex` makes the store `Send + Sync` (rusqlite's `Connection` is `!Sync`), which
    // the async `Ledger` trait's `Send` futures require. The methods never await while
    // holding the guard (pure SQLite), so there is no guard-across-await hazard.
    conn: Mutex<Connection>,
}

impl LedgerStore {
    /// Open (creating if needed) the ledger database at `path`, applying the hardening
    /// and creating the schema. Use `":memory:"` via [`LedgerStore::in_memory`] for tests.
    pub fn open(path: &Path) -> Result<LedgerStore, String> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        // Owner-only file permissions — the seed-recoverable balances live here.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path) {
                let mut perm = meta.permissions();
                perm.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perm);
            }
        }
        let store = LedgerStore { conn: Mutex::new(conn) };
        store.init()?;
        Ok(store)
    }

    /// An in-memory store — for tests and ephemeral use.
    pub fn in_memory() -> Result<LedgerStore, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        let store = LedgerStore { conn: Mutex::new(conn) };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<(), String> {
        // WAL + FULL: a credit is durable and crash-atomic before we reply to the client.
        self.conn
            .lock()
            .map_err(|e| e.to_string())?
            .execute_batch(
                "PRAGMA journal_mode = WAL;\n\
                 PRAGMA synchronous = FULL;\n\
                 CREATE TABLE IF NOT EXISTS sessions (\
                    session_id TEXT PRIMARY KEY,\
                    balance    INTEGER NOT NULL DEFAULT 0,\
                    counter    INTEGER NOT NULL DEFAULT 0\
                 );",
            )
            .map_err(|e| e.to_string())
    }

    /// Credit `amount` into a session, returning the resulting balance. Additive and
    /// idempotent-free at this layer — the caller (redeem) guarantees each credit is a
    /// distinct, already-quorum-accepted coin burn. One transaction so the read reflects
    /// the write.
    fn credit_inner(&self, id: &str, amount: u64) -> Result<u64, String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO sessions (session_id, balance) VALUES (?1, ?2) \
             ON CONFLICT(session_id) DO UPDATE SET balance = balance + excluded.balance",
            params![id, amount as i64],
        )
        .map_err(|e| e.to_string())?;
        let bal: i64 = tx
            .query_row("SELECT balance FROM sessions WHERE session_id = ?1", params![id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(bal.max(0) as u64)
    }

    fn status_inner(&self, id: &str) -> Result<(u64, u64), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        match conn.query_row(
            "SELECT balance, counter FROM sessions WHERE session_id = ?1",
            params![id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        ) {
            Ok((b, c)) => Ok((b.max(0) as u64, c.max(0) as u64)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((0, 0)),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The store IS a local `Ledger` (in-process, durable). The ledger-service uses this
/// directly; a co-located all-in-one server can too. Remote servers reach the service
/// over the mixnet via `MixnetLedger`, which implements the same trait.
#[async_trait]
impl Ledger for LedgerStore {
    async fn session_status(&self, id: &str) -> Result<(u64, u64), String> {
        self.status_inner(id)
    }
    async fn session_balance(&self, id: &str) -> Result<u64, String> {
        Ok(self.status_inner(id)?.0)
    }
    async fn session_credit(&mut self, id: &str, amount: u64) -> Result<u64, String> {
        self.credit_inner(id, amount)
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    #[test]
    fn credit_accumulates_and_reads_back() {
        let mut s = LedgerStore::in_memory().unwrap();
        assert_eq!(block_on(s.session_balance("sess-a")).unwrap(), 0);
        assert_eq!(block_on(s.session_credit("sess-a", 100_000)).unwrap(), 100_000);
        assert_eq!(block_on(s.session_credit("sess-a", 50_000)).unwrap(), 150_000);
        assert_eq!(block_on(s.session_balance("sess-a")).unwrap(), 150_000);
        assert_eq!(block_on(s.session_status("sess-a")).unwrap(), (150_000, 0));
        // a different session is independent
        assert_eq!(block_on(s.session_balance("sess-b")).unwrap(), 0);
    }

    #[test]
    fn survives_reopen() {
        let dir = std::env::temp_dir().join(format!("scrai-ledger-test-{}", std::process::id()));
        let path = dir.join("ledger.db");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut s = LedgerStore::open(&path).unwrap();
            block_on(s.session_credit("sess-x", 12_345)).unwrap();
        }
        // reopened → the balance is still there (durable, not a snapshot)
        let s = LedgerStore::open(&path).unwrap();
        assert_eq!(block_on(s.session_balance("sess-x")).unwrap(), 12_345);

        // and the file is owner-only
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "ledger db must be chmod 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
