// stats.rs — per-request samples → CSV on the fly, live progress, final summary.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Sample {
    pub client: usize,
    pub op: String,
    pub seq: u64,
    /// ms since the run started (send instant).
    pub start_ms: u64,
    pub latency_ms: u64,
    pub ok: bool,
    /// Empty when ok. Otherwise a short class + detail ("timeout", "busy: …", "server: …").
    pub err: String,
    pub reply_bytes: usize,
}

/// Live counters the collector shows while the run is going.
#[derive(Default)]
pub struct Live {
    pub connected: AtomicUsize,
    pub connect_failed: AtomicUsize,
    pub inflight: AtomicUsize,
    pub finished_clients: AtomicUsize,
}

/// Drain samples until every sender is gone; write CSV as they come; print a progress
/// line every few seconds. Returns everything collected for the summary.
pub async fn collect(
    mut rx: mpsc::Receiver<Sample>,
    csv_path: &Path,
    live: Arc<Live>,
    clients: usize,
    t0: Instant,
) -> Vec<Sample> {
    let mut csv = std::fs::File::create(csv_path).expect("create samples.csv");
    writeln!(csv, "client,op,seq,start_ms,latency_ms,ok,error,reply_bytes").ok();
    let mut all: Vec<Sample> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.tick().await; // the first tick fires immediately — skip it
    loop {
        tokio::select! {
            s = rx.recv() => {
                let Some(s) = s else { break };
                writeln!(
                    csv, "{},{},{},{},{},{},\"{}\",{}",
                    s.client, s.op, s.seq, s.start_ms, s.latency_ms, s.ok as u8,
                    s.err.replace('"', "'"), s.reply_bytes
                ).ok();
                all.push(s);
            }
            _ = tick.tick() => progress(&all, &live, clients, t0),
        }
    }
    csv.flush().ok();
    all
}

fn progress(all: &[Sample], live: &Live, clients: usize, t0: Instant) {
    let reqs: Vec<&Sample> = all.iter().filter(|s| s.op != "connect").collect();
    let ok = reqs.iter().filter(|s| s.ok).count();
    let err = reqs.len() - ok;
    let recent: Vec<u64> = reqs
        .iter()
        .rev()
        .take(50)
        .filter(|s| s.ok)
        .map(|s| s.latency_ms)
        .collect();
    let p50 = percentile(&recent, 50.0);
    eprintln!(
        "[{:>5.0}s] clients {}/{} up ({} failed, {} done) · in flight {} · done {} ok / {} err · recent p50 {}",
        t0.elapsed().as_secs_f64(),
        live.connected.load(Ordering::Relaxed),
        clients,
        live.connect_failed.load(Ordering::Relaxed),
        live.finished_clients.load(Ordering::Relaxed),
        live.inflight.load(Ordering::Relaxed),
        ok,
        err,
        fmt_ms(p50),
    );
}

/// Nearest-rank percentile over an UNSORTED slice (0 when empty).
pub fn percentile(v: &[u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    s[rank.clamp(1, s.len()) - 1]
}

pub fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

struct OpStats {
    n: usize,
    ok: usize,
    timeouts: usize,
    busy: usize,
    lat: Vec<u64>,
    bytes: u64,
    first_ms: u64,
    last_end_ms: u64,
}

/// Print the per-op table + error breakdown and return the same numbers as JSON.
pub fn summarize(all: &[Sample], header: Value, wall: Duration) -> Value {
    let mut ops: BTreeMap<String, OpStats> = BTreeMap::new();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for s in all {
        let e = ops.entry(s.op.clone()).or_insert_with(|| OpStats {
            n: 0, ok: 0, timeouts: 0, busy: 0, lat: Vec::new(), bytes: 0, first_ms: u64::MAX, last_end_ms: 0,
        });
        e.n += 1;
        e.first_ms = e.first_ms.min(s.start_ms);
        e.last_end_ms = e.last_end_ms.max(s.start_ms + s.latency_ms);
        if s.ok {
            e.ok += 1;
            e.lat.push(s.latency_ms);
            e.bytes += s.reply_bytes as u64;
        } else {
            if s.err == "timeout" {
                e.timeouts += 1;
            }
            if s.err.starts_with("busy") {
                e.busy += 1;
            }
            // Group by the error's class + first 70 chars so the table stays readable.
            let key: String = s.err.chars().take(70).collect();
            *errors.entry(key).or_default() += 1;
        }
    }

    println!();
    println!("{:<18} {:>6} {:>6} {:>5} {:>5} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "op", "n", "ok", "t/o", "busy", "p50", "p90", "p99", "max", "mean", "req/s");
    let mut j = serde_json::Map::new();
    for (op, st) in &ops {
        let mean = if st.lat.is_empty() { 0 } else { st.lat.iter().sum::<u64>() / st.lat.len() as u64 };
        let span_s = (st.last_end_ms.saturating_sub(st.first_ms)) as f64 / 1000.0;
        let rps = if span_s > 0.0 { st.ok as f64 / span_s } else { 0.0 };
        println!(
            "{:<18} {:>6} {:>6} {:>5} {:>5} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7.2}",
            op, st.n, st.ok, st.timeouts, st.busy,
            fmt_ms(percentile(&st.lat, 50.0)), fmt_ms(percentile(&st.lat, 90.0)),
            fmt_ms(percentile(&st.lat, 99.0)), fmt_ms(*st.lat.iter().max().unwrap_or(&0)),
            fmt_ms(mean), rps
        );
        j.insert(op.clone(), json!({
            "n": st.n, "ok": st.ok, "timeouts": st.timeouts, "busy": st.busy,
            "p50_ms": percentile(&st.lat, 50.0), "p90_ms": percentile(&st.lat, 90.0),
            "p99_ms": percentile(&st.lat, 99.0), "max_ms": st.lat.iter().max().copied().unwrap_or(0),
            "mean_ms": mean, "ok_per_s": rps, "reply_bytes_total": st.bytes,
        }));
    }
    let total: usize = ops.iter().filter(|(k, _)| *k != "connect").map(|(_, s)| s.n).sum();
    let total_ok: usize = ops.iter().filter(|(k, _)| *k != "connect").map(|(_, s)| s.ok).sum();
    println!();
    println!(
        "requests {total} · ok {total_ok} ({:.1}%) · wall {:.0}s · overall {:.2} ok req/s",
        if total > 0 { 100.0 * total_ok as f64 / total as f64 } else { 0.0 },
        wall.as_secs_f64(),
        if wall.as_secs_f64() > 0.0 { total_ok as f64 / wall.as_secs_f64() } else { 0.0 }
    );
    if !errors.is_empty() {
        println!("errors:");
        let mut e: Vec<_> = errors.iter().collect();
        e.sort_by(|a, b| b.1.cmp(a.1));
        for (msg, n) in e.iter().take(12) {
            println!("  {n:>5} × {msg}");
        }
    }
    json!({
        "run": header,
        "wall_s": wall.as_secs_f64(),
        "requests": total, "ok": total_ok,
        "ops": Value::Object(j),
        "errors": errors,
    })
}
