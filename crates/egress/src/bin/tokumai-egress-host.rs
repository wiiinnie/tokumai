//! The host side of the enclave's way out: a CONNECT proxy for the allowed destinations.
//!
//!     tokumai-egress-host <listen> <allowlist file> [announcements]
//!     tokumai-egress-host vsock:4294967295:8080 /etc/tokumai/egress.allow vsock:4294967295:8081   (on the EC2 host)
//!     tokumai-egress-host tcp:127.0.0.1:8080 deploy/egress.allow            (on a laptop)
//!
//! One log line per tunnel: destination, bytes each way, or why it was refused.

use std::sync::Arc;
use tokumai_egress::{serve, Allowlist, Endpoint, Report};

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
    println!("tokumai-egress-host on {listen:?}");
    serve(listen, allow, report).await.expect("egress proxy");
}
