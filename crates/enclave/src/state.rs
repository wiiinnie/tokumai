//! Where the book lives when the enclave has no disk of its own.
//!
//! A Nitro enclave has no storage: what must outlive a restart goes to the host, sealed
//! under the enclave's data key — which KMS releases to this image and to nothing else.
//! The host keeps two things and understands neither:
//!
//! - a **snapshot**: every row of the book at one moment;
//! - a **journal**: every change since, one sealed record each, in order.
//!
//! On a restart the enclave reads the snapshot and replays the journal. Every record is
//! sealed to its place in the line — the snapshot's generation and the record's number —
//! so a record that was dropped from the middle, reordered, or kept from an older snapshot
//! does not open, and the enclave refuses to start on a book it cannot account for.
//!
//! What this does **not** prevent: the host can hand back an older snapshot and journal and
//! rewind the book as a whole (spent credit would be back). Nothing inside a Nitro enclave
//! survives a restart — no counter, no key — so a rewind looks exactly like a fresh start.
//! Closing that needs a counter the host cannot turn back, kept outside; it is on the list
//! for before launch (docs/enclave-phase0.md). Until then the operator is trusted for
//! freshness, never for content.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use sha2::{Digest, Sha256};

/// What the host keeps for the enclave. It only ever sees sealed bytes.
pub trait Store: Send + Sync {
    /// The snapshot, or empty when there is none yet.
    fn snapshot(&self) -> Result<Vec<u8>, String>;
    /// Put a new snapshot in place and drop the journal that led to it — together, so a
    /// restart in the middle finds either the old pair or the new one.
    fn put_snapshot(&self, sealed: &[u8]) -> Result<(), String>;
    /// Every journal record since the snapshot, in order.
    fn journal(&self) -> Result<Vec<Vec<u8>>, String>;
    /// Add one record, and do not return before it is safe on the host's disk.
    fn append(&self, record: &[u8]) -> Result<(), String>;
}

/// The sealing of one record or snapshot: ChaCha20-Poly1305 under a key of the book's own,
/// derived from the data key, with the record's place in the line as associated data.
pub struct Sealing(ChaCha20Poly1305);

impl Sealing {
    pub fn new(data_key: &[u8; 32]) -> Sealing {
        let mut h = Sha256::new();
        h.update(b"tokumai/ledger/state/v1");
        h.update(data_key);
        Sealing(ChaCha20Poly1305::new_from_slice(&h.finalize()).expect("32 bytes"))
    }

    /// `generation` is the snapshot this belongs to, `number` the record's place after it
    /// (0 for the snapshot itself).
    pub fn seal(&self, generation: u64, number: u64, plain: &[u8]) -> Result<Vec<u8>, String> {
        let nonce: [u8; 12] = rand::random();
        let ct = self
            .0
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: &place(generation, number) })
            .map_err(|_| "the book could not be sealed".to_string())?;
        Ok([nonce.as_slice(), &ct].concat())
    }

    pub fn open(&self, generation: u64, number: u64, sealed: &[u8]) -> Result<Vec<u8>, String> {
        if sealed.len() < 12 {
            return Err("a piece of the book is too short to be sealed".into());
        }
        let (nonce, ct) = sealed.split_at(12);
        self.0
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: &place(generation, number) })
            .map_err(|_| format!("the book's record {number} of generation {generation} does not open — it was changed, dropped or put out of order"))
    }
}

fn place(generation: u64, number: u64) -> Vec<u8> {
    format!("tokumai/ledger/{generation}/{number}").into_bytes()
}

/// A store in a directory, for a developer's machine and for the tests: the same two files
/// the host keeps, in the same shape.
pub struct FileStore {
    dir: std::path::PathBuf,
}

impl FileStore {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Result<FileStore, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(FileStore { dir })
    }

    fn snapshot_path(&self) -> std::path::PathBuf {
        self.dir.join("book.snapshot")
    }

    fn journal_path(&self) -> std::path::PathBuf {
        self.dir.join("book.journal")
    }
}

impl Store for FileStore {
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(std::fs::read(self.snapshot_path()).unwrap_or_default())
    }

    fn put_snapshot(&self, sealed: &[u8]) -> Result<(), String> {
        let tmp = self.dir.join("book.snapshot.new");
        std::fs::write(&tmp, sealed).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, self.snapshot_path()).map_err(|e| e.to_string())?;
        // The journal belongs to the snapshot it followed: it goes when that one does.
        match std::fs::remove_file(self.journal_path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn journal(&self) -> Result<Vec<Vec<u8>>, String> {
        split(&std::fs::read(self.journal_path()).unwrap_or_default())
    }

    fn append(&self, record: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.journal_path()).map_err(|e| e.to_string())?;
        f.write_all(&framed(record)).map_err(|e| e.to_string())?;
        f.sync_data().map_err(|e| e.to_string())
    }
}

/// One record as it lies in the journal: its length, then its bytes.
pub fn framed(record: &[u8]) -> Vec<u8> {
    let mut out = (record.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(record);
    out
}

/// The records in a journal as the host hands it over. A half-written record at the end —
/// the enclave died mid-append — is dropped: it was never answered for either.
pub fn split(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as usize;
        if len == 0 || at + 4 + len > bytes.len() {
            break;
        }
        out.push(bytes[at + 4..at + 4 + len].to_vec());
        at += 4 + len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_opens_only_in_its_own_place() {
        let s = Sealing::new(&[9u8; 32]);
        let sealed = s.seal(2, 7, b"a settled request").unwrap();
        assert_eq!(s.open(2, 7, &sealed).unwrap(), b"a settled request");
        // Moved, renumbered, or read under another key: all refused.
        assert!(s.open(2, 8, &sealed).is_err());
        assert!(s.open(3, 7, &sealed).is_err());
        assert!(Sealing::new(&[8u8; 32]).open(2, 7, &sealed).is_err());
        let mut bent = sealed.clone();
        *bent.last_mut().unwrap() ^= 1;
        assert!(s.open(2, 7, &bent).is_err());
    }

    #[test]
    fn the_journal_reads_back_record_by_record_and_ignores_a_torn_end() {
        let whole = [framed(b"one"), framed(b"two")].concat();
        assert_eq!(split(&whole).unwrap(), vec![b"one".to_vec(), b"two".to_vec()]);
        let torn = [whole.as_slice(), &[0, 0, 0, 9, b'h', b'a', b'l', b'f']].concat();
        assert_eq!(split(&torn).unwrap(), vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(split(b"").unwrap().is_empty());
    }

    #[test]
    fn a_new_snapshot_takes_the_journal_with_it() {
        let dir = std::env::temp_dir().join(format!("tokumai-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStore::new(&dir).unwrap();
        assert!(store.snapshot().unwrap().is_empty());
        store.append(b"first").unwrap();
        store.append(b"second").unwrap();
        assert_eq!(store.journal().unwrap().len(), 2);
        store.put_snapshot(b"the book so far").unwrap();
        assert_eq!(store.snapshot().unwrap(), b"the book so far");
        assert!(store.journal().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
