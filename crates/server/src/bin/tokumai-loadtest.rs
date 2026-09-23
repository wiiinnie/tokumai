//! Many people at once, against a real enclave over the real mixnet.
//!
//!     cargo run --release -p tokumai-server --bin tokumai-loadtest -- [options]
//!       --users N      how many at the same time (default 10)
//!       --secs S       for how long (default 60)
//!       --op NAME      what each of them asks for (default "balance")
//!       --same-account everyone on one account, to put the book itself under contention
//!
//! It asks for things that cost nothing: `balance` and `start` are a signature check, a
//! nonce written down, and a look at the book — the whole path a question takes, minus the
//! model. So a hundred of these cost a hundred nothing.
//!
//! Every user brings their own mixnet client, as a real app does, which is also the limit:
//! past about forty the Mac running the test is the bottleneck, not the enclave (learned
//! the same way in the first server). Two machines, if a number beyond that is needed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokumai_client::app::{Connection, MixConnector};
use tokumai_client::gateways::EntryChoice;

#[derive(Default)]
struct Tally {
    done: AtomicU64,
    failed: AtomicU64,
    /// Every answer's milliseconds, for the percentiles.
    times: std::sync::Mutex<Vec<u64>>,
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    let users: usize = arg("--users").and_then(|v| v.parse().ok()).unwrap_or(10);
    let secs: u64 = arg("--secs").and_then(|v| v.parse().ok()).unwrap_or(60);
    let op = arg("--op").unwrap_or_else(|| "balance".into());
    let same_account = std::env::args().any(|a| a == "--same-account");

    let (address, pcr0) = tokumai_server::probe_target();
    let address = std::env::var("TOKUMAI_ENCLAVE").ok().or(address).expect("no enclave — deploy a probe first");
    let pcr0 = std::env::var("TOKUMAI_PCR0").ok().or(pcr0).expect("no image to pin — deploy a probe first");
    let policy = tokumai_attest::Policy { measurements: vec![pcr0.trim().to_lowercase()], simulated_root: None, simulated_any_measurement: false };
    println!("{users} at once for {secs} s, asking \"{op}\" of {}…", &address[..16]);

    let shared = same_account.then(|| tokumai_core::account::create_account().mnemonic);
    let tally: Arc<Tally> = Default::default();
    let until = Instant::now() + Duration::from_secs(secs);
    let mut running = Vec::new();
    for _ in 0..users {
        let (op, address, policy, tally) = (op.clone(), address.clone(), policy.clone(), tally.clone());
        let phrase = shared.clone().unwrap_or_else(|| tokumai_core::account::create_account().mnemonic);
        running.push(tokio::spawn(async move {
            let account = tokumai_core::account::from_mnemonic(&phrase).expect("a phrase");
            // Rule A1 holds here too: a random entry gateway, never one of ours.
            let connector = Box::new(MixConnector::new(&address, EntryChoice::Random));
            let mut conn = Connection::new(connector, policy);
            while Instant::now() < until {
                let started = Instant::now();
                match conn.call(&account, &op, &serde_json::json!({})).await {
                    Ok(answer) if answer["kind"] != "error" => {
                        tally.done.fetch_add(1, Ordering::Relaxed);
                        tally.times.lock().expect("times").push(started.elapsed().as_millis() as u64);
                    }
                    Ok(answer) => {
                        tally.failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("refused: {}", answer["error"].as_str().unwrap_or("?"));
                    }
                    Err(e) => {
                        tally.failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("failed: {e}");
                    }
                }
            }
        }));
    }

    // Say how it is going while it goes, so a run that is already sick can be stopped.
    let watching = tally.clone();
    let ticker = tokio::spawn(async move {
        let started = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let (done, failed) = (watching.done.load(Ordering::Relaxed), watching.failed.load(Ordering::Relaxed));
            println!("  {:>3} s · {done} answered, {failed} failed · {:.1}/s", started.elapsed().as_secs(), done as f64 / started.elapsed().as_secs_f64().max(0.001));
        }
    });

    for r in running {
        let _ = r.await;
    }
    ticker.abort();

    let mut times = tally.times.lock().expect("times").clone();
    times.sort_unstable();
    let at = |p: f64| times.get(((times.len() as f64 * p) as usize).min(times.len().saturating_sub(1))).copied().unwrap_or(0);
    let (done, failed) = (tally.done.load(Ordering::Relaxed), tally.failed.load(Ordering::Relaxed));
    println!("\n{done} answered, {failed} failed, {:.1} answers/s", done as f64 / secs as f64);
    if !times.is_empty() {
        println!("half within {} ms, nine in ten within {} ms, ninety-nine in a hundred within {} ms (slowest {} ms)", at(0.5), at(0.9), at(0.99), times.last().copied().unwrap_or(0));
    }
}
