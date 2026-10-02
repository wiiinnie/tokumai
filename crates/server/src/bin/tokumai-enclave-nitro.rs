//! The enclave, in an AWS Nitro enclave. This is the program the image runs.
//!
//! At start, in this order:
//! 1. the loopback comes up (an enclave boots with none) and the way out opens: a listener on
//!    127.0.0.1:1080 whose connections go over vsock to `tokumai-egress-host` on the host;
//!    `HTTPS_PROXY` and `TOKUMAI_EGRESS_PROXY` point everything at it;
//! 2. the core starts, with the Nitro Secure Module as its attester;
//! 3. its own Nym client connects, through the tunnel, to the pinned gateway; the address it
//!    got is announced to the host (vsock port 8081), and every attestation names it.
//!
//! Its secrets come from AWS KMS and nowhere else: the host hands over a sealed file and
//! the instance's credentials, the enclave asks KMS to open the file for THIS image, and
//! KMS answers with a copy encrypted to a key that exists only in here (`enclave::kms`).
//! Inside are the provider keys, the Stripe keys, the data key everything at rest is keyed
//! with, and the enclave's Nym identity — so its address survives a restart.
//!
//! Without sealed secrets it still starts, with a random data key and the mock model, which
//! is what the first probe did. With them, the book is kept on the host, sealed, as a
//! snapshot and a journal (`tokumai_enclave::state`), and written behind the requests.

use std::path::PathBuf;
use tokumai_attest::nitro::NitroAttester;
use tokumai_core::pricing::PricingTable;
use tokumai_egress::Endpoint;
use tokumai_enclave::policy::PRICING_JSON;
use tokumai_enclave::provider::Providers;
use tokumai_enclave::seal::FixedKeyProvider;
use tokumai_enclave::service::{Db, Enclave, Platform};

const PROBE: &str = "probe-2";
/// The parent instance, as an enclave sees it.
const HOST_CID: u32 = 3;
const EGRESS_PORT: u32 = 8080;
const ANNOUNCE_PORT: u32 = 8081;
/// Where the host answers "credentials" and "sealed".
const HOST_SERVICE_PORT: u32 = 8082;
const REGION: &str = "eu-central-1";
const LOOPBACK_PROXY: &str = "127.0.0.1:1080";

/// Ask the host for the sealed secrets and the instance's credentials, and have KMS open
/// them for this image. The credentials alone open nothing: the key's policy wants an
/// attestation of a published image, which only this enclave can produce.
async fn unseal(attester: &NitroAttester) -> Result<tokumai_enclave::secrets_sealed::Sealed, String> {
    let host = Endpoint::Vsock(HOST_CID, HOST_SERVICE_PORT);
    let envelope = tokumai_enclave::secrets_sealed::Envelope::parse(&tokumai_egress::ask_host(&host, "sealed").await?)?;
    let wrapped = base64_decode(envelope.kms_key.as_bytes())?;
    let credentials = tokumai_egress::ask_host(&host, "credentials").await?;
    let credentials: tokumai_enclave::kms::Credentials = serde_json::from_slice(&credentials).map_err(|e| format!("the host's credentials are unreadable: {e}"))?;
    // A key for this one request; its public half goes into the attestation, so KMS can
    // encrypt its answer to an enclave running exactly this image.
    let (private, public) = tokumai_enclave::kms::request_key()?;
    let document = attester.attest_for_kms(&public)?;
    let key = tokumai_enclave::kms::decrypt_to_enclave(&credentials, REGION, &wrapped, &document, &private, tokumai_proto::now_ms()).await?;
    envelope.open(&key)
}

/// base64, as the sealed pieces travel.
fn base64_decode(text: &[u8]) -> Result<Vec<u8>, String> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    B64.decode(String::from_utf8_lossy(text).trim()).map_err(|e| format!("the sealed file is not base64: {e}"))
}

/// Lay the sealed Nym identity out as files, where its client expects them.
fn write_identity(dir: &std::path::Path, files: &serde_json::Map<String, serde_json::Value>) -> std::io::Result<()> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    std::fs::create_dir_all(dir)?;
    for (name, content) in files {
        // Names come from the sealed file, which only we write — still, no paths.
        let name = name.rsplit('/').next().unwrap_or_default();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let Some(bytes) = content.as_str().and_then(|c| B64.decode(c).ok()) else { continue };
        std::fs::write(dir.join(name), bytes)?;
    }
    Ok(())
}

/// The book, kept on the host and sealed (`enclave::state`). Only the ledger's writer
/// thread calls this (and the start-up reads), so the wait for the host's answer is off
/// every request's path; what the writer hands over carries its number, and the host
/// takes a record it already has as said rather than written again.
struct HostBook {
    ask: std::sync::mpsc::Sender<(String, Vec<u8>, std::sync::mpsc::Sender<Result<Vec<u8>, String>>)>,
}

impl HostBook {
    fn open() -> HostBook {
        let (ask, work) = std::sync::mpsc::channel::<(String, Vec<u8>, std::sync::mpsc::Sender<Result<Vec<u8>, String>>)>();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime for the book");
            let host = Endpoint::Vsock(HOST_CID, HOST_SERVICE_PORT);
            while let Ok((what, body, back)) = work.recv() {
                let answer = runtime.block_on(tokumai_egress::tell_host(&host, &what, &body));
                let _ = back.send(answer);
            }
        });
        HostBook { ask }
    }

    fn call(&self, what: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        let (back, answer) = std::sync::mpsc::channel();
        self.ask
            .send((what.to_string(), body.to_vec(), back))
            .map_err(|_| "the channel to the host is gone — the book cannot be kept".to_string())?;
        // A deadline of its own, longer than the one inside `tell_host`, so that a worker
        // already stuck on an older exchange cannot hold every caller behind it. Without
        // this the enclave attested happily and answered nothing, for hours: a wait that
        // cannot end is not a slow book, it is a dead enclave (2026-09-25).
        answer
            .recv_timeout(std::time::Duration::from_secs(45))
            .map_err(|_| "the host never answered about the book".to_string())?
    }
}

impl tokumai_enclave::state::Store for HostBook {
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        self.call("snapshot", &[])
    }
    fn put_snapshot(&self, sealed: &[u8]) -> Result<(), String> {
        self.call("put-snapshot", sealed).map(|_| ())
    }
    fn journal(&self) -> Result<Vec<u8>, String> {
        self.call("journal", &[])
    }
    /// The record's place goes in front of it in the clear, so the host can tell a record
    /// it already has from the next one (`tokumai_egress::add_record`).
    fn append(&self, generation: u64, number: u64, record: &[u8]) -> Result<(), String> {
        let mut body = generation.to_be_bytes().to_vec();
        body.extend_from_slice(&number.to_be_bytes());
        body.extend_from_slice(record);
        self.call("add-record-2", &body).map(|_| ())
    }
}

/// A voice for threads that have no runtime of their own (the book's writer): the line
/// goes to a thread that announces it over vsock, as a panic's last words do.
fn lend_voice() {
    static LINE: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<String>>> = std::sync::OnceLock::new();
    let (say, heard) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime for the voice");
        while let Ok(words) = heard.recv() {
            eprintln!("{words}");
            let _ = runtime.block_on(tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} {words}")));
        }
    });
    let _ = LINE.set(std::sync::Mutex::new(say));
    tokumai_enclave::voice::speaks(|line| {
        if let Some(l) = LINE.get() {
            if let Ok(l) = l.lock() {
                let _ = l.send(line);
            }
        }
    });
}

/// Say the last words out loud. An enclave has no console in production, so a panic is
/// otherwise a machine that simply disappears: the host sees a hang-up and nothing else.
/// The hook hands the message to a thread of its own, which announces it over vsock the
/// way the addresses are announced.
fn announce_panics() {
    let (say, heard) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime for last words");
        while let Ok(words) = heard.recv() {
            let _ = runtime.block_on(tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &words));
        }
    });
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        let _ = say.send(format!("{PROBE} panicked at {where_}: {what}"));
        // A moment for it to leave the enclave before the process is torn down.
        std::thread::sleep(std::time::Duration::from_millis(400));
        before(info);
    }));
}

/// A pulse from outside the runtime, one from inside it — and, when the inner one stops,
/// a report of what every thread is doing.
///
/// On 2026-09-25 the enclave stopped doing everything at once — three doors, the topology
/// refresh, the Stripe beat — 93 minutes after start, and stayed that way for three days
/// with `nitro-cli` saying RUNNING and every vsock still open. On 2026-09-28 it did the
/// same with ONE door, 101 minutes after start, and this time the two pulses told the
/// story: the thread's went on, the runtime's stopped. The process was alive and the tokio
/// runtime was wedged — every worker thread stuck in something that never returns.
///
/// Where they are stuck is the one thing that cannot be seen from outside, so the thread
/// that is still alive reports it: for every thread of the process, its state and the
/// kernel function it sleeps in (`/proc/self/task/*/wchan`, `/stack`), and its own stack,
/// obtained by sending it a signal whose handler captures a backtrace. Then the process
/// exits, so that the enclave is gone rather than RUNNING and deaf. Two lines a minute
/// until then, about the machine and nobody else.
mod stuck {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;

    /// Seconds since start when a task on the runtime last ran (`heartbeat`).
    pub static RUNTIME_SEEN: AtomicU64 = AtomicU64::new(0);
    /// After this long without the runtime, the report is made and the process ends.
    pub const SILENT_FOR: u64 = 180;

    static SLOT: Mutex<Option<std::backtrace::Backtrace>> = Mutex::new(None);
    static DONE: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_signal(_: libc::c_int) {
        // Runs on the thread being asked. Not strictly signal-safe (it allocates), which
        // is acceptable in a report made once, on a machine that is finished anyway.
        let bt = std::backtrace::Backtrace::force_capture();
        if let Ok(mut s) = SLOT.try_lock() {
            *s = Some(bt);
        }
        DONE.store(true, Ordering::SeqCst);
    }

    pub fn install() {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
        }
    }

    fn read(path: &str) -> String {
        std::fs::read_to_string(path).unwrap_or_default().trim().to_string()
    }

    /// Resident memory in MiB and the number of threads, for the pulse line.
    pub fn vitals() -> String {
        let pages: u64 = read("/proc/self/statm").split(' ').nth(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0);
        format!("rss {} MiB, {threads} threads", pages * page / (1024 * 1024))
    }

    /// A backtrace as one line: the frames that name a function, each with its place.
    fn one_line(bt: &std::backtrace::Backtrace) -> String {
        let text = format!("{bt}");
        let mut frames: Vec<String> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(at) = line.strip_prefix("at ") {
                // "at /tokumai/crates/x/src/y.rs:12:3" → the last frame gets "(y.rs:12)"
                let place = at.rsplit('/').next().unwrap_or(at);
                let place = place.rsplit_once(':').map(|(p, _)| p).unwrap_or(place);
                if let Some(last) = frames.last_mut() {
                    last.push_str(&format!(" ({place})"));
                }
            } else if let Some((_, name)) = line.split_once(": ") {
                frames.push(name.to_string());
            }
        }
        frames.join(" < ")
    }

    /// Every thread: where it is in the kernel and where it is in the program.
    pub fn report(say: &mut dyn FnMut(String)) {
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
            say("no /proc/self/task — the threads cannot be seen".into());
            return;
        };
        let pid = std::process::id() as libc::pid_t;
        let me = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
        for t in tasks.flatten() {
            let Ok(tid) = t.file_name().to_string_lossy().parse::<libc::pid_t>() else { continue };
            let dir = format!("/proc/self/task/{tid}");
            let comm = read(&format!("{dir}/comm"));
            let stat = read(&format!("{dir}/stat"));
            let state = stat.rsplit_once(") ").map(|(_, r)| r.split(' ').next().unwrap_or("?")).unwrap_or("?").to_string();
            let wchan = read(&format!("{dir}/wchan"));
            let kstack = read(&format!("{dir}/stack")).lines().map(|l| l.trim().trim_start_matches("[<0>] ")).collect::<Vec<_>>().join(" < ");
            say(format!("thread {tid} {comm:?} state {state} wchan {wchan} kernel: {kstack}"));
            if tid == me {
                continue;
            }
            DONE.store(false, Ordering::SeqCst);
            if unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGUSR1) } != 0 {
                say(format!("thread {tid}: could not be signalled"));
                continue;
            }
            let t0 = std::time::Instant::now();
            while !DONE.load(Ordering::SeqCst) && t0.elapsed() < std::time::Duration::from_secs(3) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            match SLOT.lock().ok().and_then(|mut s| s.take()) {
                Some(bt) => {
                    let line = one_line(&bt);
                    // The host reads at most 4 KiB per announcement.
                    let bytes = line.as_bytes();
                    let mut at = 0;
                    let mut part = 1;
                    while at < bytes.len() {
                        let mut end = (at + 3500).min(bytes.len());
                        while end < bytes.len() && !line.is_char_boundary(end) {
                            end -= 1;
                        }
                        say(format!("thread {tid} stack {part}: {}", &line[at..end]));
                        at = end;
                        part += 1;
                    }
                }
                None => say(format!("thread {tid}: did not answer the signal in 3 s")),
            }
        }
    }
}

fn heartbeat() {
    stuck::install();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime for the pulse");
        let mut say = |line: String| {
            let _ = runtime.block_on(tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &line));
        };
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
            let now = started.elapsed().as_secs();
            let silent = now.saturating_sub(stuck::RUNTIME_SEEN.load(std::sync::atomic::Ordering::Relaxed));
            say(format!("{PROBE} pulse thread {} min, {}, runtime seen {silent} s ago", now / 60, stuck::vitals()));
            if silent >= stuck::SILENT_FOR {
                say(format!("{PROBE} the runtime has been silent for {silent} s — what every thread is doing:"));
                stuck::report(&mut say);
                say(format!("{PROBE} reported, and leaving: a RUNNING enclave that answers nothing is worse than none"));
                std::thread::sleep(std::time::Duration::from_millis(500));
                std::process::exit(70);
            }
        }
    });
    tokio::spawn(async move {
        let mut n = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            stuck::RUNTIME_SEEN.store(started.elapsed().as_secs(), std::sync::atomic::Ordering::Relaxed);
            n += 1;
            if n % 6 == 0 {
                tokumai_server::say(format!("{PROBE} pulse runtime {} min", started.elapsed().as_secs() / 60));
            }
        }
    });
}

/// `ip link set lo up`, without a shell or iproute2 in the image.
fn loopback_up() -> std::io::Result<()> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo\0".iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        let r = if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut req) < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &req) < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        };
        libc::close(fd);
        r
    }
}

#[tokio::main]
async fn main() {
    println!("tokumai enclave ({PROBE}) starting");
    // Give the enclave a voice before anything can go wrong in it. Without this its
    // `eprintln!`s go nowhere — there is no console in a non-debug enclave — and the host
    // log stops at the last explicit announcement, which is how two days of failures left
    // no trace at all.
    tokumai_server::speaks_to(Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT));
    // …and lends it to the core, which has no way to reach the host on its own.
    tokumai_enclave::trace::speaks(tokumai_server::say);
    lend_voice();
    announce_panics();
    heartbeat();
    loopback_up().expect("bring up the loopback");
    // Everything leaves through the tunnel: the providers, Stripe, Apple and the Nym API by
    // HTTPS_PROXY, the Nym gateway by TOKUMAI_EGRESS_PROXY (vendor/nym-gateway-client).
    std::env::set_var("HTTPS_PROXY", format!("http://{LOOPBACK_PROXY}"));
    std::env::set_var("HTTP_PROXY", format!("http://{LOOPBACK_PROXY}"));
    std::env::set_var("TOKUMAI_EGRESS_PROXY", LOOPBACK_PROXY);
    tokio::spawn(async {
        if let Err(e) = tokumai_egress::forward(Endpoint::Tcp(LOOPBACK_PROXY.into()), Endpoint::Vsock(HOST_CID, EGRESS_PORT)).await {
            eprintln!("tokumai enclave: the way out failed: {e}");
        }
    });

    let attester = NitroAttester::open().expect("the Nitro Secure Module");
    // What the host keeps for us, sealed: only an enclave running a published image can
    // have KMS open it. A probe without sealed secrets still runs, on the mock model.
    let sealed = match unseal(&attester).await {
        Ok(s) => Some(s),
        Err(e) => {
            // Said out loud on the host's side too: a production enclave has no console, and
            // "it runs on the mock model" is otherwise indistinguishable from a working one.
            let pcr0 = attester.pcr0().unwrap_or_else(|e| e);
            eprintln!("tokumai enclave: no sealed secrets ({e}) — the mock model only");
            let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} unsealed-not: {e} (this image measures PCR0 {pcr0})")).await;
            None
        }
    };
    if sealed.is_some() {
        let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} unsealed its secrets")).await;
    }
    let (keys, providers, stripe, apple_api, dev_mode) = match &sealed {
        Some(s) => (
            Box::new(FixedKeyProvider(s.data_key().expect("the sealed data key"))) as Box<dyn tokumai_enclave::seal::KeyProvider>,
            Providers::from_secrets(s),
            tokumai_enclave::stripe::Stripe::from_secrets(s),
            tokumai_enclave::apple::AppleApi::from_secrets(s),
            false,
        ),
        // Nothing sealed: a random key for the run, the mock model, test credit.
        None => (Box::new(FixedKeyProvider(rand::random())) as Box<dyn tokumai_enclave::seal::KeyProvider>, Providers::mock(), None, None, true),
    };
    // The book lives on the host, so the host service has to be there. It may still be
    // starting (a reboot brings both up at once), and an enclave that gives up on the
    // first try dies in a second and looks like a broken image — which is what happened.
    if sealed.is_some() {
        for attempt in 1..=30 {
            match tokumai_egress::ask_host(&Endpoint::Vsock(HOST_CID, HOST_SERVICE_PORT), "snapshot").await {
                Ok(_) => break,
                Err(e) if attempt == 30 => {
                    let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} cannot reach its book: {e}")).await;
                    panic!("the host is not keeping the book: {e}");
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            }
        }
    }
    // The witness outside the machine (tokumai_enclave::witness): the bucket is part of
    // the image, so the host cannot point the enclave at one of its own. The enclave does
    // not start on a book the witness has seen the future of, and does not start without
    // the witness's answer either — a book that cannot be checked is not run.
    let witness = match (&sealed, std::env::var("TOKUMAI_WITNESS_BUCKET").ok().filter(|b| !b.is_empty())) {
        (Some(_), Some(name)) => {
            let bucket = std::sync::Arc::new(tokumai_enclave::witness::Bucket { name, region: REGION.to_string() });
            let mut seen = None;
            for attempt in 1..=30 {
                let looked = async {
                    let creds = tokumai_egress::ask_host(&Endpoint::Vsock(HOST_CID, HOST_SERVICE_PORT), "credentials").await?;
                    let creds: tokumai_enclave::kms::Credentials = serde_json::from_slice(&creds).map_err(|e| format!("the host's credentials are unreadable: {e}"))?;
                    tokumai_enclave::witness::read(&bucket, &creds, tokumai_proto::now_ms()).await
                }
                .await;
                match looked {
                    Ok(s) => {
                        seen = Some(s);
                        break;
                    }
                    Err(e) if attempt == 30 => {
                        let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} cannot read its witness: {e}")).await;
                        panic!("the witness cannot be read: {e}");
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_secs(4)).await,
                }
            }
            let seen = seen.expect("looked");
            let _ = tokumai_egress::announce(
                &Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT),
                &match seen.mark {
                    Some(((g, n), _)) => format!("{PROBE} witness: last mark generation {g} record {n}, {} acknowledgement(s)", seen.accepts.len()),
                    None => format!("{PROBE} witness: no mark yet"),
                },
            )
            .await;
            let record: tokumai_enclave::witness::Record = std::sync::Arc::new(move |mark| {
                // From the book's writer thread: a runtime of its own for the two calls.
                let bucket = bucket.clone();
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
                runtime.block_on(async move {
                    let creds = tokumai_egress::ask_host(&Endpoint::Vsock(HOST_CID, HOST_SERVICE_PORT), "credentials").await?;
                    let creds: tokumai_enclave::kms::Credentials = serde_json::from_slice(&creds).map_err(|e| format!("the host's credentials are unreadable: {e}"))?;
                    tokumai_enclave::witness::record(&bucket, &creds, mark, tokumai_proto::now_ms()).await
                })
            });
            Some(tokumai_enclave::witness::Setup { seen, record })
        }
        (Some(_), None) => {
            let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} witness: none in this image — a rewound book would not be noticed")).await;
            None
        }
        _ => None,
    };
    let enclave = Enclave::start(Platform {
        attester: Box::new(attester),
        keys,
        providers,
        // Sealed secrets mean a real enclave: the book is kept on the host, sealed under
        // the data key. Without them (a probe on the mock model) it stays in memory.
        db: if sealed.is_some() { Db::Kept(std::sync::Arc::new(HostBook::open())) } else { Db::Memory },
        pricing: PricingTable::parse(PRICING_JSON).expect("pricing.json"),
        dev_mode,
        stripe,
        apple_api,
        witness,
        // The operator's account, named in the image (`tokumai_enclave::admin`).
        admin: std::env::var("TOKUMAI_ADMIN_ACCOUNT").ok().filter(|a| !a.is_empty()),
    })
    .expect("start the enclave");
    // What the book came back with, over vsock: a production enclave has no console, and
    // "the book is empty" must not be indistinguishable from "the book was not found".
    let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} book: {} change(s) replayed", enclave.replayed())).await;
    let enclave: &'static Enclave = Box::leak(Box::new(enclave));
    tokio::spawn(async move {
        loop {
            enclave.tick().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // Its own front doors. One Nym client per gateway (TOKUMAI_GATEWAYS, part of the
    // image and therefore measured), each with its own sealed identity, all answering out
    // of this one enclave and its one book. Three doors mean the enclave is still
    // reachable when a gateway is down — which with one door it is not.
    //
    // Each client's identity is laid out in its own directory at every start, so every
    // address survives a restart; without a sealed identity a door comes up under a new
    // address, which an app pinned to the old one will not find.
    let gateways: Vec<String> = std::env::var("TOKUMAI_GATEWAYS")
        .or_else(|_| std::env::var("TOKUMAI_GATEWAY"))
        .unwrap_or_default()
        .split(',')
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    if gateways.is_empty() {
        panic!("no gateway in the image — TOKUMAI_GATEWAYS");
    }
    // The doors open side by side, and each one serves the moment it is open: a gateway
    // that takes its five minutes of retries no longer keeps the other two shut (until
    // 2026-10-02 they were opened one after the other, and nothing served before the last).
    let mut opening = tokio::task::JoinSet::new();
    for gateway in gateways.clone() {
        let nym = PathBuf::from("/tmp/nym").join(&gateway);
        if let Some(files) = sealed.as_ref().and_then(|s| s.nym_identity_for(&gateway)) {
            if let Err(e) = write_identity(&nym, files) {
                eprintln!("tokumai enclave: could not lay out the sealed identity for {gateway}: {e}");
            }
        }
        opening.spawn(async move {
            let opened = tokumai_server::mix::connect_at_boot(&nym, Some(&gateway)).await;
            (gateway, nym, opened)
        });
    }
    let mut open = 0usize;
    while let Some(joined) = opening.join_next().await {
        let Ok((gateway, nym, opened)) = joined else { continue };
        match opened {
            Ok(client) => {
                let address = client.nym_address().to_string();
                println!("tokumai enclave ({PROBE}) on the mixnet at {address}");
                let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} nym-address {address}")).await;
                tokio::spawn(Box::pin(tokumai_server::mix::serve(enclave, client, nym)));
                open += 1;
                if open == 1 {
                    let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} serving on its first door")).await;
                }
            }
            // One door that will not open is not a reason to keep the others shut.
            Err(e) => eprintln!("tokumai enclave: the door at {gateway} stayed shut: {e}"),
        }
    }
    if open == 0 {
        panic!("not one of the enclave's doors opened");
    }
    let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} serving on {open} of {} doors", gateways.len())).await;
    // Redemption-shaped traffic for the thin hours (tokumai_enclave::ghost).
    tokio::spawn(tokumai_server::mix::ghosts(enclave));
    // The doors' loops hold the process; this task has nothing left to do.
    std::future::pending::<()>().await;
}
