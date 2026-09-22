//! A stand-in for the app against the simulated enclave: attest, pin the simulator root from
//! ./dev-data, then credit, ask and read the balance — each request signed and sealed.
//!
//!     cargo run -p tokumai-enclave --bin tokumai-dev-client -- "a question" [model]

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokumai_attest::{sim, Policy};
use tokumai_enclave::client::{attest_request, Session};

async fn roundtrip(lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>, w: &mut tokio::net::tcp::OwnedWriteHalf, msg: &[u8]) -> Vec<u8> {
    w.write_all(msg).await.expect("send");
    w.write_all(b"\n").await.expect("send");
    lines.next_line().await.expect("read").expect("an answer").into_bytes()
}

#[tokio::main]
async fn main() {
    let question = std::env::args().nth(1).unwrap_or_else(|| "hello from the dev client".into());
    // Second argument: the model (default the mock; e.g. gemini-3.5-flash-lite with a key set).
    let model = std::env::args().nth(2).unwrap_or_else(|| "mock".into());
    // Third argument: the picture size for image models (512, 1K, 2K, 4K).
    let image_size = std::env::args().nth(3);
    let root: [u8; 32] = std::fs::read("dev-data/sim-root.key").expect("start tokumai-enclave-dev first").try_into().expect("32 bytes");
    let policy = Policy { measurements: vec![], simulated_root: Some(sim::root_public(&root)), simulated_any_measurement: true };
    let phrase = std::fs::read_to_string("dev-data/dev.phrase").unwrap_or_else(|_| {
        let a = tokumai_core::account::create_account();
        std::fs::write("dev-data/dev.phrase", &a.mnemonic).expect("write dev.phrase");
        a.mnemonic
    });
    let account = tokumai_core::account::from_mnemonic(&phrase).expect("dev phrase");

    let sock = TcpStream::connect("127.0.0.1:7707").await.expect("connect to the simulated enclave");
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();

    let nonce: [u8; 32] = rand::random();
    let reply = roundtrip(&mut lines, &mut w, &attest_request(&nonce)).await;
    let session = Session::from_attestation(&reply, &nonce, &policy).expect("attestation");
    println!("attested: {:?} image {}", session.claims.platform, &session.claims.measurement[..16]);
    println!("account:  {}", &account.account_id[..16]);

    let ask = |op: &'static str, body: Value| {
        let (p, bytes) = session.request(&account, op, &body, tokumai_enclave::now_ms());
        (p, bytes, op)
    };
    for (p, bytes, op) in [
        ask("dev.credit", json!({ "toku": 100_000 })),
        ask("chat", json!({ "model": model, "messages": [{ "role": "user", "content": question }], "maxTokens": 512, "imageSize": image_size })),
        ask("balance", json!({})),
    ] {
        let mut answer = p.open(&roundtrip(&mut lines, &mut w, &bytes).await).expect("answer");
        // A picture is a few hundred KB of base64: show its size, not its bytes.
        if let Some(imgs) = answer.get_mut("images").and_then(|i| i.as_array_mut()) {
            for img in imgs.iter_mut() {
                let len = img.get("data").and_then(|d| d.as_str()).map(|d| d.len()).unwrap_or(0);
                // Keep the picture itself where it can be looked at.
                if let (Some(data), Some(mime)) = (img.get("data").and_then(|d| d.as_str()), img.get("mimeType").and_then(|m| m.as_str())) {
                    use base64::Engine as _;
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        let ext = mime.rsplit('/').next().unwrap_or("bin");
                        let _ = std::fs::write(format!("dev-data/last-image.{ext}"), bytes);
                    }
                }
                img["data"] = json!(format!("<{} KB base64>", len / 1024));
            }
        }
        println!("{op:>10}: {answer}");
    }
}
