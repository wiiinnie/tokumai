//! scrai-server as a library: every request handler the mixnet loop dispatches to, so
//! the binary (`main.rs`), the admin TUI and the fuzz targets (`fuzz/`) share one code
//! path. Nothing here opens a socket — the binary owns the Nym client.

pub mod catalog;
pub mod chat;
pub mod faucet;
pub mod http;
pub mod inflight;
pub mod nyx;
pub mod pay;
pub mod replies;
pub mod store;
pub mod uploads;

/// Resolve a network-scoped config value: `{base}_MAINNET` or `{base}_TESTNET`
/// (whichever is set and non-empty), falling back to the legacy plain `{base}`. The
/// scrai-admin network toggle keeps exactly one suffix uncommented in the .env, so at
/// most one is ever present. Mirrors the two Gemini key slots (see chat::gemini_api_key).
/// Release gate: `SCRAI_MIN_APP=0.3.0` makes the server refuse every request from an app
/// older than that (or one that sends no `app` version at all — 0.2.x never did), pointing
/// at `SCRAI_UPDATE_URL` (fallback: the faucet/download site). Unset → no gate.
pub fn min_app() -> Option<(u64, u64, u64)> {
    std::env::var("SCRAI_MIN_APP").ok().and_then(|v| parse_ver(&v))
}

pub fn update_url() -> String {
    std::env::var("SCRAI_UPDATE_URL")
        .ok()
        .filter(|u| u.starts_with("https://"))
        .or_else(pay::faucet_url)
        .unwrap_or_else(|| "https://scrai-faucet.hermes-stakepool.de/".into())
}

/// "0.3.0", "0.3.0 (peroni)", "v0.3.0-beta" → (0, 3, 0). Anything without three numbers → None.
pub fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// The gate's verdict for one request: `Some((min_as_text, url))` when the client must
/// update — its `app` field is older than SCRAI_MIN_APP, or missing while a gate is set.
pub fn app_outdated(req: &serde_json::Value) -> Option<(String, String)> {
    let min = min_app()?;
    let app = req.get("app").and_then(|a| a.as_str()).and_then(parse_ver);
    match app {
        Some(v) if v >= min => None,
        _ => Some((format!("{}.{}.{}", min.0, min.1, min.2), update_url())),
    }
}

pub fn net_var(base: &str) -> Option<String> {
    let get = |name: String| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    get(format!("{base}_MAINNET"))
        .or_else(|| get(format!("{base}_TESTNET")))
        .or_else(|| get(base.to_string()))
}

#[cfg(test)]
mod release_gate_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_parse_and_compare() {
        assert_eq!(parse_ver("0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_ver("v0.3.0-beta"), Some((0, 3, 0)));
        assert_eq!(parse_ver("0.2.3 (kash)"), Some((0, 2, 3)));
        assert_eq!(parse_ver("0.2"), None);
        assert!(parse_ver("0.10.0").unwrap() > parse_ver("0.9.9").unwrap());
    }

    #[test]
    fn gate_refuses_old_and_missing_only_when_set() {
        // env is process-global; this is the only test touching SCRAI_MIN_APP
        std::env::remove_var("SCRAI_MIN_APP");
        assert!(app_outdated(&json!({"kind":"chat"})).is_none());
        std::env::set_var("SCRAI_MIN_APP", "0.3.0");
        assert!(app_outdated(&json!({"kind":"chat"})).is_some(), "0.2.x sends no app field");
        assert!(app_outdated(&json!({"kind":"chat","app":"0.2.3"})).is_some());
        assert!(app_outdated(&json!({"kind":"chat","app":"0.3.0"})).is_none());
        assert!(app_outdated(&json!({"kind":"chat","app":"0.3.1"})).is_none());
        let (min, url) = app_outdated(&json!({"kind":"chat","app":"0.1.0"})).unwrap();
        assert_eq!(min, "0.3.0");
        assert!(url.starts_with("https://"));
        std::env::remove_var("SCRAI_MIN_APP");
    }
}
