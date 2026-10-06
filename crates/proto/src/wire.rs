//! The layer between the app and the enclave, inside the mixnet's own encryption.
//!
//! Each request uses a fresh X25519 key on the app's side. The shared secret with the
//! enclave's attested key gives, through HKDF, one key for the request and one for its answer;
//! ChaCha20-Poly1305 seals both, each under a fresh random nonce carried in front of the
//! ciphertext (v2, 2026-10-06). The first form (v1) fixed the nonce at zero on the premise
//! that a key is used once — which the enclave did not keep: a request presented again
//! after its answer had left the cache got a SECOND message sealed under the same key and
//! nonce, and two ChaCha20 streams under one key and nonce give away their XOR (audit H1).
//! The enclave now seals once per request by construction (`ServerExchange::seal_response`
//! consumes the exchange) and the nonce is random besides, so neither premise has to hold.
//!
//! v1 is still opened, for apps in the field that send it; their answers are sealed in v1
//! too, since that is what they can open. The outer message says which (`"v": 2`).
//!
//! Why a layer of our own when the mixnet already encrypts: whatever terminates the mixnet —
//! today the enclave itself, later perhaps a front outside it — must not be able to read.
//! Only the attested code holds the key this is sealed to.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

const LABEL: &[u8] = b"tokumai/wire/v1";

fn keys(shared: &[u8; 32], epk: &[u8; 32], server_pub: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(epk);
    info.extend_from_slice(server_pub);
    let hk = Hkdf::<Sha256>::new(Some(LABEL), shared);
    let mut okm = [0u8; 64];
    hk.expand(&info, &mut okm).expect("64 bytes is a valid HKDF length");
    let (a, b) = okm.split_at(32);
    (a.try_into().unwrap(), b.try_into().unwrap())
}

/// The form of the message on the wire. Said in the outer message (`"v": 2`); absent means v1.
pub const VERSION: u64 = 2;

/// v2: a fresh nonce in front of the ciphertext.
fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce: [u8; 12] = rand::random();
    let ct = ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad })
        .expect("sealing cannot fail");
    [nonce.as_slice(), &ct].concat()
}

fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String> {
    if sealed.len() < 12 {
        return Err("the request could not be opened".into());
    }
    let (nonce, ct) = sealed.split_at(12);
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| "the request could not be opened".to_string())
}

/// v1: the nonce fixed at zero. Opened and answered for apps that still send it; never
/// sealed twice under one key (see the module note).
fn seal_v1(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: plain, aad })
        .expect("sealing cannot fail")
}

fn open_v1(key: &[u8; 32], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>, String> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: ct, aad })
        .map_err(|_| "the request could not be opened".to_string())
}

/// The app side of one exchange: seal the request, keep what is needed to open the answer.
pub struct ClientExchange {
    pub epk: [u8; 32],
    resp_key: [u8; 32],
}

impl ClientExchange {
    /// Seal `plain` to the enclave's attested key (v2). Returns the exchange and the ciphertext.
    pub fn seal(server_kx_pub: &[u8; 32], plain: &[u8]) -> (ClientExchange, Vec<u8>) {
        let eph = EphemeralSecret::random_from_rng(OsRng);
        let epk = PublicKey::from(&eph).to_bytes();
        let shared = eph.diffie_hellman(&PublicKey::from(*server_kx_pub)).to_bytes();
        let (req_key, resp_key) = keys(&shared, &epk, server_kx_pub);
        (ClientExchange { epk, resp_key }, seal(&req_key, b"req", plain))
    }
    pub fn open_response(&self, ct: &[u8]) -> Result<Vec<u8>, String> {
        open(&self.resp_key, b"resp", ct)
    }
}

/// The enclave side: open a request, and seal its answer to the same exchange — once.
/// `seal_response` takes the exchange by value, so a second answer under the same key is
/// a compile error rather than a leak.
pub struct ServerExchange {
    resp_key: [u8; 32],
    legacy: bool,
}

impl ServerExchange {
    /// Open a request in the form the outer message names: `version` 2 (or more) for the
    /// current one, anything else for v1.
    pub fn open(kx: &StaticSecret, epk: &[u8; 32], ct: &[u8], version: u64) -> Result<(ServerExchange, Vec<u8>), String> {
        let server_pub = PublicKey::from(kx).to_bytes();
        let shared = kx.diffie_hellman(&PublicKey::from(*epk)).to_bytes();
        let (req_key, resp_key) = keys(&shared, epk, &server_pub);
        let legacy = version < VERSION;
        let plain = if legacy { open_v1(&req_key, b"req", ct)? } else { open(&req_key, b"req", ct)? };
        Ok((ServerExchange { resp_key, legacy }, plain))
    }
    pub fn seal_response(self, plain: &[u8]) -> Vec<u8> {
        if self.legacy {
            seal_v1(&self.resp_key, b"resp", plain)
        } else {
            seal(&self.resp_key, b"resp", plain)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_request_opens_only_with_the_enclave_key_and_its_answer_only_for_the_sender() {
        let server = StaticSecret::random_from_rng(OsRng);
        let server_pub = PublicKey::from(&server).to_bytes();
        let (client, ct) = ClientExchange::seal(&server_pub, b"hello");
        let (srv, plain) = ServerExchange::open(&server, &client.epk, &ct, VERSION).unwrap();
        assert_eq!(plain, b"hello");
        let answer = srv.seal_response(b"world");
        assert_eq!(client.open_response(&answer).unwrap(), b"world");

        // Another key cannot open it, and a flipped bit is refused — in the nonce or after it.
        let other = StaticSecret::random_from_rng(OsRng);
        assert!(ServerExchange::open(&other, &client.epk, &ct, VERSION).is_err());
        for at in [0, 20] {
            let mut bad = ct.clone();
            bad[at] ^= 1;
            assert!(ServerExchange::open(&server, &client.epk, &bad, VERSION).is_err());
        }
        assert!(ServerExchange::open(&server, &client.epk, &ct[..8], VERSION).is_err());
        // A request cannot be passed off as an answer (different key and label).
        assert!(client.open_response(&ct).is_err());
        // Nor can a v2 request be opened as v1, or the other way round.
        assert!(ServerExchange::open(&server, &client.epk, &ct, 1).is_err());
    }

    /// Two seals of the same words under the same key are different bytes: the nonce is
    /// fresh each time, so even an enclave that answered twice would give nothing away.
    #[test]
    fn the_nonce_is_fresh_for_every_seal() {
        let key = [3u8; 32];
        let a = seal(&key, b"resp", b"the same answer");
        let b = seal(&key, b"resp", b"the same answer");
        assert_ne!(a, b);
        assert_ne!(a[..12], b[..12]);
        assert_eq!(open(&key, b"resp", &a).unwrap(), b"the same answer");
        assert_eq!(open(&key, b"resp", &b).unwrap(), b"the same answer");
    }

    /// An app of the first form is still answered, in its own form.
    #[test]
    fn a_v1_request_is_opened_and_answered_in_v1() {
        let server = StaticSecret::random_from_rng(OsRng);
        let server_pub = PublicKey::from(&server).to_bytes();
        let eph = EphemeralSecret::random_from_rng(OsRng);
        let epk = PublicKey::from(&eph).to_bytes();
        let shared = eph.diffie_hellman(&PublicKey::from(server_pub)).to_bytes();
        let (req_key, resp_key) = keys(&shared, &epk, &server_pub);
        let ct = seal_v1(&req_key, b"req", b"old app");
        let (srv, plain) = ServerExchange::open(&server, &epk, &ct, 1).unwrap();
        assert_eq!(plain, b"old app");
        let answer = srv.seal_response(b"still here");
        assert_eq!(open_v1(&resp_key, b"resp", &answer).unwrap(), b"still here");
        assert!(open(&resp_key, b"resp", &answer).is_err(), "not a v2 answer");
    }
}
