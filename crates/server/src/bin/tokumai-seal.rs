//! Seal the enclave's secrets for KMS.
//!
//!     tokumai-seal <secrets.json> <out.sealed.json>
//!
//! KMS encrypts at most 4 KiB, and the secrets (with the Nym identity in them) are larger.
//! So: a fresh key here, the secrets under it (ChaCha20-Poly1305), and only that key goes
//! to KMS — which will hand it back to an attested enclave and to nothing else. The key
//! is printed as hex for `deploy/aws/kms.sh seal` and forgotten here.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: tokumai-seal <secrets.json> <out.sealed.json>");
        std::process::exit(2);
    };
    let plain = std::fs::read(&input).expect("read the secrets");
    serde_json::from_slice::<serde_json::Value>(&plain).expect("the secrets are JSON");
    let key: [u8; 32] = rand::random();
    let nonce: [u8; 12] = rand::random();
    let ct = ChaCha20Poly1305::new_from_slice(&key)
        .expect("key")
        .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
        .expect("seal the secrets");
    let envelope = serde_json::json!({ "nonce": B64.encode(nonce), "ct": B64.encode(ct) });
    std::fs::write(&output, serde_json::to_vec_pretty(&envelope).expect("json")).expect("write the envelope");
    // The key, for KMS to seal. The only copy — this program keeps none.
    println!("{}", hex::encode(key));
}
