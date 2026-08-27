// mixbench — empirical answer to "what actually makes the upload slow?"
//
// Sends messages to our OWN Nym address (loopback through the real mixnet) and times
// send → full-receive. For each performance config we compare three ways to move ~1 MB:
//   1x1MB single  — one message, Nym fragments it into ~2KB Sphinx packets itself
//   8x128KB       — the app's CURRENT chunking (sequential, ack per chunk)
//   2x512KB       — bigger app chunks (fewer round trips)
// across different message_sending_average_delay (send rate) values. The winner tells
// us how to set the send-rate slider and whether app-level chunking should change.
//
// Run:  cargo run -p mixbench --release
// It connects to the live mixnet, so expect ~15s per config to connect.

use nym_sdk::mixnet::{IncludedSurbs, MixnetClient, MixnetClientBuilder, MixnetMessageSender};
use std::time::{Duration, Instant};

fn dbg_config(cover_ms: u64, mix_ms: u64, send_ms: u64, continuous: bool) -> nym_sdk::DebugConfig {
    // EXACTLY the knobs the app sets in nym.rs::debug_config_for.
    let mut d = nym_sdk::DebugConfig::default();
    d.traffic.average_packet_delay = Duration::from_millis(mix_ms.max(1));
    d.traffic.message_sending_average_delay = Duration::from_millis(send_ms.max(1));
    d.cover_traffic.loop_cover_traffic_average_delay = Duration::from_millis(cover_ms.max(1));
    d.cover_traffic.disable_loop_cover_traffic_stream = !continuous;
    d
}

/// Wait until `want` bytes have been reassembled back to us, or `secs` elapse.
async fn recv_bytes(client: &mut MixnetClient, want: usize, secs: u64) -> Option<usize> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut got = 0usize;
    while got < want {
        let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
        match tokio::time::timeout(remaining, client.wait_for_messages()).await {
            Ok(Some(batch)) => {
                for m in batch {
                    got += m.message.len();
                }
            }
            _ => return None, // timeout or stream ended
        }
    }
    Some(got)
}

fn report(label: &str, got: Option<usize>, want: usize, d: Duration) {
    match got {
        Some(g) if g >= want => {
            let kbps = (want as f64 / 1024.0) / d.as_secs_f64();
            println!("  {label:34}  {:>7.2}s   {:>6.1} KB/s", d.as_secs_f64(), kbps);
        }
        _ => println!("  {label:34}  TIMEOUT/incomplete after {:.1}s", d.as_secs_f64()),
    }
}

async fn run_case(client: &mut MixnetClient, me: nym_sdk::mixnet::Recipient) {
    let kb = 1024usize;
    let full = vec![0x61u8; 1000 * kb];
    let c128 = vec![0x62u8; 128 * kb];
    let c512 = vec![0x63u8; 512 * kb];

    // 1) one ~1MB message — Nym fragments + pipelines the packets itself
    let t = Instant::now();
    client.send_message(me, full.clone(), IncludedSurbs::new(20)).await.unwrap();
    let n = recv_bytes(client, full.len(), 180).await;
    report("1x1MB single (nym auto-frag)", n, full.len(), t.elapsed());

    // 2) 8x128KB sequential — the app's current chunking (ack per chunk)
    let t = Instant::now();
    let mut ok = true;
    for _ in 0..8 {
        client.send_message(me, c128.clone(), IncludedSurbs::new(20)).await.unwrap();
        if recv_bytes(client, c128.len(), 90).await.is_none() {
            ok = false;
            break;
        }
    }
    report("8x128KB sequential (app now)", ok.then_some(8 * c128.len()), 8 * c128.len(), t.elapsed());

    // 3) 2x512KB sequential — bigger chunks, fewer round trips
    let t = Instant::now();
    let mut ok = true;
    for _ in 0..2 {
        client.send_message(me, c512.clone(), IncludedSurbs::new(20)).await.unwrap();
        if recv_bytes(client, c512.len(), 120).await.is_none() {
            ok = false;
            break;
        }
    }
    report("2x512KB sequential", ok.then_some(2 * c512.len()), 2 * c512.len(), t.elapsed());

    // 4) one ~5MB message — large single-message throughput under this delay pattern
    let big = vec![0x64u8; 5000 * kb];
    let t = Instant::now();
    client.send_message(me, big.clone(), IncludedSurbs::new(50)).await.unwrap();
    let n = recv_bytes(client, big.len(), 300).await;
    report("1x5MB single (nym auto-frag)", n, big.len(), t.elapsed());
}

#[tokio::main]
async fn main() {
    // (label, cover_ms, mix_ms, send_ms, continuous)
    let configs = [
        ("A  privacy default (send 20ms, mix 15ms, cover on)", 200u64, 15u64, 20u64, true),
        ("B  faster send     (send  5ms, mix 15ms, cover on)", 200, 15, 5, true),
        ("C  fastest         (send  1ms, mix  2ms, cover on)", 200, 2, 1, true),
        ("D  fastest+no cover(send  1ms, mix  2ms, cover OFF)", 3000, 2, 1, false),
    ];

    for (label, cov, mix, snd, cont) in configs {
        println!("\n=== {label} ===");
        print!("  connecting… ");
        let built = match MixnetClientBuilder::new_ephemeral()
            .debug_config(dbg_config(cov, mix, snd, cont))
            .build()
        {
            Ok(b) => b,
            Err(e) => {
                println!("build failed: {e}");
                continue;
            }
        };
        let mut client = match built.connect_to_mixnet().await {
            Ok(c) => c,
            Err(e) => {
                println!("connect failed: {e}");
                continue;
            }
        };
        let me = *client.nym_address();
        println!("connected.");
        // Let the cover-traffic stream settle before measuring.
        tokio::time::sleep(Duration::from_secs(3)).await;
        run_case(&mut client, me).await;
        client.disconnect().await;
    }
    println!("\ndone.");
}
