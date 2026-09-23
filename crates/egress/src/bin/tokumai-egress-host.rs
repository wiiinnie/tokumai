//! The host side of the enclave's way out: a CONNECT proxy for the allowed destinations.
//!
//!     tokumai-egress-host <listen> <allowlist file> [announcements] [host service] [sealed file] [book dir]
//!     tokumai-egress-host vsock:4294967295:8080 /etc/tokumai/egress.allow vsock:4294967295:8081   (on the EC2 host)
//!     tokumai-egress-host tcp:127.0.0.1:8080 deploy/egress.allow            (on a laptop)
//!
//! **What it writes down.** By default: nothing per tunnel. Once an hour, a line per
//! destination with how many calls and how many bytes — no timestamps, no order, nothing
//! that pairs one call with one moment. That is deliberate. We hold the payment records,
//! so a log of when each provider call went out is the other half of a join between a
//! named customer and a question; a file we do not keep cannot be asked for. An hour is
//! coarse enough that the pairing does not survive it, and precise enough to see that the
//! machine is working.
//!
//! `TOKUMAI_EGRESS_LOG=lines` brings back a line per tunnel, for development and for
//! chasing a fault. It is never the default, and the probe sets it explicitly.
//!
//! Refusals are always logged: a destination that is not on the allowlist is a
//! misconfiguration or somebody trying, and it is not a call anyone made.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokumai_egress::{serve, Allowlist, Endpoint, Report};

/// What the proxy writes about the traffic it carries.
struct Tally {
    /// destination host → (calls, bytes out, bytes back)
    per_host: Mutex<BTreeMap<String, (u64, u64, u64)>>,
}

impl Tally {
    /// One line per destination, then start again. Called every hour, and once more when
    /// the process is asked to stop.
    fn say_and_clear(&self) {
        let Ok(mut per_host) = self.per_host.lock() else { return };
        if per_host.is_empty() {
            println!("egress: nothing in the last hour");
        }
        for (host, (calls, up, down)) in per_host.iter() {
            println!("egress {host}: {calls} call(s), up {up}, down {down} (last hour)");
        }
        per_host.clear();
    }
}

/// The instance role's temporary credentials, from the metadata service (IMDSv2).
async fn instance_credentials() -> Result<String, String> {
    const IMDS: &str = "http://169.254.169.254/latest";
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().map_err(|e| e.to_string())?;
    let token = http
        .put(format!("{IMDS}/api/token"))
        .header("x-aws-ec2-metadata-token-ttl-seconds", "60")
        .send()
        .await
        .map_err(|e| format!("no metadata token: {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let get = |path: String| {
        let (http, token) = (http.clone(), token.clone());
        async move { http.get(path).header("x-aws-ec2-metadata-token", token).send().await.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string()) }
    };
    let role = get(format!("{IMDS}/meta-data/iam/security-credentials/")).await?;
    let role = role.lines().next().unwrap_or("").trim().to_string();
    if role.is_empty() {
        return Err("this instance has no role — start it with tokumai-enclave-host".into());
    }
    get(format!("{IMDS}/meta-data/iam/security-credentials/{role}")).await
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(listen), Some(file)) = (args.first(), args.get(1)) else {
        eprintln!("usage: tokumai-egress-host <tcp:host:port | vsock:cid:port> <allowlist file>");
        std::process::exit(2);
    };
    let listen = Endpoint::parse(listen).unwrap_or_else(|e| panic!("{e}"));
    let allow = Allowlist::parse(&std::fs::read_to_string(file).expect("read the allowlist")).unwrap_or_else(|e| panic!("{e}"));
    // Per tunnel, or per hour: see the note at the top of this file.
    let lines = std::env::var("TOKUMAI_EGRESS_LOG").map(|v| v == "lines").unwrap_or(false);
    let report: Report = if lines {
        println!("tokumai-egress-host: writing a line per tunnel (TOKUMAI_EGRESS_LOG=lines) — development only");
        Arc::new(|host, port, r| match r {
            Ok((0, 0)) => println!("egress {host}:{port} open"),
            Ok((up, down)) => println!("egress {host}:{port} closed, up {up} down {down}"),
            Err(e) => println!("egress {host}:{port} refused: {e}"),
        })
    } else {
        let tally: Arc<Tally> = Arc::new(Tally { per_host: Mutex::new(BTreeMap::new()) });
        let hourly = tally.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                hourly.say_and_clear();
            }
        });
        Arc::new(move |host: &str, port: u16, r: Result<(u64, u64), String>| match r {
            // A tunnel opening is not counted: the same call is counted once, when it closes.
            Ok((0, 0)) => {}
            Ok((up, down)) => {
                if let Ok(mut per_host) = tally.per_host.lock() {
                    let row = per_host.entry(host.to_string()).or_insert((0, 0, 0));
                    row.0 += 1;
                    row.1 += up;
                    row.2 += down;
                }
            }
            Err(e) => println!("egress {host}:{port} refused: {e}"),
        })
    };
    // What the enclave says about itself (its Nym address), on a port of its own.
    if let Some(a) = args.get(2) {
        let a = Endpoint::parse(a).unwrap_or_else(|e| panic!("{e}"));
        println!("tokumai-egress-host hears the enclave on {a:?}");
        tokio::spawn(async move { tokumai_egress::hear(a).await.expect("announcements") });
    }
    // The enclave's own questions: its instance credentials and the sealed secrets. The
    // credentials are fetched here, from the instance metadata service the enclave cannot
    // reach; they open nothing by themselves, since the key's policy wants an attestation.
    if let (Some(svc), Some(sealed)) = (args.get(3), args.get(4)) {
        let svc = Endpoint::parse(svc).unwrap_or_else(|e| panic!("{e}"));
        let sealed = std::path::PathBuf::from(sealed);
        // Where the enclave's book is kept, sealed — this side stores bytes it cannot read.
        let book = std::path::PathBuf::from(args.get(5).map(String::as_str).unwrap_or("book"));
        println!("tokumai-egress-host answers the enclave on {svc:?} (sealed secrets: {}, book: {})", sealed.display(), book.display());
        tokio::spawn(async move { tokumai_egress::serve_host(svc, sealed, book, instance_credentials).await.expect("host service") });
    }
    println!("tokumai-egress-host on {listen:?}");
    serve(listen, allow, report).await.expect("egress proxy");
}
