//! The enclave, simulated on this machine: the real service with a stand-in attester, a local
//! key file and a mock model. Listens on 127.0.0.1:7707, one JSON message per line.
//!
//!     cargo run -p tokumai-enclave --bin tokumai-enclave-dev
//!
//! State lives in ./dev-data. `sim-root.key` stands in for the platform's attestation key; a
//! development client pins its public half, as the app will pin AWS's or Google's root.

use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokumai_attest::sim;
use tokumai_enclave::provider::MockProvider;
use tokumai_enclave::seal::FileKeyProvider;
use tokumai_enclave::service::{Db, Enclave, Platform};

fn sim_root(dir: &std::path::Path) -> [u8; 32] {
    let path = dir.join("sim-root.key");
    if let Ok(b) = std::fs::read(&path) {
        if let Ok(k) = b.try_into() {
            return k;
        }
    }
    let k: [u8; 32] = rand::random();
    std::fs::write(&path, k).expect("write sim-root.key");
    k
}

/// The simulator's "measurement": the hash of this very binary, the nearest a laptop has to
/// an image digest.
fn measurement() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    hex::encode(tokumai_core::account::sha256(&[&exe]))
}

#[tokio::main]
async fn main() {
    let dir = PathBuf::from("dev-data");
    std::fs::create_dir_all(&dir).expect("create dev-data");
    let root = sim_root(&dir);
    let m = measurement();
    let enclave = Enclave::start(Platform {
        attester: Box::new(sim::SimAttester::new(root, m.clone())),
        keys: Box::new(FileKeyProvider { path: dir.join("data.key") }),
        provider: Box::new(MockProvider),
        db: Db::File(dir.join("ledger.db")),
        prices: [("mock".to_string(), (100, 400))].into_iter().collect(),
        dev_mode: true,
    })
    .expect("start the enclave");
    let enclave: &'static Enclave = Box::leak(Box::new(enclave));
    let addr = "127.0.0.1:7707";
    let listener = TcpListener::bind(addr).await.expect("bind 127.0.0.1:7707");
    println!("tokumai enclave (SIMULATED) on {addr}");
    println!("  measurement {m}");
    println!("  identity    {}", enclave.identity_hex());
    println!("  sim root    {}", hex::encode(sim::root_public(&root)));
    loop {
        let Ok((sock, _)) = listener.accept().await else { continue };
        tokio::spawn(async move {
            let (r, mut w) = sock.into_split();
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut out = enclave.handle(line.as_bytes()).await;
                out.push(b'\n');
                if w.write_all(&out).await.is_err() {
                    break;
                }
            }
        });
    }
}
