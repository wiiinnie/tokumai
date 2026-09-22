//! The layer between the app and the enclave, inside the mixnet's own encryption.
//!
//! Each request uses a fresh X25519 key on the app's side. The shared secret with the
//! enclave's attested key gives, through HKDF, one key for the request and one for its answer;
//! ChaCha20-Poly1305 seals both. A key is used once, so the nonce can be fixed.
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

fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(&[0u8; 12]), Payload { msg: plain, aad })
        .expect("sealing cannot fail")
}

fn open(key: &[u8; 32], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>, String> {
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
    /// Seal `plain` to the enclave's attested key. Returns the exchange and the ciphertext.
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

/// The enclave side: open a request, and seal its answer to the same exchange.
pub struct ServerExchange {
    resp_key: [u8; 32],
}

impl ServerExchange {
    pub fn open(kx: &StaticSecret, epk: &[u8; 32], ct: &[u8]) -> Result<(ServerExchange, Vec<u8>), String> {
        let server_pub = PublicKey::from(kx).to_bytes();
        let shared = kx.diffie_hellman(&PublicKey::from(*epk)).to_bytes();
        let (req_key, resp_key) = keys(&shared, epk, &server_pub);
        Ok((ServerExchange { resp_key }, open(&req_key, b"req", ct)?))
    }
    pub fn seal_response(&self, plain: &[u8]) -> Vec<u8> {
        seal(&self.resp_key, b"resp", plain)
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
        let (srv, plain) = ServerExchange::open(&server, &client.epk, &ct).unwrap();
        assert_eq!(plain, b"hello");
        let answer = srv.seal_response(b"world");
        assert_eq!(client.open_response(&answer).unwrap(), b"world");

        // Another key cannot open it, and a flipped bit is refused.
        let other = StaticSecret::random_from_rng(OsRng);
        assert!(ServerExchange::open(&other, &client.epk, &ct).is_err());
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(ServerExchange::open(&server, &client.epk, &bad).is_err());
        // A request cannot be passed off as an answer (different key and label).
        assert!(client.open_response(&ct).is_err());
    }
}
