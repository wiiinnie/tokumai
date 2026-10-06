//! Seal the enclave's secrets for KMS.
//!
//!     tokumai-seal <secrets.json> <out.sealed.json>
//!
//! KMS encrypts at most 4 KiB, and the secrets (Stripe's price table among them) can be
//! larger. So: a fresh key here, the secrets under it (ChaCha20-Poly1305), and only that
//! key goes to KMS — which will hand it back to an attested enclave and to nothing else.
//! The key is printed as hex for `deploy/aws/kms.sh secrets` and forgotten here.
//!
//! Only the operator's secrets go this way: provider keys, Stripe. The enclave's data key
//! and its doors' identities are refused — the enclave makes those itself.

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
    let secrets = serde_json::from_slice::<serde_json::Value>(&plain).expect("the secrets are JSON");
    // The enclave's own are not sealed from here: the data key is born inside it (KMS
    // GenerateDataKey to an attested enclave) and an operator who held it could read the
    // book — so a file that carries one is refused. The doors' identities are the
    // enclave's own too, kept sealed under that key; an enclave that has none yet reads
    // the operator-sealed set ONCE (the addresses the apps pin survive that way), so they
    // are let through with a warning, to be taken out after that first start.
    if secrets.get("dataKey").is_some() {
        eprintln!("tokumai-seal: dataKey must not be in the sealed secrets — the enclave makes its own (since 2026-10-05). Remove it, then shred this file.");
        std::process::exit(1);
    }
    for doors in ["nymIdentity", "nymIdentities"] {
        if secrets.get(doors).is_some() {
            eprintln!("tokumai-seal: note: {doors} is in the secrets. An enclave with no sealed doors of its own on the host reads it once and keeps the doors itself from then on; seal again without it after that first start.");
        }
    }
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
