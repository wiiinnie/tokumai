// http.rs — the one place outbound HTTP clients are built.
//
// Every provider call is pinned to IPv4: Google geo-blocks the VPS's OVH IPv6
// range ("User location is not supported") while the IPv4 route is accepted,
// and which family the resolver picks must not depend on the HTTP stack's
// mood (Node preferred v4, reqwest happily took v6 — that's how the Gemini
// catalog silently vanished after the Rust port).

use std::net::{IpAddr, Ipv4Addr};

/// A reqwest client that only binds IPv4 for outbound connections.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .build()
        .expect("reqwest client build cannot fail with these options")
}
