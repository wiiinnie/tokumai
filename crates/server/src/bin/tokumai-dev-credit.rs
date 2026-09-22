//! Development only: put TOKU on the account of the app running on this machine (a debug
//! build), in the simulated enclave. The phrase is read from the app's own dev profile and
//! its dev key file — never printed — and the request is signed by that account, like any
//! other.
//!
//!     cargo run -p tokumai-server --bin tokumai-dev-credit -- [toku]    (default 100000)
//!
//! Against a real probe enclave over the mixnet: TOKUMAI_ENCLAVE=<its Nym address>
//! TOKUMAI_PCR0=<its image>.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokumai_attest::{sim, Policy};
use tokumai_proto::session::{attest_request, Session};

fn app_phrase() -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "no HOME")?;
    let profile = std::fs::read_to_string(format!("{home}/Library/Application Support/com.tokumai.app.dev/profile.json"))
        .map_err(|_| "no dev app profile — start the app (tauri dev) and create an account first")?;
    let key: [u8; 32] = std::fs::read("dev-data/app-keys/profile-encryption-key.key")
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("no dev profile key in dev-data/app-keys")?;
    let env: Value = serde_json::from_str(&profile).map_err(|e| e.to_string())?;
    let nonce = B64.decode(env["nonce"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
    let ct = B64.decode(env["ct"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
    let plain = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| e.to_string())?
        .decrypt(Nonce::from_slice(&nonce), ct.as_slice())
        .map_err(|_| "the dev profile does not open with the dev key")?;
    let p: Value = serde_json::from_slice(&plain).map_err(|e| e.to_string())?;
    p["mnemonic"].as_str().map(str::to_string).ok_or_else(|| "the app has no account yet".into())
}

async fn roundtrip(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    msg: Vec<u8>,
) -> Vec<u8> {
    w.write_all(&msg).await.expect("send");
    w.write_all(b"\n").await.expect("send");
    lines.next_line().await.expect("read").expect("an answer").into_bytes()
}

#[tokio::main]
async fn main() {
    let toku: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let account = tokumai_core::account::from_mnemonic(&app_phrase().unwrap_or_else(|e| panic!("{e}"))).expect("phrase");
    let policy = match std::env::var("TOKUMAI_PCR0").ok().filter(|p| !p.trim().is_empty()) {
        Some(pcr0) => Policy { measurements: vec![pcr0.trim().to_lowercase()], simulated_root: None, simulated_any_measurement: false },
        None => {
            let root: [u8; 32] = std::fs::read("dev-data/sim-root.key").expect("start tokumai-enclave-dev first").try_into().expect("32 bytes");
            Policy { measurements: vec![], simulated_root: Some(sim::root_public(&root)), simulated_any_measurement: true }
        }
    };
    let answer = match std::env::var("TOKUMAI_ENCLAVE").ok().filter(|a| !a.trim().is_empty()) {
        // A real enclave, over the mixnet (rule A1 picks the entry gateway).
        Some(address) => {
            let connector = tokumai_client::app::MixConnector::new(&address, tokumai_client::gateways::EntryChoice::Random);
            let mut conn = tokumai_client::app::Connection::new(Box::new(connector), policy);
            conn.call(&account, "dev.credit", &json!({ "toku": toku })).await.expect("credit")
        }
        None => {
            let sock = TcpStream::connect("127.0.0.1:7707").await.expect("connect to the simulated enclave");
            let (r, mut w) = sock.into_split();
            let mut lines = BufReader::new(r).lines();
            let nonce: [u8; 32] = rand::random();
            let reply = roundtrip(&mut w, &mut lines, attest_request(&nonce)).await;
            let session = Session::from_attestation(&reply, &nonce, &policy, None).expect("attestation");
            let (p, bytes) = session.request(&account, "dev.credit", &json!({ "toku": toku }), tokumai_proto::now_ms());
            p.open(&roundtrip(&mut w, &mut lines, bytes).await).expect("answer")
        }
    };
    println!("account {}…: {}", &account.account_id[..16], answer["balance"]["total"]);
}
