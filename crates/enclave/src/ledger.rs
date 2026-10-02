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

use crate::state::{Sealing, Store};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How many changes are written down before the whole book is written out again. Small
/// enough that a restart replays in an instant, large enough that a chat does not rewrite
/// the book (see `state`).
const CHANGES_PER_SNAPSHOT: usize = 2_000;

/// The tables of the book, with the columns a snapshot carries. Everything the enclave
/// keeps is here: a table missing from this list would not survive a restart.
///
/// Not here: `nonces`. A request is sealed to the enclave's own key, which does not survive
/// a restart, so no request can be replayed across one — and a nonce written to the host's
/// disk on every request was two fsyncs per chat for nothing (2026-10-02).
const TABLES: &[(&str, &[&str])] = &[
    ("allowance", &["acct", "period", "ends_ms", "granted", "left"]),
    ("lots", &["id", "acct", "left", "expires_ms", "bought_ms"]),
    ("holds", &["id", "acct", "parts"]),
    ("plans", &["acct", "json"]),
    ("rails", &["rail", "acct"]),
    ("payments", &["ref", "at_ms"]),
    ("minted", &["ref", "fp"]),
    // Counts for the operator (`admin`): aggregates under a key, never an account.
    ("tally", &["key", "n"]),
];

/// A value as it travels in a journal record. The book writes nothing else.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum Val {
    I(i64),
    S(String),
}

impl rusqlite::ToSql for Val {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            Val::I(i) => i.to_sql(),
            Val::S(s) => s.to_sql(),
        }
    }
}

impl rusqlite::types::FromSql for Val {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Val> {
        match value {
            rusqlite::types::ValueRef::Integer(i) => Ok(Val::I(i)),
            rusqlite::types::ValueRef::Text(t) => Ok(Val::S(String::from_utf8_lossy(t).into_owned())),
            other => Err(rusqlite::types::FromSqlError::InvalidType).map_err(|e| {
                let _ = other;
                e
            }),
        }
    }
}

/// One change, as it is written down and replayed.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Change {
    sql: String,
    p: Vec<Val>,
}

/// What the book is kept in when the enclave has no disk: the host's sealed snapshot and
/// journal (`state`), and where we are in them.
///
/// The change is applied to the book in memory and sealed to its place at once, and then
/// handed to a thread of its own that carries it to the host. The caller does not wait:
/// a chat's hold and settle are written behind, and the host's disk is never on the path
/// of a request. Whoever needs a change to be on the disk before answering — a purchase,
/// a note, a plan — waits for its [`Mark`] (see `flushed`). Until 2026-10-02 every change
/// waited for the host under the one lock every request needs, so a slow disk was a
/// stopped enclave.
struct Kept {
    sealing: Sealing,
    generation: u64,
    /// The number the next record gets after the snapshot.
    next: u64,
    /// How many records there are since the snapshot.
    since: usize,
    /// The line to the thread that writes to the host.
    queue: std::sync::mpsc::Sender<Job>,
    flushed: Arc<Flushed>,
}

/// What the writer carries to the host, in order.
enum Job {
    Record { generation: u64, number: u64, sealed: Vec<u8> },
    Snapshot { generation: u64, sealed: Vec<u8> },
    /// Tell the witness where the book stands now, whatever the clock says.
    Witness,
}

/// How often the witness hears where the book stands while it moves (`crate::witness`).
const WITNESS_EVERY: Duration = Duration::from_secs(10 * 60);

/// A place in the line: (generation, number). Marks compare like the records they name.
pub type Mark = (u64, u64);

/// How far the host has confirmed the journal, for whoever waits on it.
pub struct Flushed {
    at: Mutex<Mark>,
    changed: Condvar,
}

impl Flushed {
    /// True once the host has confirmed every record up to `mark`; false if `within` ran
    /// out first. The change is still on its way: the writer never gives up on a record.
    pub fn wait(&self, mark: Mark, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        let mut at = match self.at.lock() {
            Ok(a) => a,
            Err(p) => p.into_inner(),
        };
        while *at < mark {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            at = match self.changed.wait_timeout(at, deadline - now) {
                Ok((a, _)) => a,
                Err(p) => p.into_inner().0,
            };
        }
        true
    }

    /// Where the host has got to.
    pub fn position(&self) -> Mark {
        match self.at.lock() {
            Ok(a) => *a,
            Err(p) => *p.into_inner(),
        }
    }

    fn reached(&self, mark: Mark) {
        if let Ok(mut at) = self.at.lock() {
            if *at < mark {
                *at = mark;
            }
        }
        self.changed.notify_all();
    }
}

/// How long the writer waits before offering a record to the host again after a refusal.
/// It never stops trying: the book in memory is right, and the host will take it when it
/// can. What it says meanwhile goes to the host log (`crate::voice`), once a minute.
const RETRY_AFTER: Duration = Duration::from_secs(2);

/// The thread that carries the book to the host. It takes jobs in order and does not move
/// on from one until the host has confirmed it, so a record never lands before the ones
/// in front of it — and a record the host already took (the answer was lost) is answered
/// again instead of written again (`Store::append`).
fn writer(store: Arc<dyn Store>, jobs: std::sync::mpsc::Receiver<Job>, flushed: Arc<Flushed>, witness: Option<crate::witness::Record>) {
    let mut last_said = std::time::Instant::now() - Duration::from_secs(60);
    // What the witness has been told, and what it should be told: after a fold, at once;
    // while the book moves, every WITNESS_EVERY; and whenever an earlier try failed.
    let mut told: Mark = (0, 0);
    let mut told_at = std::time::Instant::now();
    let mut owed = false;
    loop {
        let job = match jobs.recv_timeout(Duration::from_secs(30)) {
            Ok(job) => Some(job),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match job {
            Some(Job::Witness) => owed = true,
            Some(job) => {
                let (mark, what) = match &job {
                    Job::Record { generation, number, .. } => ((*generation, *number), "record"),
                    Job::Snapshot { generation, .. } => ((*generation, 0), "snapshot"),
                    Job::Witness => unreachable!("handled above"),
                };
                loop {
                    let result = match &job {
                        Job::Record { generation, number, sealed } => store.append(*generation, *number, sealed),
                        Job::Snapshot { sealed, .. } => store.put_snapshot(sealed),
                        Job::Witness => Ok(()),
                    };
                    match result {
                        Ok(()) => break,
                        Err(e) => {
                            if last_said.elapsed() >= Duration::from_secs(60) {
                                crate::voice::say(format!("book: the host did not take {what} {}/{}: {e} — trying again", mark.0, mark.1));
                                last_said = std::time::Instant::now();
                            }
                            std::thread::sleep(RETRY_AFTER);
                        }
                    }
                }
                flushed.reached(mark);
                if matches!(job, Job::Snapshot { .. }) {
                    owed = true;
                }
            }
            None => {}
        }
        let Some(witness) = &witness else { continue };
        let at = match flushed.at.lock() {
            Ok(a) => *a,
            Err(p) => *p.into_inner(),
        };
        if at > told && (owed || told_at.elapsed() >= WITNESS_EVERY) {
            match witness(at) {
                Ok(()) => {
                    told = at;
                    told_at = std::time::Instant::now();
                    owed = false;
                }
                Err(e) => {
                    owed = true;
                    if last_said.elapsed() >= Duration::from_secs(60) {
                        crate::voice::say(format!("witness: the mark {}/{} could not be written: {e} — trying again", at.0, at.1));
                        last_said = std::time::Instant::now();
                    }
                }
            }
        }
    }
}

/// How long a prepaid lot is valid: three years from purchase, by the calendar.
pub const PREPAID_MONTHS: u32 = 36;

pub struct Ledger {
    conn: Connection,
    data_key: [u8; 32],
    /// Where the book outlives the process, when it is not simply a file (see `state`).
    kept: Option<Mutex<Kept>>,
    replayed: usize,
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
             CREATE TABLE IF NOT EXISTS nonces (nonce TEXT PRIMARY KEY, ts_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS plans (acct TEXT PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS rails (rail TEXT PRIMARY KEY, acct TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS payments (ref TEXT PRIMARY KEY, at_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS minted (ref TEXT PRIMARY KEY, fp TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tally (key TEXT PRIMARY KEY, n INTEGER NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Ledger { conn, data_key, kept: None, replayed: 0 })
    }

    /// The book kept on the host, sealed: the snapshot is read back, the journal replayed,
    /// and from then on every change is written down behind the request that made it.
    /// With a witness, a book that stands before the last mark the witness saw is refused
    /// (`crate::witness::may_start`).
    pub fn open_sealed(store: Arc<dyn Store>, data_key: [u8; 32], witness: Option<crate::witness::Setup>) -> Result<Ledger, String> {
        let mut ledger = Self::with(Connection::open_in_memory().map_err(|e| e.to_string())?, data_key)?;
        let sealing = Sealing::new(&data_key);
        let snapshot = store.snapshot()?;
        let mut generation = 1;
        if !snapshot.is_empty() {
            let plain = sealing.open(0, 0, &snapshot)?;
            let book: serde_json::Value = serde_json::from_slice(&plain).map_err(|e| format!("the book's snapshot is unreadable: {e}"))?;
            generation = book["generation"].as_u64().ok_or("the book's snapshot has no generation")?;
            ledger.restore(&book["rows"])?;
        }
        let journal = crate::state::split(&store.journal()?)?;
        for (i, record) in journal.records.iter().enumerate() {
            let plain = sealing.open(generation, i as u64 + 1, record)?;
            let change: Change = serde_json::from_slice(&plain).map_err(|e| format!("a record of the book is unreadable: {e}"))?;
            ledger
                .conn
                .execute(&change.sql, params_from_iter(change.p.iter()))
                .map_err(|e| format!("the book's record {} does not apply: {e}", i + 1))?;
        }
        let replayed = journal.records.len();
        ledger.replayed = replayed;
        let position: Mark = (generation, replayed as u64);
        let (seen, record) = match witness {
            Some(w) => (Some(w.seen), Some(w.record)),
            None => (None, None),
        };
        let accepted = match &seen {
            Some(seen) => crate::witness::may_start(position, seen)?,
            None => false,
        };
        let flushed = Arc::new(Flushed { at: Mutex::new(position), changed: Condvar::new() });
        let (queue, jobs) = std::sync::mpsc::channel();
        {
            let flushed = flushed.clone();
            std::thread::Builder::new().name("book".into()).spawn(move || writer(store, jobs, flushed, record)).map_err(|e| format!("no thread for the book: {e}"))?;
        }
        if accepted {
            // An acknowledged restore: the witness hears the new position at once, so the
            // acknowledgement is spent (`crate::witness`).
            let _ = queue.send(Job::Witness);
        }
        ledger.kept = Some(Mutex::new(Kept { sealing, generation, next: replayed as u64 + 1, since: replayed, queue, flushed }));
        if snapshot.is_empty() || journal.legacy {
            // A book that starts from nothing gets its first snapshot at once, so that its
            // generation is on the host's disk before anything is written down. A journal
            // in the old form is folded for the same reason: what follows carries numbers.
            ledger.write_out()?;
        }
        Ok(ledger)
    }

    /// The place of the last change written down. Whoever must not answer before it is on
    /// the host's disk waits for it with [`Ledger::flushed`], outside the ledger's lock.
    pub fn mark(&self) -> Mark {
        match &self.kept {
            Some(kept) => kept.lock().map(|k| (k.generation, k.next - 1)).unwrap_or((0, 0)),
            None => (0, 0),
        }
    }

    /// Where the host has got to, to wait on. None for a book that is not kept on a host:
    /// such a book is on its own disk (or in memory) the moment the change is made.
    pub fn flushed(&self) -> Option<Arc<Flushed>> {
        self.kept.as_ref().and_then(|k| k.lock().ok().map(|k| k.flushed.clone()))
    }

    /// Wait until everything written down so far is on the host's disk. For the tests and
    /// for a stop that wants to leave nothing behind.
    pub fn flush(&self, within: Duration) -> Result<(), String> {
        let (Some(flushed), mark) = (self.flushed(), self.mark()) else { return Ok(()) };
        if flushed.wait(mark, within) {
            Ok(())
        } else {
            Err(format!("the host has not confirmed the book up to record {}/{} within {}s", mark.0, mark.1, within.as_secs()))
        }
    }

    /// How many changes were replayed at the last start — what the enclave says out loud,
    /// since nobody can look inside it.
    pub fn replayed(&self) -> usize {
        self.replayed
    }

    /// Do one change and write it down: applied to the book in memory, sealed to its place,
    /// and handed to the writer. The journal on the host will say the same thing; a change
    /// that cannot even be sealed did not happen.
    fn change(&self, sql: &str, p: Vec<Val>) -> Result<usize, String> {
        let n = self.conn.execute(sql, params_from_iter(p.iter())).map_err(|e| e.to_string())?;
        self.write_down(vec![Change { sql: sql.to_string(), p }])?;
        Ok(n)
    }

    /// Write changes down, in order, each sealed to its place in the line and queued for
    /// the host. Returns at once; `mark` says where the line now ends.
    fn write_down(&self, changes: Vec<Change>) -> Result<(), String> {
        let Some(kept) = &self.kept else { return Ok(()) };
        let mut full = false;
        {
            let mut kept = kept.lock().map_err(|_| "the book's journal is in an unknown state".to_string())?;
            for change in changes {
                let record = serde_json::to_vec(&change).map_err(|e| e.to_string())?;
                let sealed = kept.sealing.seal(kept.generation, kept.next, &record)?;
                kept.queue
                    .send(Job::Record { generation: kept.generation, number: kept.next, sealed })
                    .map_err(|_| "the book's writer is gone".to_string())?;
                kept.next += 1;
                kept.since += 1;
            }
            full = full || kept.since >= CHANGES_PER_SNAPSHOT;
        }
        if full {
            self.write_out()?;
        }
        Ok(())
    }

    /// Write the whole book out as one snapshot and start a new generation, so the journal
    /// stays short and a restart is quick. Queued behind the records it folds in, so the
    /// host sees them before it sees the snapshot that replaces them.
    fn write_out(&self) -> Result<(), String> {
        let Some(kept) = &self.kept else { return Ok(()) };
        let rows = self.rows()?;
        let mut kept = kept.lock().map_err(|_| "the book's journal is in an unknown state".to_string())?;
        let generation = kept.generation + 1;
        let book = serde_json::json!({ "generation": generation, "rows": rows });
        let sealed = kept.sealing.seal(0, 0, &serde_json::to_vec(&book).map_err(|e| e.to_string())?)?;
        kept.queue.send(Job::Snapshot { generation, sealed }).map_err(|_| "the book's writer is gone".to_string())?;
        kept.generation = generation;
        kept.next = 1;
        kept.since = 0;
        Ok(())
    }

    /// Every row of the book, table by table.
    fn rows(&self) -> Result<serde_json::Value, String> {
        let mut all = serde_json::Map::new();
        for (table, columns) in TABLES {
            let mut st = self.conn.prepare(&format!("SELECT {} FROM {table}", columns.join(", "))).map_err(|e| e.to_string())?;
            let rows = st
                .query_map([], |r| (0..columns.len()).map(|i| r.get::<_, Val>(i)).collect::<rusqlite::Result<Vec<Val>>>())
                .map_err(|e| e.to_string())?;
            let rows: Vec<Vec<Val>> = rows.collect::<rusqlite::Result<_>>().map_err(|e| e.to_string())?;
            all.insert((*table).to_string(), serde_json::to_value(rows).map_err(|e| e.to_string())?);
        }
        Ok(serde_json::Value::Object(all))
    }

    /// Put the rows of a snapshot back, in place of whatever is there.
    fn restore(&self, rows: &serde_json::Value) -> Result<(), String> {
        for (table, columns) in TABLES {
            self.conn.execute(&format!("DELETE FROM {table}"), []).map_err(|e| e.to_string())?;
            let Some(list) = rows.get(table).and_then(|v| v.as_array()) else { continue };
            let places: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
            let sql = format!("INSERT INTO {table} ({}) VALUES ({})", columns.join(", "), places.join(", "));
            for row in list {
                let values: Vec<Val> = serde_json::from_value(row.clone()).map_err(|e| format!("a row of {table} is unreadable: {e}"))?;
                self.conn.execute(&sql, params_from_iter(values.iter())).map_err(|e| format!("a row of {table} does not go back in: {e}"))?;
            }
        }
        Ok(())
    }

    fn key(&self, account_id: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"tokumai/ledger/acct/v1");
        h.update(self.data_key);
        h.update(account_id.as_bytes());
        hex::encode(h.finalize())
    }

    /// The name an account is stored under. The plan logic works in these names, so that
    /// even code walking every plan (the renewal check) never holds an account id.
    pub(crate) fn acct_key(&self, account_id: &str) -> String {
        self.key(account_id)
    }

    /// A payment reference (Stripe subscription, Apple transaction) as stored in the lookup
    /// table: a keyed hash, so the table alone connects no payment to anything (audit
    /// 2026-09-21, M13: the first server kept Apple's raw number, and kept it for good).
    fn rail_key(&self, rail: &str) -> String {
        let mut h = Sha256::new();
        h.update(b"tokumai/ledger/rail/v1");
        h.update(self.data_key);
        h.update(rail.as_bytes());
        hex::encode(h.finalize())
    }

    /// A one-off payment (an App Store consumable) is credited once: false if this reference
    /// was seen before. Stored as a keyed hash, like every payment reference.
    pub fn first_payment(&self, reference: &str, now_ms: u64) -> Result<bool, String> {
        let n = self.change("INSERT OR IGNORE INTO payments (ref, at_ms) VALUES (?1, ?2)", vec![Val::S(self.rail_key(reference)), Val::I(now_ms as i64)])?;
        Ok(n == 1)
    }

    // ---- blind notes (see `notes`) ------------------------------------------------

    /// What was minted for a payment's month: the fingerprint of the blinded message the
    /// enclave signed, under a keyed hash of the payment reference. Nothing here names an
    /// account, and the fingerprint names no note.
    pub fn minted_get(&self, reference: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row("SELECT fp FROM minted WHERE ref = ?1", params![self.rail_key(reference)], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn minted_put(&self, reference: &str, fingerprint: &str) -> Result<(), String> {
        self.change("INSERT OR IGNORE INTO minted (ref, fp) VALUES (?1, ?2)", vec![Val::S(self.rail_key(reference)), Val::S(fingerprint.into())]).map(|_| ())
    }

    // ---- plans (see `plans`) ------------------------------------------------------

    pub(crate) fn plan_get(&self, key: &str) -> Result<Option<crate::plans::Plan>, String> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT json FROM plans WHERE acct = ?1", params![key], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        json.map(|j| serde_json::from_str(&j).map_err(|e| e.to_string())).transpose()
    }

    pub(crate) fn plan_put(&self, key: &str, plan: &crate::plans::Plan) -> Result<(), String> {
        let json = serde_json::to_string(plan).map_err(|e| e.to_string())?;
        self.change("INSERT INTO plans (acct, json) VALUES (?1, ?2) ON CONFLICT(acct) DO UPDATE SET json = excluded.json", vec![Val::S(key.into()), Val::S(json)])
            .map(|_| ())
    }

    pub(crate) fn plan_delete(&self, key: &str) -> Result<(), String> {
        self.change("DELETE FROM plans WHERE acct = ?1", vec![Val::S(key.into())]).map(|_| ())
    }

    pub(crate) fn plan_keys(&self) -> Result<Vec<String>, String> {
        let mut st = self.conn.prepare("SELECT acct FROM plans").map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// Which account (stored name) a payment reference belongs to. Kept for every reference
    /// ever attached, not only the current one: a replaced subscription must not be carried
    /// to a fresh account for another month (audit H3).
    pub(crate) fn rail_owner(&self, rail: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row("SELECT acct FROM rails WHERE rail = ?1", params![self.rail_key(rail)], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub(crate) fn rail_bind(&self, rail: &str, key: &str) -> Result<(), String> {
        self.change(
            "INSERT INTO rails (rail, acct) VALUES (?1, ?2) ON CONFLICT(rail) DO UPDATE SET acct = excluded.acct",
            vec![Val::S(self.rail_key(rail)), Val::S(key.into())],
        )
        .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn debug_dump_rails(&self) -> Vec<String> {
        let mut st = self.conn.prepare("SELECT rail || ' ' || acct FROM rails").unwrap();
        let rows = st.query_map([], |r| r.get(0)).unwrap();
        rows.flatten().collect()
    }

    /// (period start in seconds, ends at, granted, left) of an account's allowance.
    pub(crate) fn allowance_of(&self, key: &str) -> Result<Option<(u32, u64, u64, u64)>, String> {
        self.conn
            .query_row("SELECT period, ends_ms, granted, left FROM allowance WHERE acct = ?1", params![key], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64))
            })
            .optional()
            .map_err(|e| e.to_string())
    }

    pub(crate) fn allowance_set(&self, key: &str, start_ms: u64, ends_ms: u64, toku: u64) -> Result<(), String> {
        self.change(
            "INSERT INTO allowance (acct, period, ends_ms, granted, left) VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(acct) DO UPDATE SET period = excluded.period, ends_ms = excluded.ends_ms,
                     granted = excluded.granted, left = excluded.left",
            vec![Val::S(key.into()), Val::I((start_ms / 1000) as i64), Val::I(ends_ms as i64), Val::I(toku as i64)],
        )
        .map(|_| ())
    }

    pub(crate) fn allowance_add(&self, key: &str, extra: u64) -> Result<(), String> {
        self.change("UPDATE allowance SET granted = granted + ?2, left = left + ?2 WHERE acct = ?1", vec![Val::S(key.into()), Val::I(extra as i64)])
            .map(|_| ())
    }

    pub(crate) fn allowance_take(&self, key: &str, amount: u64) -> Result<(), String> {
        self.change("UPDATE allowance SET left = MAX(0, left - ?2) WHERE acct = ?1", vec![Val::S(key.into()), Val::I(amount as i64)])
            .map(|_| ())
    }

    pub(crate) fn allowance_lapse(&self, key: &str) -> Result<(), String> {
        self.change("UPDATE allowance SET granted = 0, left = 0 WHERE acct = ?1", vec![Val::S(key.into())]).map(|_| ())
    }

    /// A prepaid purchase: a new lot, valid three years from `now_ms`.
    pub fn credit_prepaid(&self, account_id: &str, toku: u64, now_ms: u64) -> Result<(), String> {
        let expires = tokumai_core::subscription::add_months_ms(now_ms, PREPAID_MONTHS);
        let key = self.key(account_id);
        self.conn
            .execute("INSERT INTO lots (acct, left, expires_ms, bought_ms) VALUES (?1, ?2, ?3, ?4)", params![key, toku as i64, expires as i64, now_ms as i64])
            .map_err(|e| e.to_string())?;
        // Written down with the id it was given: later records name that lot, and a replay
        // must find the same one.
        let id = self.conn.last_insert_rowid();
        self.write_down(vec![Change {
            sql: "INSERT INTO lots (id, acct, left, expires_ms, bought_ms) VALUES (?1, ?2, ?3, ?4, ?5)".into(),
            p: vec![Val::I(id), Val::S(key), Val::I(toku as i64), Val::I(expires as i64), Val::I(now_ms as i64)],
        }])
    }

    /// A plan period begins: its allowance is SET, never added to.
    pub fn grant_allowance(&self, account_id: &str, start_ms: u64, ends_ms: u64, toku: u64) -> Result<(), String> {
        self.allowance_set(&self.key(account_id), start_ms, ends_ms, toku)
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
        // What this hold does to the book, to be written down once it holds (see `state`).
        let mut written: Vec<Change> = Vec::new();
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
                written.push(Change { sql: "UPDATE allowance SET left = left - ?2 WHERE acct = ?1".into(), p: vec![Val::S(k.clone()), Val::I(take as i64)] });
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
                written.push(Change { sql: "UPDATE lots SET left = left - ?2 WHERE id = ?1".into(), p: vec![Val::I(id), Val::I(take as i64)] });
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
        written.push(Change { sql: "INSERT INTO holds (id, acct, parts) VALUES (?1, ?2, ?3)".into(), p: vec![Val::I(id), Val::S(k), Val::S(json)] });
        tx.commit().map_err(|e| e.to_string())?;
        // If the host cannot take it, the request does not go ahead; what the hold took is
        // given back at the next start, as any hold left open is.
        self.write_down(written)?;
        Ok(Hold { id, account: account_id.to_string(), amount, parts })
    }

    /// The request is answered: keep `cost` (at most what was held) and give the rest back,
    /// last pocket first, so what was spent came out of the allowance first.
    pub fn settle(&mut self, hold: Hold, cost: u64) -> Result<u64, String> {
        let kept = cost.min(hold.amount);
        let mut refund = hold.amount - kept;
        let acct = self.key(&hold.account);
        let mut written: Vec<Change> = Vec::new();
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        for (pocket, took) in hold.parts.iter().rev() {
            if refund == 0 {
                break;
            }
            let back = refund.min(*took);
            written.push(Self::give_back(&tx, &acct, *pocket, back)?);
            refund -= back;
        }
        tx.execute("DELETE FROM holds WHERE id = ?1", params![hold.id]).map_err(|e| e.to_string())?;
        written.push(Change { sql: "DELETE FROM holds WHERE id = ?1".into(), p: vec![Val::I(hold.id)] });
        tx.commit().map_err(|e| e.to_string())?;
        self.write_down(written)?;
        Ok(kept)
    }

    /// Put `amount` back where it came from, and say what was done so it can be written down.
    fn give_back(tx: &rusqlite::Transaction, acct: &str, pocket: Pocket, amount: u64) -> Result<Change, String> {
        let change = match pocket {
            Pocket::Allowance => Change {
                sql: "UPDATE allowance SET left = left + ?2 WHERE acct = ?1".into(),
                p: vec![Val::S(acct.to_string()), Val::I(amount as i64)],
            },
            Pocket::Lot(id) => Change { sql: "UPDATE lots SET left = left + ?2 WHERE id = ?1".into(), p: vec![Val::I(id), Val::I(amount as i64)] },
        };
        tx.execute(&change.sql, params_from_iter(change.p.iter())).map_err(|e| e.to_string())?;
        Ok(change)
    }

    /// At start: holds left by requests that died with the last process go back in full.
    pub fn release_open_holds(&mut self) -> Result<usize, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let open: Vec<(i64, String, String)> = {
            let mut st = tx.prepare("SELECT id, acct, parts FROM holds").map_err(|e| e.to_string())?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(|e| e.to_string())?;
            rows.flatten().collect()
        };
        let mut written: Vec<Change> = Vec::new();
        for (id, acct, parts) in &open {
            let parts: Vec<(Pocket, u64)> = serde_json::from_str(parts).map_err(|e| e.to_string())?;
            for (pocket, took) in parts {
                written.push(Self::give_back(&tx, acct, pocket, took)?);
            }
            tx.execute("DELETE FROM holds WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
            written.push(Change { sql: "DELETE FROM holds WHERE id = ?1".into(), p: vec![Val::I(*id)] });
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.write_down(written)?;
        Ok(open.len())
    }

    /// How many records the journal holds since the last snapshot.
    pub fn since_snapshot(&self) -> usize {
        self.kept.as_ref().and_then(|k| k.lock().ok().map(|k| k.since)).unwrap_or(0)
    }

    // ---- counts for the operator (see `admin`) ---------------------------------------

    pub fn tally_add(&self, key: &str, n: i64) -> Result<(), String> {
        self.change("INSERT INTO tally (key, n) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET n = n + excluded.n", vec![Val::S(key.into()), Val::I(n)]).map(|_| ())
    }

    pub fn tally_read(&self, prefix: &str) -> Result<Vec<(String, i64)>, String> {
        let mut st = self.conn.prepare("SELECT key, n FROM tally WHERE key >= ?1 AND key < ?2").map_err(|e| e.to_string())?;
        let end = format!("{prefix}\u{10FFFF}");
        let rows = st.query_map(params![prefix, end], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// Every plan, without the names they are stored under.
    pub(crate) fn plans_all(&self) -> Result<Vec<crate::plans::Plan>, String> {
        let mut st = self.conn.prepare("SELECT json FROM plans").map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
        Ok(rows.flatten().filter_map(|j| serde_json::from_str(&j).ok()).collect())
    }

    /// (accounts the book knows, prepaid lots with something left, TOKU left in them).
    pub fn account_counts(&self) -> Result<(u64, u64, u64), String> {
        let accounts: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM (SELECT acct FROM allowance UNION SELECT acct FROM plans UNION SELECT acct FROM lots)", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let (lots, left): (i64, i64) = self
            .conn
            .query_row("SELECT COUNT(*), COALESCE(SUM(left), 0) FROM lots WHERE left > 0", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| e.to_string())?;
        Ok((accounts as u64, lots as u64, left as u64))
    }

    /// Records that change nothing, of given sizes (the sealed JSON of the change, before
    /// the seal's own bytes): what a ghost redemption writes (`ghost`). Replayed, they
    /// update no row.
    pub fn ghost_records(&self, sizes: &[usize]) -> Result<(), String> {
        const SQL: &str = "UPDATE tally SET n = n WHERE key = ?1";
        for size in sizes {
            // {"sql":"…","p":["ghost:…"]} — pad the parameter to the size asked for.
            let base = serde_json::to_vec(&Change { sql: SQL.into(), p: vec![Val::S("ghost:".into())] }).map(|v| v.len()).unwrap_or(0);
            let pad = size.saturating_sub(base);
            self.change(SQL, vec![Val::S(format!("ghost:{}", "x".repeat(pad)))])?;
        }
        Ok(())
    }

    pub fn holds_open(&self) -> Result<u64, String> {
        self.conn.query_row("SELECT COUNT(*) FROM holds", [], |r| r.get::<_, i64>(0)).map(|n| n as u64).map_err(|e| e.to_string())
    }

    /// Record a request nonce. False if it was seen before (a replay, or a resend). In
    /// memory only: see `TABLES` for why the host never hears of a nonce.
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

    /// The book lives in memory inside the enclave; what the host keeps of it must bring it
    /// back exactly — balances, the plan, the payments already credited.
    #[test]
    fn what_the_host_keeps_brings_the_book_back() {
        let dir = std::env::temp_dir().join(format!("tokumai-book-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(crate::state::FileStore::new(&dir).unwrap());
        let now = 1_790_000_000_000;

        let mut first = Ledger::open_sealed(store.clone(), [4u8; 32], None).unwrap();
        first.grant_allowance("acct", now, now + 30 * DAY, 700_000).unwrap();
        first.credit_prepaid("acct", 50_000, now).unwrap();
        assert!(first.first_payment("stripe-sub-1", now).unwrap());
        let hold = first.hold("acct", 20_000, now).unwrap();
        assert_eq!(first.settle(hold, 12_000).unwrap(), 12_000);
        // One that is still open when the enclave goes away.
        let _open = first.hold("acct", 5_000, now).unwrap();
        let before = first.balance("acct", now).unwrap();
        first.flush(Duration::from_secs(5)).unwrap();
        drop(first);

        // A new enclave, the same host: the snapshot and the journal are all it has.
        let mut again = Ledger::open_sealed(store.clone(), [4u8; 32], None).unwrap();
        assert!(again.replayed() > 0, "the journal should have carried the changes");
        assert_eq!(again.balance("acct", now).unwrap(), before);
        // The payment is still known, so a second delivery of it credits nothing twice.
        assert!(!again.first_payment("stripe-sub-1", now).unwrap());
        // And the hold nobody settled comes back to the account.
        assert_eq!(again.release_open_holds().unwrap(), 1);
        assert_eq!(again.balance("acct", now).unwrap().total, before.total + 5_000);

        // A third time, to be sure the replay of a replay is the same.
        let total = again.balance("acct", now).unwrap().total;
        again.flush(Duration::from_secs(5)).unwrap();
        drop(again);
        let third = Ledger::open_sealed(store.clone(), [4u8; 32], None).unwrap();
        assert_eq!(third.balance("acct", now).unwrap().total, total);

        // Another key does not open it, and a bent record is not quietly skipped.
        drop(third);
        assert!(Ledger::open_sealed(store.clone(), [5u8; 32], None).is_err());
        let journal = dir.join("book.journal");
        let mut bytes = std::fs::read(&journal).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&journal, &bytes).unwrap();
        assert!(Ledger::open_sealed(store, [4u8; 32], None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When the journal grows past its mark the whole book is written out again, and what
    /// it says stays the same.
    #[test]
    fn a_long_journal_is_folded_into_a_snapshot() {
        let dir = std::env::temp_dir().join(format!("tokumai-fold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(crate::state::FileStore::new(&dir).unwrap());
        let now = 1_790_000_000_000;
        let book = Ledger::open_sealed(store.clone(), [6u8; 32], None).unwrap();
        book.grant_allowance("acct", now, now + 30 * DAY, 1_000).unwrap();
        for i in 0..CHANGES_PER_SNAPSHOT {
            assert!(book.first_payment(&format!("payment-{i}"), now).unwrap());
        }
        book.flush(Duration::from_secs(10)).unwrap();
        assert!(crate::state::split(&store.journal().unwrap()).unwrap().records.len() < CHANGES_PER_SNAPSHOT, "the journal should have been folded in");
        drop(book);

        let again = Ledger::open_sealed(store, [6u8; 32], None).unwrap();
        assert_eq!(again.balance("acct", now).unwrap().allowance, 1_000);
        // A payment seen before the snapshot is still a payment seen before.
        assert!(!again.first_payment("payment-7", now).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A nonce is a guard against a replay within one life of the enclave, and only that:
    /// the host is not told about it, so the journal does not grow by two records per chat.
    #[test]
    fn a_nonce_is_not_written_down() {
        let dir = std::env::temp_dir().join(format!("tokumai-nonce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(crate::state::FileStore::new(&dir).unwrap());
        let book = Ledger::open_sealed(store.clone(), [6u8; 32], None).unwrap();
        assert!(book.first_sight("n", 1_000).unwrap());
        assert!(!book.first_sight("n", 2_000).unwrap());
        book.flush(Duration::from_secs(5)).unwrap();
        assert!(store.journal().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The host said nothing, or said it late: the record is offered again, and the host,
    /// which had taken it, does not take it twice. The next start accounts for everything.
    #[test]
    fn a_record_the_host_took_but_did_not_confirm_is_not_written_twice() {
        struct Flaky {
            inner: crate::state::FileStore,
            fail_next: std::sync::atomic::AtomicBool,
        }
        impl Store for Flaky {
            fn snapshot(&self) -> Result<Vec<u8>, String> {
                self.inner.snapshot()
            }
            fn put_snapshot(&self, sealed: &[u8]) -> Result<(), String> {
                self.inner.put_snapshot(sealed)
            }
            fn journal(&self) -> Result<Vec<u8>, String> {
                self.inner.journal()
            }
            fn append(&self, generation: u64, number: u64, record: &[u8]) -> Result<(), String> {
                self.inner.append(generation, number, record)?;
                if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Err("the vsock went quiet after the write".into());
                }
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("tokumai-flaky-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(Flaky { inner: crate::state::FileStore::new(&dir).unwrap(), fail_next: std::sync::atomic::AtomicBool::new(false) });
        let now = 1_790_000_000_000;
        let book = Ledger::open_sealed(store.clone(), [7u8; 32], None).unwrap();
        book.flush(Duration::from_secs(5)).unwrap();
        store.fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
        book.credit_prepaid("acct", 1_000, now).unwrap(); // this one lands, unconfirmed
        book.credit_prepaid("acct", 1_000, now).unwrap();
        book.flush(Duration::from_secs(10)).unwrap();
        assert_eq!(crate::state::split(&store.journal().unwrap()).unwrap().records.len(), 2);
        drop(book);
        let again = Ledger::open_sealed(store, [7u8; 32], None).unwrap();
        assert_eq!(again.balance("acct", now).unwrap().total, 2_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A book that stands before the witness's last mark does not start; an acknowledged
    /// restore does, and tells the witness at once.
    #[test]
    fn a_rewound_book_is_refused_and_an_acknowledged_restore_tells_the_witness() {
        use crate::witness::{Setup, Witnessed};
        let dir = std::env::temp_dir().join(format!("tokumai-witness-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(crate::state::FileStore::new(&dir).unwrap());
        let told: Arc<Mutex<Vec<Mark>>> = Arc::new(Mutex::new(Vec::new()));
        let record: crate::witness::Record = {
            let told = told.clone();
            Arc::new(move |m: Mark| {
                told.lock().unwrap().push(m);
                Ok(())
            })
        };
        let now = 1_790_000_000_000;
        // A fresh book: its first snapshot is a fold, and the witness hears of it.
        let book = Ledger::open_sealed(store.clone(), [7u8; 32], Some(Setup { seen: Witnessed::default(), record: record.clone() })).unwrap();
        book.credit_prepaid("acct", 10, now).unwrap();
        book.flush(Duration::from_secs(5)).unwrap();
        for _ in 0..50 {
            if !told.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let first = *told.lock().unwrap().first().expect("the fold was witnessed");
        assert_eq!(first.0, 2, "the fold made generation 2");
        drop(book);
        // The witness now says generation 2 (and a record): the same book is fine.
        let seen = Witnessed { mark: Some(((2, 1), 1_000)), accepts: vec![] };
        assert!(Ledger::open_sealed(store.clone(), [7u8; 32], Some(Setup { seen, record: record.clone() })).is_ok());
        // A witness that has seen further than this book: refused.
        let ahead = Witnessed { mark: Some(((3, 0), 1_000)), accepts: vec![] };
        let refused = Ledger::open_sealed(store.clone(), [7u8; 32], Some(Setup { seen: ahead, record: record.clone() }));
        assert!(refused.err().unwrap_or_default().contains("older book"));
        // Acknowledged for this very position, after the mark: it starts and tells the witness.
        told.lock().unwrap().clear();
        let acked = Witnessed { mark: Some(((3, 0), 1_000)), accepts: vec![((2, 1), 2_000)] };
        let book = Ledger::open_sealed(store, [7u8; 32], Some(Setup { seen: acked, record })).unwrap();
        for _ in 0..50 {
            if !told.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(told.lock().unwrap().first().copied(), Some((2, 1)), "the accepted position was witnessed at once");
        drop(book);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The journals the first enclaves wrote carry no numbers. One is read as before, and
    /// folded into a snapshot at once, so that the host's journal is in the new form from
    /// then on.
    #[test]
    fn a_journal_in_the_old_form_is_replayed_and_folded() {
        let dir = std::env::temp_dir().join(format!("tokumai-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = [8u8; 32];
        let sealing = Sealing::new(&key);
        let now = 1_790_000_000_000;
        // What such an enclave left behind: a snapshot of generation 3, and two records.
        let snapshot = serde_json::json!({ "generation": 3, "rows": { "allowance": [[ "k", 1, now + DAY, 500, 500 ]] } });
        std::fs::write(dir.join("book.snapshot"), sealing.seal(0, 0, &serde_json::to_vec(&snapshot).unwrap()).unwrap()).unwrap();
        let mut journal = Vec::new();
        for (i, change) in [
            Change { sql: "UPDATE allowance SET left = left - ?2 WHERE acct = ?1".into(), p: vec![Val::S("k".into()), Val::I(100)] },
            Change { sql: "INSERT OR IGNORE INTO nonces (nonce, ts_ms) VALUES (?1, ?2)".into(), p: vec![Val::S("old".into()), Val::I(now as i64)] },
        ]
        .iter()
        .enumerate()
        {
            let sealed = sealing.seal(3, i as u64 + 1, &serde_json::to_vec(change).unwrap()).unwrap();
            journal.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
            journal.extend_from_slice(&sealed);
        }
        std::fs::write(dir.join("book.journal"), &journal).unwrap();

        let store = Arc::new(crate::state::FileStore::new(&dir).unwrap());
        let book = Ledger::open_sealed(store.clone(), key, None).unwrap();
        assert_eq!(book.replayed(), 2);
        let left: i64 = book.conn.query_row("SELECT left FROM allowance WHERE acct = 'k'", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 400);
        book.flush(Duration::from_secs(5)).unwrap();
        assert!(store.journal().unwrap().is_empty(), "folded: the old journal is gone");
        assert_eq!(book.mark().0, 4, "a new generation");
        book.credit_prepaid("acct", 10, now).unwrap();
        book.flush(Duration::from_secs(5)).unwrap();
        assert!(store.journal().unwrap().starts_with(crate::state::MAGIC));
        drop(book);
        let again = Ledger::open_sealed(store, key, None).unwrap();
        assert_eq!(again.balance("acct", now).unwrap().total, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

}
