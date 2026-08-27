// http.rs — the one place outbound HTTP clients are built.
//
// Every provider call is pinned to IPv4: Google geo-blocks the VPS's OVH IPv6
// range ("User location is not supported") while the IPv4 route is accepted,
// and which family the resolver picks must not depend on the HTTP stack's
// mood (Node preferred v4, reqwest happily took v6 — that's how the Gemini
// catalog silently vanished after the Rust port).

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// A reqwest client that only binds IPv4 for outbound connections.
///
/// Every request carries a DEFAULT timeout so one hung upstream can't wedge the
/// (serial) mixnet message loop forever (H8) — a stalled provider call now fails
/// instead of blocking every other client. Calls that legitimately run longer
/// (image generation) set a larger per-request `.timeout(...)`, which overrides
/// this default. `connect_timeout` fails fast on an unreachable host.
pub fn client() -> reqwest::Client {
    // M9: build ONCE and hand out clones — a reqwest::Client owns the connection pool +
    // TLS session cache, and clones share them, so rebuilding per request threw both away.
    // Per-request `.timeout(...)` overrides still work on a clone.
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client build cannot fail with these options")
        })
        .clone()
}
