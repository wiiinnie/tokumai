//! scrai-server as a library: every request handler the mixnet loop dispatches to, so
//! the binary (`main.rs`), the admin TUI and the fuzz targets (`fuzz/`) share one code
//! path. Nothing here opens a socket — the binary owns the Nym client.

pub mod catalog;
pub mod chat;
pub mod http;
pub mod nyx;
pub mod pay;
pub mod replies;
pub mod store;
pub mod uploads;

/// Resolve a network-scoped config value: `{base}_MAINNET` or `{base}_TESTNET`
/// (whichever is set and non-empty), falling back to the legacy plain `{base}`. The
/// scrai-admin network toggle keeps exactly one suffix uncommented in the .env, so at
/// most one is ever present. Mirrors the two Gemini key slots (see chat::gemini_api_key).
pub fn net_var(base: &str) -> Option<String> {
    let get = |name: String| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    get(format!("{base}_MAINNET"))
        .or_else(|| get(format!("{base}_TESTNET")))
        .or_else(|| get(base.to_string()))
}
