//! The one outbound HTTP client. IPv4 only: Google refuses some hosting ranges over IPv6
//! ("User location is not supported") and accepts the same machine over IPv4. A default
//! timeout keeps one hung provider from holding a request forever; slower calls (reasoning,
//! pictures) set their own.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

pub fn client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(120))
                .build()
                .expect("the HTTP client builds with these options")
        })
        .clone()
}
