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
        Ok(Store { conn })
    }

    /// The stored JSON blob for `key`, if any.
    pub fn load(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT v FROM kv WHERE k = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
    }

    /// Upsert the JSON blob for `key`.
    pub fn save(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO kv (k, v) VALUES (?1, ?2) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
