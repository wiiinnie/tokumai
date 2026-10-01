//! Blind notes: how a paid month reaches an account without the book recording whose
//! payment it was.
//!
//! A note is one paid month of one tier. The app blinds it, the enclave signs the blinded
//! form (RSA blind signatures as in RFC 9474: PSS encoding, deterministic, SHA-384, one
//! key per calendar month), the app unblinds, and later an account redeems the note for
//! that month's allowance. The enclave sees the note twice — blinded at minting, plain at
//! redemption — and cannot connect the two: that is the whole point, and it is maths, not
//! a promise.
//!
//! What this module holds is the part BOTH sides compute, so they cannot drift apart: the
//! note's bytes, the calendar epochs, the derivation of a note's nonce and blinding from
//! the account's seed (so a phone restored from the phrase makes the very same blinded
//! message and can be re-signed, see docs/blind-tokens.md), and the signature arithmetic.
//! The arithmetic is written out here on `rsa`'s integers rather than taken from a crate:
//! the one crate that implements the RFC either drags a release-candidate `rsa` into an
//! audited image or no longer builds against the macros in this tree, and the operations
//! are a page of RFC 8017 and RFC 9474 — blind, sign, unblind, verify — which an auditor
//! can read next to the RFC.
//!
//! Design, with the reasons, in docs/blind-tokens.md.

use hkdf::Hkdf;
use num_bigint_dig::ModInverse;
use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256, Sha384};

/// The note's format; a note of another version does not redeem.
pub const VERSION: u8 = 1;
/// How long after its month a note's allowance still runs: the grace in which the app
/// may redeem a renewal at a random moment while the old month's leftover is still usable.
pub const GRACE_MS: u64 = 7 * 86_400_000;
/// RSA modulus of the month keys.
pub const KEY_BITS: usize = 2048;
/// SHA-384 everywhere in the encoding (RFC 9474's RSABSSA-SHA384-PSS-Deterministic).
const H_LEN: usize = 48;
/// The month epochs count from here (January 2026 = 0).
const EPOCH_ZERO_YEAR: i32 = 2026;

// ---- calendar ------------------------------------------------------------------------

/// Calendar month of a moment, as months since January 2026 (UTC).
pub fn epoch_of_ms(ms: u64) -> u16 {
    let (y, m, _) = crate::subscription::civil_from_ms(ms);
    ((y - EPOCH_ZERO_YEAR) * 12 + (m as i32 - 1)).clamp(0, u16::MAX as i32) as u16
}

/// The first millisecond of a month epoch.
pub fn epoch_start_ms(epoch: u16) -> u64 {
    let y = EPOCH_ZERO_YEAR + (epoch / 12) as i32;
    let m = (epoch % 12) as u32 + 1;
    crate::subscription::ms_from_civil(y, m, 1)
}

/// The first millisecond after a month epoch.
pub fn epoch_end_ms(epoch: u16) -> u64 {
    epoch_start_ms(epoch + 1)
}

/// The month epochs a paid period may mint notes for: the month each monthly slice of it
/// ENDS in. A period ending on the 28th of October is October's note; a year is twelve.
/// Computed by the app (to ask) and by the enclave (to allow), from the same rail dates.
pub fn epochs_covered(start_ms: u64, until_ms: u64, yearly: bool) -> Vec<u16> {
    let n = if yearly { 12 } else { 1 };
    let mut out = Vec::with_capacity(n);
    for i in 0..n as u32 {
        let s = crate::subscription::add_months_ms(start_ms, i);
        let e = crate::subscription::add_months_ms(start_ms, i + 1).min(until_ms);
        if e <= s {
            break;
        }
        out.push(epoch_of_ms(e - 1));
    }
    out
}

/// The months open at `now`: last month, this month, next month. A renewal charged on
/// the 28th is for a period ending next month.
pub fn window(now_ms: u64) -> (u16, u16) {
    let cur = epoch_of_ms(now_ms);
    (cur.saturating_sub(1), cur + 1)
}

// ---- the note --------------------------------------------------------------------------

/// What a note says: its format, the tier it pays for, the month it is for, and a nonce
/// nothing else knows. 36 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub tier: u8,
    pub epoch: u16,
    pub nonce: [u8; 32],
}

impl Note {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(36);
        out.push(VERSION);
        out.push(self.tier);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&self.nonce);
        out
    }

    pub fn parse(bytes: &[u8]) -> Result<Note, String> {
        if bytes.len() != 36 {
            return Err("a note is 36 bytes".into());
        }
        if bytes[0] != VERSION {
            return Err("a note of another version".into());
        }
        let mut nonce = [0u8; 32];
        nonce.copy_from_slice(&bytes[4..36]);
        Ok(Note { tier: bytes[1], epoch: u16::from_be_bytes([bytes[2], bytes[3]]), nonce })
    }
}

/// 32 bytes from a secret, a label and an epoch — the one derivation both the note's nonce
/// and its blinding use, so a restored phone makes the same note again.
pub fn derive(secret: &[u8; 32], label: &str, epoch: u16) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(b"tokumai/notes/v1"), secret);
    let mut out = [0u8; 32];
    let mut info = label.as_bytes().to_vec();
    info.extend_from_slice(&epoch.to_be_bytes());
    hk.expand(&info, &mut out).expect("32 bytes is a valid HKDF length");
    out
}

/// The note an account makes for an epoch and a tier: its nonce comes from the seed.
pub fn note_for(seed: &[u8; 32], tier: u8, epoch: u16) -> Note {
    Note { tier, epoch, nonce: derive(seed, "nonce", epoch) }
}

// ---- keys --------------------------------------------------------------------------------

pub struct KeyPair {
    pub sk: RsaPrivateKey,
    pub pk: RsaPublicKey,
}

/// The enclave's key for a month, derived from its data key: nothing to store, and every
/// start of an image that can open the sealed secrets produces the same keys.
pub fn keypair_for(secret: &[u8; 32], epoch: u16) -> Result<KeyPair, String> {
    let mut rng = ChaCha20Rng::from_seed(derive(secret, "month-key", epoch));
    let sk = RsaPrivateKey::new(&mut rng, KEY_BITS).map_err(|e| format!("the month key could not be made: {e}"))?;
    let pk = sk.to_public_key();
    Ok(KeyPair { sk, pk })
}

pub fn public_to_spki(pk: &RsaPublicKey) -> Result<Vec<u8>, String> {
    pk.to_public_key_der().map(|d| d.as_bytes().to_vec()).map_err(|e| format!("the month key does not encode: {e}"))
}

pub fn public_from_spki(spki: &[u8]) -> Result<RsaPublicKey, String> {
    RsaPublicKey::from_public_key_der(spki).map_err(|e| format!("not a month key: {e}"))
}

// ---- the arithmetic (RFC 8017 § 9.1 with no salt, RFC 9474 § 4) ------------------------

fn mgf1(seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + H_LEN);
    let mut counter: u32 = 0;
    while out.len() < len {
        let mut h = Sha384::new();
        h.update(seed);
        h.update(counter.to_be_bytes());
        out.extend_from_slice(&h.finalize());
        counter += 1;
    }
    out.truncate(len);
    out
}

/// EMSA-PSS-ENCODE with an empty salt (deterministic), for `em_bits` = modulus bits − 1.
fn pss_encode(msg: &[u8], em_bits: usize) -> Result<Vec<u8>, String> {
    let em_len = em_bits.div_ceil(8);
    if em_len < H_LEN + 2 {
        return Err("the key is too small for the encoding".into());
    }
    let m_hash = Sha384::digest(msg);
    let mut m2 = vec![0u8; 8];
    m2.extend_from_slice(&m_hash);
    let h = Sha384::digest(&m2);
    let ps_len = em_len - H_LEN - 2;
    let mut db = vec![0u8; ps_len];
    db.push(0x01);
    let mask = mgf1(&h, em_len - H_LEN - 1);
    let mut masked: Vec<u8> = db.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect();
    let top = 8 * em_len - em_bits;
    masked[0] &= 0xFFu8 >> top;
    let mut em = masked;
    em.extend_from_slice(&h);
    em.push(0xbc);
    Ok(em)
}

/// EMSA-PSS-VERIFY with an empty salt.
fn pss_verify(msg: &[u8], em: &[u8], em_bits: usize) -> bool {
    let em_len = em_bits.div_ceil(8);
    if em.len() != em_len || em[em_len - 1] != 0xbc || em_len < H_LEN + 2 {
        return false;
    }
    let (masked, rest) = em.split_at(em_len - H_LEN - 1);
    let h = &rest[..H_LEN];
    let top = 8 * em_len - em_bits;
    if masked[0] & !(0xFFu8 >> top) != 0 {
        return false;
    }
    let mask = mgf1(h, em_len - H_LEN - 1);
    let mut db: Vec<u8> = masked.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect();
    db[0] &= 0xFFu8 >> top;
    let ps_len = em_len - H_LEN - 2;
    if db[..ps_len].iter().any(|&b| b != 0) || db[ps_len] != 0x01 {
        return false;
    }
    let m_hash = Sha384::digest(msg);
    let mut m2 = vec![0u8; 8];
    m2.extend_from_slice(&m_hash);
    Sha384::digest(&m2).as_slice() == h
}

fn to_fixed(x: &BigUint, len: usize) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0u8; len.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

/// The app's side, first half: blind a note with a blinding factor derived from the seed.
/// Returns the blinded message (what the enclave sees) and the secret the unblinding needs
/// — both reproducible from the seed, which is what a restored phone relies on.
pub fn blind(pk: &RsaPublicKey, note: &Note, seed: &[u8; 32]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let n = pk.n();
    let k = pk.size();
    let em = pss_encode(&note.to_bytes(), KEY_BITS - 1)?;
    let m = BigUint::from_bytes_be(&em);
    if &m >= n {
        return Err("the encoded note is not below the modulus".into());
    }
    let mut rng = ChaCha20Rng::from_seed(derive(seed, "blind", note.epoch));
    let mut buf = vec![0u8; k];
    let r = loop {
        rng.fill_bytes(&mut buf);
        let r = BigUint::from_bytes_be(&buf) % n;
        if r > BigUint::from(1u8) && (&r).mod_inverse(n).is_some() {
            break r;
        }
    };
    let blinded = (&m * r.modpow(pk.e(), n)) % n;
    Ok((to_fixed(&blinded, k), to_fixed(&r, k)))
}

/// The enclave's side: sign what it cannot read. Deterministic: the same blinded message
/// gives the same signature, so a restored phone's note can be signed again without
/// becoming a second note.
pub fn blind_sign(sk: &RsaPrivateKey, blinded: &[u8]) -> Result<Vec<u8>, String> {
    let k = sk.size();
    if blinded.len() != k {
        return Err("a blinded note has the modulus' length".into());
    }
    let z = BigUint::from_bytes_be(blinded);
    if &z >= sk.n() {
        return Err("the blinded note is not below the modulus".into());
    }
    Ok(to_fixed(&z.modpow(sk.d(), sk.n()), k))
}

/// The app's side, second half: the plain signature over the plain note, checked.
pub fn finalize(pk: &RsaPublicKey, blind_sig: &[u8], secret: &[u8], note: &Note) -> Result<Vec<u8>, String> {
    let n = pk.n();
    let k = pk.size();
    if blind_sig.len() != k || secret.len() != k {
        return Err("a blind signature and its secret have the modulus' length".into());
    }
    let r = BigUint::from_bytes_be(secret);
    let r_inv = (&r).mod_inverse(n).and_then(|i| i.to_biguint()).ok_or("the blinding factor has no inverse")?;
    let s = (BigUint::from_bytes_be(blind_sig) * r_inv) % n;
    let sig = to_fixed(&s, k);
    if !verify(pk, note, &sig) {
        return Err("the signature does not unblind to the note".into());
    }
    Ok(sig)
}

/// The enclave's side at redemption.
pub fn verify(pk: &RsaPublicKey, note: &Note, sig: &[u8]) -> bool {
    let k = pk.size();
    if sig.len() != k {
        return false;
    }
    let s = BigUint::from_bytes_be(sig);
    if &s >= pk.n() {
        return false;
    }
    let em = to_fixed(&s.modpow(pk.e(), pk.n()), (KEY_BITS - 1).div_ceil(8));
    pss_verify(&note.to_bytes(), &em, KEY_BITS - 1)
}

/// What the attestation binds: the month keys the enclave publishes, as one hash. Every app
/// computes it from the keys it was sent, so an enclave that handed one person a key of
/// its own would fail that person's attestation check.
pub fn keys_digest(keys: &[(u16, Vec<u8>)]) -> [u8; 32] {
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(keys.len() * 2 + 1);
    parts.push(b"tokumai/notes/keys/v1".to_vec());
    for (epoch, spki) in keys {
        parts.push(epoch.to_be_bytes().to_vec());
        parts.push((spki.len() as u32).to_be_bytes().to_vec());
        parts.push(spki.clone());
    }
    let refs: Vec<&[u8]> = parts.iter().map(|v| v.as_slice()).collect();
    crate::account::sha256(&refs)
}

/// Sixteen bytes of a blinded message's hash, for the book's "minted" record: enough to
/// tell "the same blinded message again" from "another one", and nothing a note can be
/// recognised by later.
pub fn blinded_fingerprint(blinded: &[u8]) -> String {
    hex::encode(&crate::account::sha256(&[b"tokumai/notes/blinded", blinded])[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epochs_are_calendar_months_from_january_2026() {
        let jan = crate::subscription::ms_from_civil(2026, 1, 15);
        let oct = crate::subscription::ms_from_civil(2026, 10, 1);
        let feb27 = crate::subscription::ms_from_civil(2027, 2, 28);
        assert_eq!(epoch_of_ms(jan), 0);
        assert_eq!(epoch_of_ms(oct), 9);
        assert_eq!(epoch_of_ms(feb27), 13);
        assert_eq!(epoch_start_ms(9), oct);
        assert_eq!(epoch_end_ms(9), crate::subscription::ms_from_civil(2026, 11, 1));
        assert_eq!(epoch_of_ms(epoch_end_ms(9) - 1), 9);
    }

    #[test]
    fn a_note_travels_as_36_bytes_and_comes_back_the_same() {
        let n = note_for(&[7u8; 32], 2, 9);
        let b = n.to_bytes();
        assert_eq!(b.len(), 36);
        assert_eq!(Note::parse(&b).unwrap(), n);
        assert!(Note::parse(&b[1..]).is_err());
        let mut other = b.clone();
        other[0] = 9;
        assert!(Note::parse(&other).is_err());
        assert_ne!(note_for(&[7u8; 32], 2, 10).nonce, n.nonce);
        assert_eq!(note_for(&[7u8; 32], 2, 9).nonce, n.nonce);
    }

    #[test]
    fn the_pss_encoding_verifies_itself_and_nothing_else() {
        let em = pss_encode(b"a note", KEY_BITS - 1).unwrap();
        assert_eq!(em.len(), 256);
        assert!(pss_verify(b"a note", &em, KEY_BITS - 1));
        assert!(!pss_verify(b"a different note", &em, KEY_BITS - 1));
        let mut bent = em.clone();
        bent[100] ^= 1;
        assert!(!pss_verify(b"a note", &bent, KEY_BITS - 1));
    }

    /// The whole round: the enclave makes a month key, the app blinds, the enclave signs
    /// what it cannot read, the app unblinds, the enclave verifies the plain note. And the
    /// enclave signing the SAME blinded message twice gives the same signature — the
    /// restored phone's case.
    #[test]
    fn a_note_is_signed_blind_and_verifies_plain() {
        let data_key = [3u8; 32];
        let kp = keypair_for(&data_key, 9).unwrap();
        let pk = public_from_spki(&public_to_spki(&kp.pk).unwrap()).unwrap();
        let seed = [5u8; 32];
        let note = note_for(&seed, 1, 9);
        let (blinded, secret) = blind(&pk, &note, &seed).unwrap();
        assert_eq!(blinded.len(), KEY_BITS / 8);
        // The blinded message is not the note's own encoding.
        assert_ne!(blinded, pss_encode(&note.to_bytes(), KEY_BITS - 1).unwrap());
        let sig_b = blind_sign(&kp.sk, &blinded).unwrap();
        let sig = finalize(&pk, &sig_b, &secret, &note).unwrap();
        assert!(verify(&pk, &note, &sig));
        // Not under another month's key, not for another note, not when bent.
        let other = keypair_for(&data_key, 10).unwrap();
        assert!(!verify(&other.pk, &note, &sig));
        assert!(!verify(&pk, &note_for(&seed, 1, 10), &sig));
        let mut bent = sig.clone();
        bent[7] ^= 1;
        assert!(!verify(&pk, &note, &bent));
        // The same seed blinds to the same bytes, and the same bytes sign to the same bytes.
        let (blinded2, secret2) = blind(&pk, &note, &seed).unwrap();
        assert_eq!((&blinded2, &secret2), (&blinded, &secret));
        assert_eq!(blind_sign(&kp.sk, &blinded).unwrap(), sig_b);
        // Another seed: another blinding of the same note, which the enclave cannot relate.
        let (blinded3, _) = blind(&pk, &note, &[6u8; 32]).unwrap();
        assert_ne!(blinded3, blinded);
    }

    /// The month key is a function of the data key: the same key on every start.
    #[test]
    fn the_month_key_is_the_same_on_every_start() {
        let a = public_to_spki(&keypair_for(&[9u8; 32], 9).unwrap().pk).unwrap();
        let b = public_to_spki(&keypair_for(&[9u8; 32], 9).unwrap().pk).unwrap();
        let c = public_to_spki(&keypair_for(&[8u8; 32], 9).unwrap().pk).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
