//! A stand-in for the app against the simulated enclave: attest, pin the simulator root from
//! ./dev-data, then credit, ask and read the balance — each request signed and sealed.
//!
//!     cargo run -p tokumai-server --bin tokumai-dev-client -- [--mix] "a question" [model] [imageSize]
//!
//! Against a real Nitro enclave: TOKUMAI_PCR0=<its PCR0> TOKUMAI_ENCLAVE=<its Nym address>
//! with `--mix` — the proof is then checked against the AWS Nitro root and that image only.
//!
//! With `--mix` the requests go over the Nym mixnet to the address in ./dev-data/nym-address,
//! through a random entry gateway that is not ours (rule A1), and the attestation must name
//! that address. Everything goes through `tokumai_client::app::Connection`, as in the app.

use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokumai_attest::{sim, Policy};
use tokumai_client::app::{BoxFuture, Connection, Connector, MixConnector};
use tokumai_client::gateways::EntryChoice;
use tokumai_client::Transport;

/// JSON lines over TCP to the simulator on this machine.
struct Tcp {
    lines: Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

/// The mixnet connector, saying which entry gateway it drew.
struct Logged(std::sync::Arc<MixConnector>);

impl Connector for Logged {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        Box::pin(async move {
            let t = self.0.connect().await?;
            let entry = self.0.last_entry.lock().ok().and_then(|e| e.clone()).unwrap_or_default();
            let ours = tokumai_client::gateways::OPERATOR_GATEWAYS.contains(&entry.as_str());
            println!("entry:    {entry} (one of ours: {ours})");
            Ok(t)
        })
    }
}

struct TcpConnector;

impl Connector for TcpConnector {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        Box::pin(async {
            let sock = TcpStream::connect("127.0.0.1:7707").await.map_err(|e| format!("connect to the simulated enclave: {e}"))?;
            let (r, w) = sock.into_split();
            Ok(Box::new(Tcp { lines: BufReader::new(r).lines(), w }) as Box<dyn Transport>)
        })
    }
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
    // A real Nitro enclave: TOKUMAI_PCR0 (its published image) and TOKUMAI_ENCLAVE (its Nym
    // address, as it announced it). The policy then pins the image and accepts no simulator.
    let (probe_address, probe_pcr0) = tokumai_server::probe_target();
    let nitro_pcr0 = std::env::var("TOKUMAI_PCR0").ok().filter(|p| !p.trim().is_empty()).or(probe_pcr0);
    let policy = match &nitro_pcr0 {
        Some(pcr0) => Policy { measurements: vec![pcr0.trim().to_lowercase()], simulated_root: None, simulated_any_measurement: false },
        None => {
            let root: [u8; 32] = std::fs::read(tokumai_server::dev_data().join("sim-root.key")).expect("start tokumai-enclave-dev first").try_into().expect("32 bytes");
            Policy { measurements: vec![], simulated_root: Some(sim::root_public(&root)), simulated_any_measurement: true }
        }
    };
    let phrase = std::fs::read_to_string(tokumai_server::dev_data().join("dev.phrase")).unwrap_or_else(|_| {
        let a = tokumai_core::account::create_account();
        std::fs::write(tokumai_server::dev_data().join("dev.phrase"), &a.mnemonic).expect("write dev.phrase");
        a.mnemonic
    });
    let account = tokumai_core::account::from_mnemonic(&phrase).expect("dev phrase");

    let started = std::time::Instant::now();
    let connector: Box<dyn Connector> = if mix {
        let address = std::env::var("TOKUMAI_ENCLAVE")
            .ok()
            .or(probe_address)
            .or_else(|| std::fs::read_to_string(tokumai_server::dev_data().join("nym-address")).ok())
            .expect("start tokumai-enclave-dev --mix, or a probe with deploy/aws/probe.sh");
        // Rule A1: a random entry gateway, never one of ours, never the enclave's own.
        Box::new(Logged(std::sync::Arc::new(MixConnector::new(&address, EntryChoice::Random))))
    } else {
        Box::new(TcpConnector)
    };
    let mut conn = Connection::new(connector, policy);
    conn.ready().await.expect("connect and attest");
    let session = conn.session().expect("attested");
    println!("attested: {:?} image {} ({} ms, connect included)", session.claims.platform, &session.claims.measurement[..16], started.elapsed().as_millis());
    if !session.address.is_empty() {
        println!("address:  the enclave's own client, {}…", &session.address[..16]);
    }
    println!("account:  {}", &account.account_id[..16]);

    // Measuring the mixnet with a big answer, without paying a model for a picture:
    //     TOKUMAI_BYTES=2000000 … --mix
    if let Some(bytes) = std::env::var("TOKUMAI_BYTES").ok().and_then(|v| v.parse::<u64>().ok()) {
        let t = std::time::Instant::now();
        let answer = conn.call(&account, "dev.bytes", &json!({ "bytes": bytes })).await.expect("answer");
        let got = answer["data"].as_str().map(str::len).unwrap_or(0);
        if got == 0 {
            println!("dev.bytes: nothing came back — {}", serde_json::to_string(&answer).unwrap_or_default().chars().take(300).collect::<String>());
        }
        let secs = t.elapsed().as_secs_f64();
        println!("dev.bytes: {got} bytes in {:.1} s = {:.0} KB/s", secs, got as f64 / 1024.0 / secs);
        return;
    }
    let asks = [
        ("dev.credit", json!({ "toku": 100_000 })),
        ("chat", json!({ "model": model, "messages": [{ "role": "user", "content": question }], "maxTokens": 512, "imageSize": image_size })),
        ("balance", json!({})),
        // Also a sign of what the enclave was given: plans need the Stripe keys, which
        // only a sealed enclave has.
        ("plans", json!({})),
    ];
    for (op, body) in asks {
        let t = std::time::Instant::now();
        let mut answer = conn.call(&account, op, &body).await.expect("answer");
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
