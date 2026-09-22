//! A stand-in for the app against the simulated enclave: attest, pin the simulator root from
//! ./dev-data, then credit, ask and read the balance — each request signed and sealed.
//!
//!     cargo run -p tokumai-server --bin tokumai-dev-client -- [--mix] "a question" [model] [imageSize]
//!
//! With `--mix` the requests go over the Nym mixnet to the address in ./dev-data/nym-address,
//! and the attestation must name that address.

use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokumai_attest::{sim, Policy};
use tokumai_enclave::client::{attest_request, Session};
use tokumai_server::Transport;

/// JSON lines over TCP to the simulator on this machine.
struct Tcp {
    lines: Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

impl Transport for Tcp {
    fn roundtrip<'a>(&'a mut self, msg: &'a [u8]) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>> {
        Box::pin(async move {
            self.w.write_all(msg).await.map_err(|e| e.to_string())?;
            self.w.write_all(b"\n").await.map_err(|e| e.to_string())?;
            let line = self.lines.next_line().await.map_err(|e| e.to_string())?.ok_or("the enclave hung up")?;
            Ok(line.into_bytes())
        })
    }
    fn reached_at(&self) -> Option<String> {
        None
    }
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mix = args.first().map(|a| a == "--mix").unwrap_or(false);
    if mix {
        args.remove(0);
    }
    let question = args.first().cloned().unwrap_or_else(|| "hello from the dev client".into());
    // Second argument: the model (default the mock; e.g. gemini-3.5-flash-lite with a key set).
    let model = args.get(1).cloned().unwrap_or_else(|| "mock".into());
    // Third argument: the picture size for image models (1K, 2K, 4K).
    let image_size = args.get(2).cloned();
    let root: [u8; 32] = std::fs::read("dev-data/sim-root.key").expect("start tokumai-enclave-dev first").try_into().expect("32 bytes");
    let policy = Policy { measurements: vec![], simulated_root: Some(sim::root_public(&root)), simulated_any_measurement: true };
    let phrase = std::fs::read_to_string("dev-data/dev.phrase").unwrap_or_else(|_| {
        let a = tokumai_core::account::create_account();
        std::fs::write("dev-data/dev.phrase", &a.mnemonic).expect("write dev.phrase");
        a.mnemonic
    });
    let account = tokumai_core::account::from_mnemonic(&phrase).expect("dev phrase");

    let started = std::time::Instant::now();
    let mut transport: Box<dyn Transport> = if mix {
        let address = std::fs::read_to_string("dev-data/nym-address").expect("start tokumai-enclave-dev --mix first");
        let t = tokumai_server::mix::MixTransport::connect(&address).await.expect("mixnet");
        println!("on the mixnet as {} ({} ms)", &t.own_address()[..16], started.elapsed().as_millis());
        Box::new(t)
    } else {
        let sock = TcpStream::connect("127.0.0.1:7707").await.expect("connect to the simulated enclave");
        let (r, w) = sock.into_split();
        Box::new(Tcp { lines: BufReader::new(r).lines(), w })
    };

    let t0 = std::time::Instant::now();
    let nonce: [u8; 32] = rand::random();
    let reply = transport.roundtrip(&attest_request(&nonce)).await.expect("attestation answer");
    let reached = transport.reached_at();
    let session = Session::from_attestation(&reply, &nonce, &policy, reached.as_deref()).expect("attestation");
    println!("attested: {:?} image {} ({} ms)", session.claims.platform, &session.claims.measurement[..16], t0.elapsed().as_millis());
    if !session.address.is_empty() {
        println!("address:  the enclave's own client, {}…", &session.address[..16]);
    }
    println!("account:  {}", &account.account_id[..16]);

    let asks = [
        ("dev.credit", json!({ "toku": 100_000 })),
        ("chat", json!({ "model": model, "messages": [{ "role": "user", "content": question }], "maxTokens": 512, "imageSize": image_size })),
        ("balance", json!({})),
    ];
    for (op, body) in asks {
        let t = std::time::Instant::now();
        let (p, bytes) = session.request(&account, op, &body, tokumai_enclave::now_ms());
        let mut answer = p.open(&transport.roundtrip(&bytes).await.expect("answer")).expect("answer");
        show_images(&mut answer);
        println!("{op:>10}: {answer} ({} ms)", t.elapsed().as_millis());
    }
}

/// A picture is a few hundred KB of base64: keep it in dev-data, show its size.
fn show_images(answer: &mut Value) {
    let Some(imgs) = answer.get_mut("images").and_then(|i| i.as_array_mut()) else { return };
    for img in imgs.iter_mut() {
        let len = img.get("data").and_then(|d| d.as_str()).map(|d| d.len()).unwrap_or(0);
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
