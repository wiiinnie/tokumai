//! The host side of the enclave's way out: a CONNECT proxy for the allowed destinations.
//!
//!     tokumai-egress-host <listen> <allowlist file> [announcements] [host service] [sealed file] [book dir]
//!     tokumai-egress-host vsock:4294967295:8080 /etc/tokumai/egress.allow vsock:4294967295:8081   (on the EC2 host)
//!     tokumai-egress-host tcp:127.0.0.1:8080 deploy/egress.allow            (on a laptop)
//!
//! One log line per tunnel: destination, bytes each way, or why it was refused.

use std::sync::Arc;
use tokumai_egress::{serve, Allowlist, Endpoint, Report};

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
    let report: Report = Arc::new(|host, port, r| match r {
        Ok((0, 0)) => println!("egress {host}:{port} open"),
        Ok((up, down)) => println!("egress {host}:{port} closed, up {up} down {down}"),
        Err(e) => println!("egress {host}:{port} refused: {e}"),
    });
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
