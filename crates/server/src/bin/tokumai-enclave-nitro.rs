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
//! is what the first probe did. The ledger lives in memory either way for now; giving it a
//! home on the host, sealed, is the next step.

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

/// The book, kept on the host and sealed (`enclave::state`). The ledger is ordinary
/// blocking code, so one thread of its own holds the channel to the host and every call
/// waits for the host to say the change is on its disk.
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
        answer.recv().map_err(|_| "the host never answered about the book".to_string())?
    }
}

impl tokumai_enclave::state::Store for HostBook {
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        self.call("snapshot", &[])
    }
    fn put_snapshot(&self, sealed: &[u8]) -> Result<(), String> {
        self.call("put-snapshot", sealed).map(|_| ())
    }
    fn journal(&self) -> Result<Vec<Vec<u8>>, String> {
        tokumai_enclave::state::split(&self.call("journal", &[])?)
    }
    fn append(&self, record: &[u8]) -> Result<(), String> {
        self.call("add-record", record).map(|_| ())
    }
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
    announce_panics();
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
    let mut doors = Vec::new();
    for gateway in &gateways {
        let nym = PathBuf::from("/tmp/nym").join(gateway);
        if let Some(files) = sealed.as_ref().and_then(|s| s.nym_identity_for(gateway)) {
            if let Err(e) = write_identity(&nym, files) {
                eprintln!("tokumai enclave: could not lay out the sealed identity for {gateway}: {e}");
            }
        }
        match tokumai_server::mix::connect_at_boot(&nym, Some(gateway)).await {
            Ok(client) => {
                let address = client.nym_address().to_string();
                println!("tokumai enclave ({PROBE}) on the mixnet at {address}");
                let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} nym-address {address}")).await;
                doors.push((client, nym));
            }
            // One door that will not open is not a reason to keep the others shut.
            Err(e) => eprintln!("tokumai enclave: the door at {gateway} stayed shut: {e}"),
        }
    }
    if doors.is_empty() {
        panic!("not one of the enclave's doors opened");
    }
    // Every door gets its own loop; the last one holds the process open.
    let last = doors.pop().expect("checked");
    for (client, nym) in doors {
        tokio::spawn(Box::pin(tokumai_server::mix::serve(enclave, client, nym)));
    }
    let (client, nym) = last;
    let _ = tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} serving on its doors")).await;
    tokumai_server::mix::serve(enclave, client, nym).await;
}
