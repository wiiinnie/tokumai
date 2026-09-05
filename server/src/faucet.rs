//! Shared bits of the invite/faucet ledger (`faucet.db`), used by the `scrai-faucet`
//! binary (claims), by `scrai-admin` (minting invite codes, listing them) and — read-only
//! — by the server, which asks whether a code is worth raising a $1 invoice for. The
//! ledger lives next to `state.db`.

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

/// `TOKU-XXXX-XXXX` from an alphabet without look-alikes (no 0/O, 1/I/L).
pub fn new_code() -> String {
    use rand::Rng;
    const ALPHA: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let mut s = String::from("TOKU");
    for _ in 0..2 {
        s.push('-');
        for _ in 0..4 {
            s.push(ALPHA[rng.gen_range(0..ALPHA.len())] as char);
        }
    }
    s
}

/// Shape of an invite code as it comes off the wire, before any lookup: uppercase,
/// digits and hyphens, bounded. Rejects the obvious junk without touching the database.
pub fn looks_like_code(s: &str) -> bool {
    (8..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

/// Read-only: does this code exist and have a use left?
///
/// The SERVER asks this to decide whether to offer the $1 invite tile and raise the
/// invoice. It is deliberately NOT the money decision — the faucet binary re-checks and
/// consumes the use behind its own UNIQUE-insert lock at the moment it pays, so two
/// invoices raised against one code still buy exactly one claim. Anything unexpected
/// (missing file, locked database, unknown code) reads as "no": fail closed.
pub fn code_has_uses_left(faucet_db: &Path, code: &str) -> bool {
    use rusqlite::OpenFlags;
    if !looks_like_code(code) {
        return false;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = Connection::open_with_flags(faucet_db, flags) else { return false };
    let _ = conn.busy_timeout(std::time::Duration::from_millis(250));
    conn.query_row("SELECT max_uses, uses FROM codes WHERE code = ?1", [code], |r| {
        Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?))
    })
    .map(|(max, used)| used < max)
    .unwrap_or(false)
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
        assert_eq!(c.len(), 14); // TOKU-XXXX-XXXX
        assert!(c.starts_with("TOKU-"));
        assert!(!c[5..].contains(['0', 'O', '1', 'I', 'L']), "{c}"); // the TOKU prefix has an O
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
