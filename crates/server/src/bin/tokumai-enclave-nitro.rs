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
//! PROBE 1 (phase 0): the data key is drawn at random and the ledger lives in memory, so a
//! restart forgets everything; no provider keys reach it yet, so it offers the mock model
//! and test credit (dev mode). Probe 2 brings the data key and the secrets from KMS, released
//! only to this image's PCR0. The image says which it is: `PROBE` below is part of what is
//! measured.

use std::path::PathBuf;
use tokumai_attest::nitro::NitroAttester;
use tokumai_core::pricing::PricingTable;
use tokumai_egress::Endpoint;
use tokumai_enclave::policy::PRICING_JSON;
use tokumai_enclave::provider::Providers;
use tokumai_enclave::seal::FixedKeyProvider;
use tokumai_enclave::service::{Db, Enclave, Platform};

const PROBE: &str = "probe-1";
/// The parent instance, as an enclave sees it.
const HOST_CID: u32 = 3;
const EGRESS_PORT: u32 = 8080;
const ANNOUNCE_PORT: u32 = 8081;
const LOOPBACK_PROXY: &str = "127.0.0.1:1080";

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
    let enclave = Enclave::start(Platform {
        attester: Box::new(attester),
        keys: Box::new(FixedKeyProvider(rand::random())),
        providers: Providers::mock(),
        db: Db::Memory,
        pricing: PricingTable::parse(PRICING_JSON).expect("pricing.json"),
        dev_mode: true,
        stripe: None,
        apple_api: None,
    })
    .expect("start the enclave");
    let enclave: &'static Enclave = Box::leak(Box::new(enclave));
    tokio::spawn(async move {
        loop {
            enclave.tick().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // Its own Nym identity, in the enclave's memory: a new address on every start. (Probe 2:
    // the identity sealed with the data key, so the address stays.)
    let nym = PathBuf::from("/tmp/nym");
    let gateway = std::env::var("TOKUMAI_GATEWAY").ok().filter(|g| !g.trim().is_empty());
    let client = tokumai_server::mix::connect_at_boot(&nym, gateway.as_deref()).await.expect("connect to the mixnet");
    let address = client.nym_address().to_string();
    println!("tokumai enclave ({PROBE}) on the mixnet at {address}");
    for _ in 0..30 {
        match tokumai_egress::announce(&Endpoint::Vsock(HOST_CID, ANNOUNCE_PORT), &format!("{PROBE} nym-address {address}")).await {
            Ok(()) => break,
            Err(_) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
        }
    }
    tokumai_server::mix::serve(enclave, client, nym).await;
}
