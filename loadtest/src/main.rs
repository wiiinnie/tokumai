// scrai-loadtest — how many users can ONE scrai-server serve at once, and at what latency?
//
// N ephemeral Nym clients (one per simulated user) connect with a ramp, optionally buy +
// withdraw + redeem credit the way the app does (fake-payments server only), then run a
// request loop: ping (mixnet + dispatch loop only), models (spawned catalog), or signed
// chat (reserve → provider → settle → persist; pair with MOCK_PROVIDER to keep the
// real model out of it). Every request is one CSV row; the end prints p50/p90/p99 per op.
//
//   cargo run --release -p scrai-loadtest -- --server <id.enc@gw> --mode ping --clients 10
//   scripts/loadtest.sh            # local fake server + staged runs (see docs/load-testing.md)

mod keys;
mod mix;
mod proto;
mod stats;

use mix::{Mix, Perf};
use nym_sdk::mixnet::Recipient;
use proto::{Ctx, User};
use serde_json::json;
use stats::{Live, Sample};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Set by Ctrl+C: loops stop after their current request, the summary still prints.
static STOP: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Ping,
    Models,
    Chat,
    /// Per user, round-robin: chat, ping, models — a rough "everything at once" mix.
    Mixed,
}

struct Args {
    server: String,
    clients: usize,
    mode: Mode,
    requests: u64,
    duration: Option<Duration>,
    think_ms: u64,
    ramp_ms: u64,
    inflight: usize,
    timeout: Duration,
    surbs: u32,
    surbs_chat: u32,
    perf: Perf,
    gateway: Option<String>,
    usd: u32,
    tender_coins: u64,
    model: String,
    prompt_bytes: usize,
    max_tokens: u64,
    skip_status: bool,
    out: PathBuf,
    label: String,
}

const USAGE: &str = "\
scrai-loadtest — N simulated users against one scrai-server over the mixnet

  --server <addr>[,<addr>…]   the server's Nym address(es) (or SERVER_ADDRESS);
                     several = the same server's extra identities, users spread round-robin
  --mode ping|models|chat|mixed   what each user sends (default ping)
  --clients N        simulated users = independent Nym clients (default 10)
  --requests R       requests per user after connect/fund (default 10)
  --duration S       instead of --requests: keep sending for S seconds per user
  --think-ms MS      pause between a user's requests (default 0)
  --ramp-ms MS       stagger between client connects (default 1000)
  --inflight K       concurrent requests per user, ping/models only (default 1)
  --timeout-ms MS    per-request reply deadline (default 120000, like the app)
  --surbs N          reply SURBs on control requests (default 80 = app's SURBS_SMALL)
  --surbs-chat N     reply SURBs on chat (default 150 = app's SURBS_TEXT)
  --fast             mixnet knobs at the app's performance end (default: privacy = Nym defaults)
  --gateway ID       pin every client to this entry gateway (default: SDK picks per client)
  --usd N            purchase tier per user for chat/mixed (default 5 = one ticketbook)
  --tender-coins N   coins a chat puts on the table (default 31 = a text prompt's ceiling)
  --model M          chat model (default gemini-3.5-flash-lite)
  --prompt-bytes N   size of the user message (default 300)
  --max-tokens N     maxTokens on chat (default 64)
  --out DIR          results root (default loadtest/results)
  --label TEXT       tag for the results directory
";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        server: std::env::var("TOKUMAI_SERVER_ADDRESS")
            .or_else(|_| std::env::var("SCRAI_SERVER_ADDRESS"))
            .unwrap_or_default(),
        clients: 10,
        mode: Mode::Ping,
        requests: 10,
        duration: None,
        think_ms: 0,
        ramp_ms: 1000,
        inflight: 1,
        timeout: Duration::from_millis(120_000),
        surbs: 80,
        surbs_chat: 150,
        perf: Perf::PRIVACY,
        gateway: None,
        usd: 5,
        tender_coins: 31,
        model: "gemini-3.5-flash-lite".into(),
        prompt_bytes: 300,
        max_tokens: 64,
        skip_status: false,
        out: PathBuf::from("loadtest/results"),
        label: String::new(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let next = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        argv.get(*i).cloned().ok_or_else(|| format!("{flag} needs a value"))
    };
    while i < argv.len() {
        let f = argv[i].as_str();
        match f {
            "--server" => a.server = next(&mut i, f)?,
            "--mode" => {
                a.mode = match next(&mut i, f)?.as_str() {
                    "ping" => Mode::Ping,
                    "models" => Mode::Models,
                    "chat" => Mode::Chat,
                    "mixed" => Mode::Mixed,
                    m => return Err(format!("unknown mode {m}")),
                }
            }
            "--clients" => a.clients = next(&mut i, f)?.parse().map_err(|_| "--clients: number")?,
            "--requests" => a.requests = next(&mut i, f)?.parse().map_err(|_| "--requests: number")?,
            "--duration" => a.duration = Some(Duration::from_secs(next(&mut i, f)?.parse().map_err(|_| "--duration: seconds")?)),
            "--think-ms" => a.think_ms = next(&mut i, f)?.parse().map_err(|_| "--think-ms: number")?,
            "--ramp-ms" => a.ramp_ms = next(&mut i, f)?.parse().map_err(|_| "--ramp-ms: number")?,
            "--inflight" => a.inflight = next(&mut i, f)?.parse().map_err(|_| "--inflight: number")?,
            "--timeout-ms" => a.timeout = Duration::from_millis(next(&mut i, f)?.parse().map_err(|_| "--timeout-ms: number")?),
            "--surbs" => a.surbs = next(&mut i, f)?.parse().map_err(|_| "--surbs: number")?,
            "--surbs-chat" => a.surbs_chat = next(&mut i, f)?.parse().map_err(|_| "--surbs-chat: number")?,
            "--fast" => a.perf = Perf::FAST,
            "--gateway" => a.gateway = Some(next(&mut i, f)?),
            "--usd" => a.usd = next(&mut i, f)?.parse().map_err(|_| "--usd: number")?,
            "--tender-coins" => a.tender_coins = next(&mut i, f)?.parse().map_err(|_| "--tender-coins: number")?,
            "--model" => a.model = next(&mut i, f)?,
            "--prompt-bytes" => a.prompt_bytes = next(&mut i, f)?.parse().map_err(|_| "--prompt-bytes: number")?,
            "--max-tokens" => a.max_tokens = next(&mut i, f)?.parse().map_err(|_| "--max-tokens: number")?,
            "--out" => a.out = PathBuf::from(next(&mut i, f)?),
            "--label" => a.label = next(&mut i, f)?,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag {other}\n\n{USAGE}")),
        }
        i += 1;
    }
    if a.server.is_empty() {
        return Err(format!("--server is required\n\n{USAGE}"));
    }
    if a.clients == 0 || a.inflight == 0 {
        return Err("--clients and --inflight must be ≥ 1".into());
    }
    if matches!(a.mode, Mode::Chat | Mode::Mixed) && a.inflight > 1 {
        eprintln!("note: chat is strictly sequential per user (counter + 1) — --inflight ignored");
        a.inflight = 1;
    }
    Ok(a)
}

/// A user message of roughly `bytes` characters (the guard/prune-free "plain question").
fn prompt(bytes: usize) -> String {
    const S: &str = "Explain in two short paragraphs why mix networks add latency and what cover traffic buys. ";
    let mut p = String::new();
    while p.len() < bytes {
        p.push_str(S);
    }
    p.truncate(bytes);
    p
}

#[tokio::main]
async fn main() {
    // Keep the SDK's chatter down unless RUST_LOG says otherwise (first logger wins).
    env_logger::Builder::new().parse_filters("warn").parse_default_env().try_init().ok();
    let args = match parse_args() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let servers: Vec<Recipient> = args
        .server
        .split(',')
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|a| match Recipient::try_from_base58_string(a) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("bad --server address {a}: {e}");
                std::process::exit(2);
            }
        })
        .collect();
    if servers.is_empty() {
        eprintln!("--server: no address");
        std::process::exit(2);
    }

    let stamp = {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        format!("{t}")
    };
    let dir_name = if args.label.is_empty() {
        format!("{stamp}-{:?}-{}c", args.mode, args.clients).to_lowercase()
    } else {
        format!("{stamp}-{}", args.label)
    };
    let run_dir = args.out.join(dir_name);
    std::fs::create_dir_all(&run_dir).expect("create results dir");

    let header = json!({
        "server": args.server, "mode": format!("{:?}", args.mode), "clients": args.clients,
        "requests": args.requests, "duration_s": args.duration.map(|d| d.as_secs()),
        "think_ms": args.think_ms, "ramp_ms": args.ramp_ms, "inflight": args.inflight,
        "timeout_ms": args.timeout.as_millis() as u64, "surbs": args.surbs, "surbs_chat": args.surbs_chat,
        "perf": args.perf.to_string(), "gateway": args.gateway, "usd": args.usd,
        "tender_coins": args.tender_coins, "model": args.model, "prompt_bytes": args.prompt_bytes,
        "max_tokens": args.max_tokens, "status_before_chat": !args.skip_status, "app": proto::APP,
    });
    eprintln!("scrai-loadtest v{} → {} address(es)", proto::APP, servers.len());
    for (i, a) in args.server.split(',').filter(|a| !a.trim().is_empty()).enumerate() {
        eprintln!("  [{i}] {}", a.trim());
    }
    eprintln!(
        "  {:?} · {} clients (ramp {} ms) · {} · timeout {} s · mixnet {}",
        args.mode,
        args.clients,
        args.ramp_ms,
        match args.duration {
            Some(d) => format!("{} s per user", d.as_secs()),
            None => format!("{} requests per user", args.requests),
        },
        args.timeout.as_secs(),
        args.perf
    );
    if matches!(args.mode, Mode::Chat | Mode::Mixed) {
        eprintln!(
            "  chat: fund ${} → redeem {} coins → model {} · prompt {} B · maxTokens {} · session.status before chat: {}",
            args.usd, args.tender_coins, args.model, args.prompt_bytes, args.max_tokens, !args.skip_status
        );
    }
    eprintln!("  results → {}", run_dir.display());

    let t0 = Instant::now();
    let live = Arc::new(Live::default());
    let (tx, rx) = mpsc::channel::<Sample>(4096);
    let collector = tokio::spawn({
        let live = live.clone();
        let csv = run_dir.join("samples.csv");
        let clients = args.clients;
        async move { stats::collect(rx, &csv, live, clients, t0).await }
    });

    let mut tasks = Vec::new();
    for i in 0..args.clients {
        let (args, tx, live) = (args.clone(), tx.clone(), live.clone());
        let server = servers[i % servers.len()];
        tasks.push(tokio::spawn(Box::pin(run_client(i, args, server, tx, live, t0))));
    }
    drop(tx);

    // Wait for every user — or Ctrl+C, which asks the loops to wind down.
    let all = async {
        for t in tasks {
            let _ = t.await;
        }
    };
    tokio::select! {
        _ = all => {}
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Ctrl+C — stopping after in-flight requests, summary follows");
            STOP.store(true, Ordering::Relaxed);
        }
    }
    let samples = match tokio::time::timeout(args.timeout + Duration::from_secs(10), collector).await {
        Ok(Ok(s)) => s,
        _ => {
            eprintln!("collector did not finish — samples.csv holds what arrived");
            return;
        }
    };
    let summary = stats::summarize(&samples, header, t0.elapsed());
    std::fs::write(run_dir.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap()).ok();
    println!("→ {}", run_dir.join("summary.json").display());
}

async fn run_client(
    i: usize,
    args: Arc<Args>,
    server: Recipient,
    tx: mpsc::Sender<Sample>,
    live: Arc<Live>,
    t0: Instant,
) {
    tokio::time::sleep(Duration::from_millis(args.ramp_ms * i as u64)).await;
    if STOP.load(Ordering::Relaxed) {
        return;
    }
    let start = Instant::now();
    let mix = match Mix::connect(args.perf, args.gateway.as_deref()).await {
        Ok(m) => Arc::new(m),
        Err(e) => {
            live.connect_failed.fetch_add(1, Ordering::Relaxed);
            let _ = tx
                .send(Sample {
                    client: i,
                    op: "connect".into(),
                    seq: 0,
                    start_ms: start.duration_since(t0).as_millis() as u64,
                    latency_ms: start.elapsed().as_millis() as u64,
                    ok: false,
                    err: format!("connect: {e}"),
                    reply_bytes: 0,
                })
                .await;
            eprintln!("client {i}: mixnet connect failed: {e}");
            return;
        }
    };
    live.connected.fetch_add(1, Ordering::Relaxed);
    let ctx = Arc::new(Ctx::new(i, mix.clone(), server, args.surbs, args.surbs_chat, args.tender_coins, args.timeout, tx, live.clone(), t0));
    ctx.record("connect", start, true, String::new(), 0).await;
    log::info!("client {i}: connected as {} via gateway {} → server {}", mix.address, mix.gateway, server);

    let mut user = User::new();
    let funded = if matches!(args.mode, Mode::Chat | Mode::Mixed) {
        match proto::fund(&ctx, &mut user, args.usd).await {
            Ok(balance) => {
                log::info!("client {i}: funded, session balance {balance}");
                true
            }
            Err(e) => {
                eprintln!("client {i}: funding failed: {e}");
                false
            }
        }
    } else {
        false
    };

    if funded || matches!(args.mode, Mode::Ping | Mode::Models) {
        let steady = Instant::now();
        let done = |n: u64| -> bool {
            STOP.load(Ordering::Relaxed)
                || match args.duration {
                    Some(d) => steady.elapsed() >= d,
                    None => n >= args.requests,
                }
        };
        match args.mode {
            Mode::Chat | Mode::Mixed => {
                let text = prompt(args.prompt_bytes);
                let mut n = 0u64;
                while !done(n) {
                    let which = if args.mode == Mode::Mixed { n % 3 } else { 0 };
                    let r = match which {
                        0 => proto::chat(&ctx, &mut user, &args.model, &text, args.max_tokens, args.skip_status).await.map(|_| ()),
                        1 => proto::ping(&ctx).await.map(|_| ()),
                        _ => proto::models(&ctx).await.map(|_| ()),
                    };
                    if let Err(e) = r {
                        log::warn!("client {i}: {e}");
                        // A refused chat leaves the counter uncertain — re-sync like the app does.
                        if which == 0 && !e.starts_with("busy") && args.skip_status {
                            
                        }
                    }
                    n += 1;
                    if args.think_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(args.think_ms)).await;
                    }
                }
            }
            Mode::Ping | Mode::Models => {
                // K workers per user share one mixnet client (K requests in flight).
                let per_worker = args.requests.div_ceil(args.inflight as u64);
                let mut workers = Vec::new();
                for _ in 0..args.inflight {
                    let (ctx, args) = (ctx.clone(), args.clone());
                    workers.push(tokio::spawn(async move {
                        let mut n = 0u64;
                        let is_done = |n: u64| {
                            STOP.load(Ordering::Relaxed)
                                || match args.duration {
                                    Some(d) => steady.elapsed() >= d,
                                    None => n >= per_worker,
                                }
                        };
                        while !is_done(n) {
                            let r = match args.mode {
                                Mode::Ping => proto::ping(&ctx).await,
                                _ => proto::models(&ctx).await,
                            };
                            if let Err(e) = r {
                                log::warn!("client {}: {e}", ctx.client);
                            }
                            n += 1;
                            if args.think_ms > 0 {
                                tokio::time::sleep(Duration::from_millis(args.think_ms)).await;
                            }
                        }
                    }));
                }
                for w in workers {
                    let _ = w.await;
                }
            }
        }
    }
    live.finished_clients.fetch_add(1, Ordering::Relaxed);
    mix.shutdown().await;
}
