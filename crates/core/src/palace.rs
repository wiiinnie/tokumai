//! The Memory Palace: a memory that follows the person from device to device, which the
//! operator can neither read nor tie to an account or a payment.
//!
//! Everything secret comes out of the recovery phrase, under labels of its own, so the
//! palace and the account share nothing a third party could follow from one to the other:
//!
//! - `tokumai/palace/v1/enc`  — the AES-256-GCM key every blob is sealed with;
//! - `tokumai/palace/v1/sign` — the Ed25519 key every palace request is signed with;
//! - `tokumai/palace/v1/ns`   — the salt of the namespace's name.
//!
//! The namespace is `sha256(signing public key ‖ salt)`: the enclave can check that a
//! request comes from whoever wrote first under that name (the key is registered on the
//! first write), and nobody without the phrase can compute the name from anything the
//! account ever showed. A palace request is never signed by the account and never travels
//! with anything of the account's (the app sends it under a throwaway envelope key).
//!
//! A blob's name is `HMAC(enc key, file id)`, so the enclave sees neither file ids nor
//! titles. A blob is `nonce(12) ‖ AES-256-GCM(plaintext)` with the additional data
//! `namespace ‖ blob id ‖ version`, so a blob cannot be moved to another name, another
//! namespace, or back to an older version without the seal failing.
//!
//! The plaintext shapes (manifest, files, placeholders) and how two devices' copies are
//! merged live here too, so the app's sync and the tests agree on them.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap};

use crate::account::sha256;

pub const LABEL_ENC: &[u8] = b"tokumai/palace/v1/enc";
pub const LABEL_SIGN: &[u8] = b"tokumai/palace/v1/sign";
pub const LABEL_NS: &[u8] = b"tokumai/palace/v1/ns";

pub const MANIFEST_SCHEMA: &str = "tokumai/palace-manifest@1";
pub const FILE_SCHEMA: &str = "tokumai/palace-file@1";
pub const PLACEHOLDERS_SCHEMA: &str = "tokumai/palace-placeholders@1";
/// The file ids of the two blobs that are not topic files.
pub const MANIFEST_ID: &str = "manifest";
pub const PLACEHOLDERS_ID: &str = "placeholders";

/// The palace's keys, from the phrase. Holds secrets: never logged, never serialised.
pub struct PalaceKeys {
    enc: [u8; 32],
    sign: SigningKey,
    namespace: String,
}

fn hkdf32(seed: &[u8], label: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, seed);
    let mut out = [0u8; 32];
    hk.expand(label, &mut out).expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// The namespace's name: `sha256(signing public key ‖ salt)`, hex.
pub fn namespace_of(sign_pub: &[u8; 32], ns_salt: &[u8; 32]) -> String {
    hex::encode(sha256(&[sign_pub, ns_salt]))
}

impl PalaceKeys {
    /// From the 24 words, exactly as the account reads them.
    pub fn from_mnemonic(phrase: &str) -> Result<PalaceKeys, String> {
        let norm = phrase.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
        let m = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &norm).map_err(|_| "that is not a valid recovery phrase".to_string())?;
        Ok(Self::from_seed(&m.to_seed("")))
    }

    /// From the BIP39 seed (64 bytes).
    pub fn from_seed(seed: &[u8]) -> PalaceKeys {
        let enc = hkdf32(seed, LABEL_ENC);
        let sign = SigningKey::from_bytes(&hkdf32(seed, LABEL_SIGN));
        let ns_salt = hkdf32(seed, LABEL_NS);
        let namespace = namespace_of(&sign.verifying_key().to_bytes(), &ns_salt);
        PalaceKeys { enc, sign, namespace }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn sign_pub_hex(&self) -> String {
        hex::encode(self.sign.verifying_key().to_bytes())
    }

    /// The name a file's blob is stored under: `HMAC-SHA256(enc key, file id)`, hex.
    pub fn blob_id(&self, file_id: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.enc).expect("HMAC takes any key length");
        mac.update(b"tokumai/palace/v1/blob:");
        mac.update(file_id.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn aad(&self, blob_id: &str, version: u64) -> Vec<u8> {
        let mut aad = Vec::with_capacity(64 + blob_id.len() + 8);
        aad.extend_from_slice(self.namespace.as_bytes());
        aad.extend_from_slice(blob_id.as_bytes());
        aad.extend_from_slice(&version.to_be_bytes());
        aad
    }

    /// Seal a blob for a name and the version it will have on the server.
    pub fn seal(&self, blob_id: &str, version: u64, plaintext: &[u8]) -> Vec<u8> {
        seal_with(&self.enc, &self.aad(blob_id, version), plaintext)
    }

    pub fn open(&self, blob_id: &str, version: u64, sealed: &[u8]) -> Result<Vec<u8>, String> {
        open_with(&self.enc, &self.aad(blob_id, version), sealed).map_err(|_| "a palace blob does not open — not this phrase's, or tampered with".to_string())
    }

    /// The device's own cache of the palace, under the same key with a label of its own,
    /// so a cache file can never pass for a blob or the other way round.
    pub fn seal_local(&self, plaintext: &[u8]) -> Vec<u8> {
        let aad = [b"tokumai/palace/v1/local:".as_slice(), self.namespace.as_bytes()].concat();
        seal_with(&self.enc, &aad, plaintext)
    }

    pub fn open_local(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        let aad = [b"tokumai/palace/v1/local:".as_slice(), self.namespace.as_bytes()].concat();
        open_with(&self.enc, &aad, sealed).map_err(|_| "the palace cache does not open with this phrase".to_string())
    }

    /// Sign a palace request (see [`request_message`]), base64.
    pub fn sign_request(&self, r: &SignedFields<'_>) -> String {
        B64.encode(self.sign.sign(request_message(&self.namespace, r).as_bytes()).to_bytes())
    }
}

fn seal_with(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("a 32-byte key");
    let nonce: [u8; 12] = rand_bytes();
    let ct = cipher.encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad }).expect("AES-GCM sealing does not fail");
    [nonce.as_slice(), &ct].concat()
}

fn open_with(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, ()> {
    if sealed.len() < 12 + 16 {
        return Err(());
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| ())?;
    cipher.decrypt(Nonce::from_slice(&sealed[..12]), Payload { msg: &sealed[12..], aad }).map_err(|_| ())
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    use aes_gcm::aead::{rand_core::RngCore, OsRng};
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

/// A fresh random id, hex (`f_…` for files, `l_…` for lines).
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", hex::encode(rand_bytes::<8>()))
}

/// What a palace request is signed over, besides the namespace.
pub struct SignedFields<'a> {
    pub op: &'a str,
    /// The blob the request is about; for `list` empty, for `get` the ids joined by ",".
    pub blob_id: &'a str,
    pub version: u64,
    /// sha256 of the ciphertext, hex (of nothing for anything but `put`).
    pub ct_sha256: &'a str,
    pub ts: u64,
    pub nonce: &'a str,
}

/// `tokumai/palace/v1|ns|op|blob|version|sha256(ct)|ts|nonce`.
pub fn request_message(namespace: &str, r: &SignedFields<'_>) -> String {
    format!("tokumai/palace/v1|{namespace}|{}|{}|{}|{}|{}|{}", r.op, r.blob_id, r.version, r.ct_sha256, r.ts, r.nonce)
}

pub fn ct_hash(ct: &[u8]) -> String {
    hex::encode(sha256(&[ct]))
}

/// Did the holder of `pub_hex` sign this request for this namespace?
pub fn verify_request(pub_hex: &str, namespace: &str, r: &SignedFields<'_>, sig_b64: &str) -> bool {
    let Some(key) = hex::decode(pub_hex).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()).and_then(|b| VerifyingKey::from_bytes(&b).ok()) else {
        return false;
    };
    let Some(sig) = B64.decode(sig_b64).ok().and_then(|b| Signature::from_slice(&b).ok()) else { return false };
    key.verify(request_message(namespace, r).as_bytes(), &sig).is_ok()
}

// ---- the plaintext ------------------------------------------------------------------

fn is_false(b: &bool) -> bool {
    !*b
}

/// One topic file as the index lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// How often the file's content changed (counted locally, merged by maximum).
    #[serde(default)]
    pub version: u64,
    #[serde(default, rename = "neverSend")]
    pub never_send: bool,
    /// When title, description or the flag last changed (ms).
    #[serde(default)]
    pub updated: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest { schema: MANIFEST_SCHEMA.into(), files: Vec::new() }
    }
}

/// One fact. A deleted line is a tombstone: its id, the hash of its words, no words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, rename = "neverSend")]
    pub never_send: bool,
    #[serde(default)]
    pub updated: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    /// For a tombstone: the hash of the normalised words it had, so the same fact arriving
    /// from another device under another id is recognised as deleted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub h: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemFile {
    pub schema: String,
    pub id: String,
    #[serde(default)]
    pub lines: Vec<Line>,
}

impl MemFile {
    pub fn new(id: &str) -> MemFile {
        MemFile { schema: FILE_SCHEMA.into(), id: id.into(), lines: Vec::new() }
    }
}

/// What the anonymiser put in place of a detail (`[EMPLOYER_1]` → "Telekom"). Kept in the
/// palace, sealed like everything else; it never goes to a model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placeholder {
    pub ph: String,
    pub value: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub updated: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placeholders {
    pub schema: String,
    #[serde(default)]
    pub map: Vec<Placeholder>,
}

impl Default for Placeholders {
    fn default() -> Self {
        Placeholders { schema: PLACEHOLDERS_SCHEMA.into(), map: Vec::new() }
    }
}

/// The words of a fact as compared for duplicates: lower case, single spaces, no closing
/// punctuation.
pub fn normalize(text: &str) -> String {
    let t = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    t.trim_end_matches(['.', '!', ';', ',']).trim().to_string()
}

pub fn text_hash(text: &str) -> String {
    hex::encode(&sha256(&[normalize(text).as_bytes()])[..16])
}

/// Turn a line into its tombstone, at `now`.
pub fn tombstone(line: &mut Line, now: u64) {
    if !line.deleted {
        line.h = text_hash(&line.text);
    }
    line.deleted = true;
    line.text.clear();
    line.updated = now;
}

/// Two copies of the index → one. Files are united by id; for the same file the later
/// change of title/description/flag wins; a deleted file stays deleted.
pub fn merge_manifest(a: &Manifest, b: &Manifest) -> Manifest {
    let mut by_id: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for f in a.files.iter().chain(b.files.iter()) {
        match by_id.get_mut(&f.id) {
            None => {
                order.push(f.id.clone());
                by_id.insert(f.id.clone(), f.clone());
            }
            Some(have) => {
                let deleted = have.deleted || f.deleted;
                let version = have.version.max(f.version);
                if f.updated > have.updated {
                    *have = f.clone();
                }
                have.deleted = deleted;
                have.version = version;
            }
        }
    }
    Manifest { schema: MANIFEST_SCHEMA.into(), files: order.into_iter().filter_map(|id| by_id.remove(&id)).collect() }
}

/// Two copies of a file → one: the union of the lines; for the same id the later edit
/// wins and a deletion always wins; a live line whose words a tombstone of the same age or
/// newer carries is gone too; and of several live lines with the same words, the one with
/// the smallest id stays (the same choice on every device, so copies converge).
pub fn merge_file(a: &MemFile, b: &MemFile) -> MemFile {
    let mut by_id: HashMap<String, Line> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for l in a.lines.iter().chain(b.lines.iter()) {
        match by_id.get_mut(&l.id) {
            None => {
                order.push(l.id.clone());
                by_id.insert(l.id.clone(), l.clone());
            }
            Some(have) => {
                if have.deleted || l.deleted {
                    let h = if have.deleted && !have.h.is_empty() { have.h.clone() } else if l.deleted && !l.h.is_empty() { l.h.clone() } else { text_hash(if have.deleted { &l.text } else { &have.text }) };
                    let updated = have.updated.max(l.updated);
                    *have = Line { id: have.id.clone(), text: String::new(), never_send: false, updated, deleted: true, h };
                } else if l.updated > have.updated {
                    *have = l.clone();
                }
            }
        }
    }
    // The newest deletion of each wording.
    let mut gone: HashMap<String, u64> = HashMap::new();
    for l in by_id.values().filter(|l| l.deleted && !l.h.is_empty()) {
        let e = gone.entry(l.h.clone()).or_insert(0);
        *e = (*e).max(l.updated);
    }
    let mut kept_text: HashMap<String, String> = HashMap::new();
    let mut live: Vec<&Line> = by_id.values().filter(|l| !l.deleted).collect();
    live.sort_by(|x, y| x.id.cmp(&y.id));
    let mut drop: Vec<String> = Vec::new();
    for l in live {
        let h = text_hash(&l.text);
        if gone.get(&h).is_some_and(|&t| t >= l.updated) {
            drop.push(l.id.clone());
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(e) = kept_text.entry(h) {
            e.insert(l.id.clone());
        } else {
            drop.push(l.id.clone());
        }
    }
    let lines = order
        .into_iter()
        .filter(|id| !drop.contains(id))
        .filter_map(|id| by_id.remove(&id))
        .collect();
    MemFile { schema: FILE_SCHEMA.into(), id: if a.id.is_empty() { b.id.clone() } else { a.id.clone() }, lines }
}

/// Two copies of the placeholder map → one. A placeholder that two devices gave to two
/// different details keeps the older detail (the newer one was given second).
pub fn merge_placeholders(a: &Placeholders, b: &Placeholders) -> Placeholders {
    let mut by_ph: BTreeMap<String, Placeholder> = BTreeMap::new();
    for p in a.map.iter().chain(b.map.iter()) {
        match by_ph.get(&p.ph) {
            Some(have) if (have.updated, &have.value) <= (p.updated, &p.value) => {}
            _ => {
                by_ph.insert(p.ph.clone(), p.clone());
            }
        }
    }
    Placeholders { schema: PLACEHOLDERS_SCHEMA.into(), map: by_ph.into_values().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";

    #[test]
    fn the_palace_keys_are_the_same_on_every_device_and_namespaced_by_key_and_salt() {
        let a = PalaceKeys::from_mnemonic(M).unwrap();
        let b = PalaceKeys::from_mnemonic(&format!("  {}  ", M.to_uppercase())).unwrap();
        assert_eq!(a.namespace(), b.namespace());
        assert_eq!(a.sign_pub_hex(), b.sign_pub_hex());
        assert_eq!(a.blob_id("f_1"), b.blob_id("f_1"));
        let pk: [u8; 32] = hex::decode(a.sign_pub_hex()).unwrap().try_into().unwrap();
        let seed = bip39::Mnemonic::parse(M).unwrap().to_seed("");
        assert_eq!(a.namespace(), namespace_of(&pk, &hkdf32(&seed, LABEL_NS)));
        assert_eq!(a.namespace().len(), 64);
    }

    #[test]
    fn the_palace_and_the_account_share_nothing_that_can_be_followed_without_the_seed() {
        let account = crate::account::from_mnemonic(M).unwrap();
        let palace = PalaceKeys::from_mnemonic(M).unwrap();
        // The account's public face: its id and its key. Neither is, or appears in, any
        // value the palace shows.
        let acct_pub = account.public_key_pem.clone();
        assert_ne!(palace.namespace(), account.account_id);
        assert!(!acct_pub.contains(&palace.sign_pub_hex()));
        assert_ne!(palace.sign_pub_hex(), hex::encode(account.note_seed()));
        // Each key comes from its own label: the same seed under a different label gives
        // unrelated bytes, so knowing one derived key says nothing about another.
        let seed = bip39::Mnemonic::parse(M).unwrap().to_seed("");
        let enc = hkdf32(&seed, LABEL_ENC);
        let sign = hkdf32(&seed, LABEL_SIGN);
        let ns = hkdf32(&seed, LABEL_NS);
        let account_secret = sha256(&[b"scrai/account/v1", &seed]);
        let all = [enc, sign, ns, account_secret, account.note_seed()];
        for i in 0..all.len() {
            for j in 0..all.len() {
                if i != j {
                    assert_ne!(all[i], all[j]);
                    // Not even a byte-wise relation that would hint at a shared stream.
                    let same = all[i].iter().zip(all[j].iter()).filter(|(x, y)| x == y).count();
                    assert!(same < 6, "{i} and {j} agree in {same} bytes");
                }
            }
        }
        // The account's id is not computable from the palace's public values, and the
        // namespace is not computable from the account's: different phrases, different
        // namespaces, and no function of the account id gives it.
        let other = PalaceKeys::from_mnemonic(&crate::account::create_account().mnemonic).unwrap();
        assert_ne!(other.namespace(), palace.namespace());
        assert_ne!(hex::encode(sha256(&[account.account_id.as_bytes()])), palace.namespace());
    }

    #[test]
    fn a_blob_opens_only_under_its_name_namespace_and_version() {
        let k = PalaceKeys::from_mnemonic(M).unwrap();
        let blob = k.blob_id("f_9c2");
        let ct = k.seal(&blob, 7, b"{\"lines\":[]}");
        assert_eq!(k.open(&blob, 7, &ct).unwrap(), b"{\"lines\":[]}");
        assert!(k.open(&blob, 6, &ct).is_err(), "an older version is refused");
        assert!(k.open(&k.blob_id("f_other"), 7, &ct).is_err(), "another name is refused");
        let stranger = PalaceKeys::from_mnemonic(&crate::account::create_account().mnemonic).unwrap();
        assert!(stranger.open(&blob, 7, &ct).is_err());
        assert!(k.open_local(&ct).is_err(), "a blob is not a cache file");
        let local = k.seal_local(b"cache");
        assert_eq!(k.open_local(&local).unwrap(), b"cache");
        // The blob's name does not show the file id.
        assert!(!blob.contains("9c2"));
        assert_ne!(k.seal(&blob, 7, b"x"), k.seal(&blob, 7, b"x"), "a fresh nonce every time");
    }

    #[test]
    fn a_request_signature_covers_every_field() {
        let k = PalaceKeys::from_mnemonic(M).unwrap();
        let ct = ct_hash(b"ciphertext");
        let f = SignedFields { op: "put", blob_id: "b1", version: 3, ct_sha256: &ct, ts: 1000, nonce: "n1" };
        let sig = k.sign_request(&f);
        assert!(verify_request(&k.sign_pub_hex(), k.namespace(), &f, &sig));
        assert!(!verify_request(&k.sign_pub_hex(), "another-namespace", &f, &sig));
        for bad in [
            SignedFields { op: "delete", ..f },
            SignedFields { blob_id: "b2", ..f },
            SignedFields { version: 4, ..f },
            SignedFields { ts: 1001, ..f },
            SignedFields { nonce: "n2", ..f },
        ] {
            assert!(!verify_request(&k.sign_pub_hex(), k.namespace(), &bad, &sig));
        }
        let other = ct_hash(b"other");
        assert!(!verify_request(&k.sign_pub_hex(), k.namespace(), &SignedFields { ct_sha256: &other, ..f }, &sig));
    }

    fn line(id: &str, text: &str, updated: u64) -> Line {
        Line { id: id.into(), text: text.into(), never_send: false, updated, deleted: false, h: String::new() }
    }

    #[test]
    fn concurrent_edits_merge_without_losing_lines() {
        let base = MemFile { lines: vec![line("l_1", "Works as head of a VFX studio", 1)], ..MemFile::new("f_1") };
        let mut phone = base.clone();
        phone.lines.push(line("l_2", "Prefers short answers", 2));
        let mut laptop = base.clone();
        laptop.lines.push(line("l_3", "Lives in Berlin", 3));
        laptop.lines.push(line("l_4", "prefers short answers.", 4)); // the same fact, said twice
        let m1 = merge_file(&phone, &laptop);
        let m2 = merge_file(&laptop, &phone);
        let texts = |m: &MemFile| {
            let mut t: Vec<String> = m.lines.iter().filter(|l| !l.deleted).map(|l| l.text.clone()).collect();
            t.sort();
            t
        };
        assert_eq!(texts(&m1), texts(&m2), "the same result whichever device merges");
        assert_eq!(texts(&m1).len(), 3, "{:?}", texts(&m1));
        assert!(m1.lines.iter().any(|l| l.id == "l_2"), "the smaller id of a duplicate stays");
    }

    #[test]
    fn a_deletion_wins_and_a_restored_line_comes_back_under_a_new_id() {
        let base = MemFile { lines: vec![line("l_1", "Lives in Berlin", 1)], ..MemFile::new("f_1") };
        let mut phone = base.clone();
        tombstone(&mut phone.lines[0], 5);
        let mut laptop = base.clone();
        laptop.lines[0].text = "Lives in Berlin-Mitte".into();
        laptop.lines[0].updated = 9; // edited later, still deleted
        let m = merge_file(&phone, &laptop);
        assert!(m.lines[0].deleted && m.lines[0].text.is_empty());
        // The same words arriving under another id from a device that had not seen the
        // deletion are gone too…
        let stale = MemFile { lines: vec![line("l_9", "lives in berlin", 2)], ..MemFile::new("f_1") };
        let m = merge_file(&phone, &stale);
        assert!(m.lines.iter().all(|l| l.deleted));
        // …but the fact said again after the deletion stays.
        let again = MemFile { lines: vec![line("l_10", "Lives in Berlin", 8)], ..MemFile::new("f_1") };
        let m = merge_file(&phone, &again);
        assert!(m.lines.iter().any(|l| l.id == "l_10" && !l.deleted));
    }

    #[test]
    fn the_index_merges_by_the_later_change_and_keeps_deletions() {
        let entry = |id: &str, title: &str, updated: u64, deleted: bool| FileEntry {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            version: updated,
            never_send: false,
            updated,
            deleted,
        };
        let a = Manifest { files: vec![entry("f_1", "Work", 1, false), entry("f_2", "Health", 1, false)], ..Default::default() };
        let b = Manifest { files: vec![entry("f_1", "Job", 5, false), entry("f_2", "Health", 0, true), entry("f_3", "Family", 2, false)], ..Default::default() };
        let m = merge_manifest(&a, &b);
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.files[0].title, "Job");
        assert!(m.files[1].deleted);
        assert_eq!(merge_manifest(&b, &a).files.iter().map(|f| (&f.id, &f.title, f.deleted)).collect::<std::collections::BTreeSet<_>>(), m.files.iter().map(|f| (&f.id, &f.title, f.deleted)).collect());
    }

    #[test]
    fn placeholders_keep_the_first_detail_given() {
        let p = |ph: &str, v: &str, t: u64| Placeholder { ph: ph.into(), value: v.into(), kind: "employer".into(), updated: t };
        let a = Placeholders { map: vec![p("[EMPLOYER_1]", "Telekom", 1)], ..Default::default() };
        let b = Placeholders { map: vec![p("[EMPLOYER_1]", "Siemens", 2), p("[PERSON_1]", "Anna", 2)], ..Default::default() };
        let m = merge_placeholders(&a, &b);
        assert_eq!(m, merge_placeholders(&b, &a));
        assert_eq!(m.map.iter().find(|x| x.ph == "[EMPLOYER_1]").unwrap().value, "Telekom");
        assert_eq!(m.map.len(), 2);
    }
}
