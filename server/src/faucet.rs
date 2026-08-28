//! Shared bits of the testnet faucet ledger (`faucet.db`), used by the `scrai-faucet`
//! binary (claims) and by `scrai-admin` (minting invite codes, listing them). The ledger
//! lives next to `state.db`; the server itself never opens it.

use rusqlite::{params, Connection};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Purchases one invite code is good for: ONE — a code is a single $1 claim (user decision
/// 2026-08-28). `scrai-faucet code new [uses]` can still mint a multi-use code on purpose.
pub const DEFAULT_CODE_USES: u32 = 1;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Open (creating if needed) the faucet ledger with its schema.
pub fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("faucet.db: {e}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS codes (
           code TEXT PRIMARY KEY, max_uses INTEGER NOT NULL, uses INTEGER NOT NULL DEFAULT 0,
           note TEXT NOT NULL DEFAULT '', created INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS claims (
           memo TEXT PRIMARY KEY, invoice_id TEXT UNIQUE NOT NULL, code TEXT NOT NULL,
           unym INTEGER NOT NULL, tx TEXT NOT NULL DEFAULT '', stage TEXT NOT NULL, ts INTEGER NOT NULL);",
    )
    .map_err(|e| format!("faucet.db schema: {e}"))?;
    Ok(conn)
}

/// `SCRAI-XXXX-XXXX` from an alphabet without look-alikes (no 0/O, 1/I/L).
pub fn new_code() -> String {
    use rand::Rng;
    const ALPHA: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let mut s = String::from("SCRAI");
    for _ in 0..2 {
        s.push('-');
        for _ in 0..4 {
            s.push(ALPHA[rng.gen_range(0..ALPHA.len())] as char);
        }
    }
    s
}

/// Mint one invite code good for `uses` purchases; returns the code.
pub fn mint(conn: &Connection, uses: u32, note: &str) -> Result<String, String> {
    let code = new_code();
    conn.execute(
        "INSERT INTO codes (code, max_uses, uses, note, created) VALUES (?1, ?2, 0, ?3, ?4)",
        params![code, uses, note, now() as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(code)
}

#[derive(Debug, Clone)]
pub struct CodeRow {
    pub code: String,
    pub max_uses: u32,
    pub uses: u32,
    pub note: String,
    pub created: u64,
}

impl CodeRow {
    pub fn left(&self) -> u32 {
        self.max_uses.saturating_sub(self.uses)
    }
}

/// All invite codes, newest first.
pub fn list_codes(conn: &Connection) -> Result<Vec<CodeRow>, String> {
    let mut st = conn
        .prepare("SELECT code, max_uses, uses, note, created FROM codes ORDER BY created DESC")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(CodeRow {
                code: r.get(0)?,
                max_uses: r.get::<_, i64>(1)?.max(0) as u32,
                uses: r.get::<_, i64>(2)?.max(0) as u32,
                note: r.get(3)?,
                created: r.get::<_, i64>(4)?.max(0) as u64,
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unambiguous_and_mint_lists() {
        let c = new_code();
        assert_eq!(c.len(), 15);
        assert!(c.starts_with("SCRAI-"));
        assert!(!c[6..].contains(['0', 'O', '1', 'I', 'L']), "{c}"); // the SCRAI prefix has an I
        let dir = std::env::temp_dir().join(format!("scrai-faucet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = open_db(&dir.join("faucet.db")).unwrap();
        let code = mint(&conn, DEFAULT_CODE_USES, "alice").unwrap();
        let rows = list_codes(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].code, code);
        assert_eq!(rows[0].left(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
