//! scrai-server as a library: every request handler the mixnet loop dispatches to, so
//! the binary (`main.rs`), the admin TUI and the fuzz targets (`fuzz/`) share one code
//! path. Nothing here opens a socket — the binary owns the Nym client.

pub mod admin;
pub mod catalog;
pub mod chat;
pub mod faucet;
pub mod http;
pub mod inflight;
pub mod nyx;
pub mod openai;
pub mod pay;
pub mod replies;
pub mod store;
pub mod uploads;

/// The server's version — the shared workspace version (root Cargo.toml), so it always
/// matches the app release it was cut with. Printed at boot, sent as `serverVersion` on
/// the models reply (the app shows it under Settings) and in scrai-admin's header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolve a network-scoped config value: `{base}_MAINNET` or `{base}_TESTNET`
/// (whichever is set and non-empty), falling back to the legacy plain `{base}`. The
/// scrai-admin network toggle keeps exactly one suffix uncommented in the .env, so at
/// most one is ever present. Mirrors the two Gemini key slots (see chat::gemini_api_key).
/// Release gate: `MIN_APP=0.3.0` makes the server refuse every request from an app
/// older than that (or one that sends no `app` version at all — 0.2.x never did), pointing
/// at `UPDATE_URL` (fallback: the faucet/download site). Unset → no gate.
pub fn min_app() -> Option<(u64, u64, u64)> {
    cfg("MIN_APP").ok().and_then(|v| parse_ver(&v))
}

pub fn update_url() -> String {
    cfg("UPDATE_URL")
        .ok()
        .filter(|u| u.starts_with("https://"))
        .or_else(pay::faucet_url)
        .unwrap_or_else(|| "https://tokumai.com/".into())
}

/// "0.3.0", "0.3.0 (peroni)", "v0.3.0-beta" → (0, 3, 0). Anything without three numbers → None.
pub fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let core: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// The gate's verdict for one request: `Some((min_as_text, url))` when the client must
/// update — its `app` field is older than MIN_APP, or missing while a gate is set.
pub fn app_outdated(req: &serde_json::Value) -> Option<(String, String)> {
    let min = min_app()?;
    let app = req.get("app").and_then(|a| a.as_str()).and_then(parse_ver);
    match app {
        Some(v) if v >= min => None,
        _ => Some((format!("{}.{}.{}", min.0, min.1, min.2), update_url())),
    }
}

/// Read a config value by its NEW, prefix-free name, falling back to the old `SCRAI_`
/// one. The prefix is being dropped as part of the tokumai rename (ALLOW_SINGLE_AUTHORITY,
/// not SCRAI_ALLOW_SINGLE_AUTHORITY).
///
/// The fallback exists so the rename can ship WITHOUT touching /opt/tokumai/.env in the
/// same breath. Several of these are fail-closed — a missing ALLOW_SINGLE_AUTHORITY stops
/// the server dead — so a deploy that renamed the code but not the file would take the box
/// down. Migrate the .env at leisure, then delete this function and the `SCRAI_` half.
///
/// Returns `Result` on purpose, so every existing call site (`.ok()`, `.is_ok()`,
/// `.as_deref() == Ok("1")`, `.unwrap_or_else(|_| …)`) keeps its exact meaning. An EMPTY
/// new-style value falls through to the old name rather than masking it.
pub fn cfg(name: &str) -> Result<String, std::env::VarError> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => std::env::var(format!("SCRAI_{name}")),
    }
}

/// Where this binary is installed: `/opt/tokumai/bin/<exe>` → `/opt/tokumai`.
///
/// The CLIs used to resolve DATA and the .env against the CURRENT DIRECTORY, so running
/// one the obvious way — `sudo -u scrai /opt/tokumai/bin/tokumai-faucet code new` from a
/// home directory — died with "./data/faucet.db: unable to open database file", and
/// scrai-admin read no .env at all. The install root is derived from the executable, so
/// the tools work from anywhere. Returns None for a binary that is not in a `bin/` dir
/// (cargo run, tests) — the caller then keeps the relative default.
pub fn install_root() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin = exe.parent()?;
    (bin.file_name()? == "bin").then(|| bin.parent().map(|r| r.to_path_buf()))?
}

/// The state directory: `DATA` if set, else `./data` when it exists (a dev checkout),
/// else `<install root>/data`.
pub fn data_dir() -> std::path::PathBuf {
    if let Ok(d) = cfg("DATA") {
        return std::path::PathBuf::from(d);
    }
    let here = std::path::PathBuf::from("./data");
    if here.is_dir() {
        return here;
    }
    install_root().map(|r| r.join("data")).unwrap_or(here)
}

/// The .env the services actually run with: `ENV_FILE` if set, else `<install root>/.env`,
/// else `./.env` (dev checkout).
pub fn env_file() -> std::path::PathBuf {
    if let Ok(f) = cfg("ENV_FILE") {
        return std::path::PathBuf::from(f);
    }
    install_root().map(|r| r.join(".env")).unwrap_or_else(|| std::path::PathBuf::from("./.env"))
}

/// Money rails whose configuration is network-scoped (`{base}_MAINNET` / `{base}_TESTNET`).
/// Getting one of these wrong does not fail loudly — it settles invoices against the wrong
/// world — so they are checked at boot (`testnet_rails_on_mainnet`).
pub const MONEY_RAILS: [&str; 7] = [
    "NYX_LCD_URL",
    "NYX_RECEIVE_ADDRESS",
    "BTCPAY_URL",
    "BTCPAY_STORE_ID",
    "BTCPAY_API_KEY",
    "MOLLIE_API_KEY",
    // The faucet wallet pin. A mainnet server still pinned to the sandbox faucet refuses
    // every invite credit (fail closed) — visible only as testers whose $1 never lands.
    "FAUCET_ADDRESS",
];

/// Rails that would silently run on TEST infrastructure on a real-money server.
///
/// `net_var` resolves `_MAINNET` → `_TESTNET` → bare, and that fallback ignores whether the
/// server is actually in testnet mode. With only `_TESTNET` values in .env — the normal
/// state of a testnet box — flipping TESTNET to 0 keeps every rail pointed at the
/// test world while the server starts accepting real money. The worst of them is Mollie:
/// its test checkout lets the payer pick "paid" for free, so anyone could mint credit and
/// spend it on provider calls we pay for. Refuse to boot instead.
pub fn testnet_rails_on_mainnet() -> Vec<&'static str> {
    rails_on_test_infra(|n| std::env::var(n).ok())
}

/// The decision itself, with the lookup passed in — pure, so its test does not have to
/// write the very env names that the pay.rs tests read (that shared-state trap has bitten
/// this crate twice now).
fn rails_on_test_infra(get: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    let set = |n: String| get(&n).is_some_and(|v| !v.trim().is_empty());
    MONEY_RAILS
        .iter()
        .copied()
        .filter(|b| !set(format!("{b}_MAINNET")) && set(format!("{b}_TESTNET")))
        .collect()
}

pub fn net_var(base: &str) -> Option<String> {
    let get = |name: String| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    get(format!("{base}_MAINNET"))
        .or_else(|| get(format!("{base}_TESTNET")))
        .or_else(|| get(base.to_string()))
}

#[cfg(test)]
mod cfg_tests {
    use super::*;

    /// The rename ships before the .env is migrated, so BOTH names must resolve — and a
    /// fail-closed value like ALLOW_SINGLE_AUTHORITY must never read as absent just because
    /// the box still carries the old spelling. Uses names nothing else in the crate touches.
    #[test]
    fn the_new_name_wins_and_the_old_one_still_works() {
        std::env::remove_var("CFG_PROBE");
        std::env::remove_var("SCRAI_CFG_PROBE");
        assert!(cfg("CFG_PROBE").is_err(), "neither set → absent");

        // an un-migrated .env: only the old spelling
        std::env::set_var("SCRAI_CFG_PROBE", "old");
        assert_eq!(cfg("CFG_PROBE").as_deref(), Ok("old"));

        // migrated: the new name wins
        std::env::set_var("CFG_PROBE", "new");
        assert_eq!(cfg("CFG_PROBE").as_deref(), Ok("new"));

        // an EMPTY new value must not mask the old one (a commented-out line left as `X=`)
        std::env::set_var("CFG_PROBE", "   ");
        assert_eq!(cfg("CFG_PROBE").as_deref(), Ok("old"));

        std::env::remove_var("CFG_PROBE");
        std::env::remove_var("SCRAI_CFG_PROBE");
    }
}

#[cfg(test)]
mod rail_guard_tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    /// The scenario this exists for: a testnet box whose .env only has _TESTNET rails, and
    /// someone flips TESTNET to 0. Mollie would then run on its `test_` key, whose
    /// checkout lets the payer choose "paid" for free — real credit, no money moved.
    #[test]
    fn a_mainnet_server_refuses_rails_that_only_have_a_testnet_value() {
        assert!(rails_on_test_infra(env(&[])).is_empty(), "nothing configured → nothing to flag");

        // exactly the shape of the live .env on 2026-09-05
        let live = env(&[
            ("MOLLIE_API_KEY_TESTNET", "test_abc"),
            ("NYX_LCD_URL_TESTNET", "https://validator-sandbox-1.nymtech.net/api"),
            ("BTCPAY_URL_TESTNET", "https://testnet.demo.btcpayserver.org"),
        ]);
        let stale = rails_on_test_infra(live);
        assert!(stale.contains(&"MOLLIE_API_KEY"), "the free-credit one must be caught: {stale:?}");
        assert!(stale.contains(&"NYX_LCD_URL") && stale.contains(&"BTCPAY_URL"));

        // a half-migrated .env still trips on what is left
        let half = env(&[
            ("MOLLIE_API_KEY_TESTNET", "test_abc"),
            ("MOLLIE_API_KEY_MAINNET", "live_abc"),
            ("NYX_LCD_URL_TESTNET", "https://validator-sandbox-1.nymtech.net/api"),
        ]);
        assert_eq!(rails_on_test_infra(half), vec!["NYX_LCD_URL"]);

        // an empty value is not a value
        let blank = env(&[("MOLLIE_API_KEY_TESTNET", "test_abc"), ("MOLLIE_API_KEY_MAINNET", "   ")]);
        assert_eq!(rails_on_test_infra(blank), vec!["MOLLIE_API_KEY"]);

        // a rail configured only for mainnet, or not at all, is fine
        assert!(rails_on_test_infra(env(&[("MOLLIE_API_KEY_MAINNET", "live_abc")])).is_empty());
    }
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
        // env is process-global; this is the only test touching MIN_APP
        std::env::remove_var("MIN_APP");
        assert!(app_outdated(&json!({"kind":"chat"})).is_none());
        std::env::set_var("MIN_APP", "0.3.0");
        assert!(app_outdated(&json!({"kind":"chat"})).is_some(), "0.2.x sends no app field");
        assert!(app_outdated(&json!({"kind":"chat","app":"0.2.3"})).is_some());
        assert!(app_outdated(&json!({"kind":"chat","app":"0.3.0"})).is_none());
        assert!(app_outdated(&json!({"kind":"chat","app":"0.3.1"})).is_none());
        let (min, url) = app_outdated(&json!({"kind":"chat","app":"0.1.0"})).unwrap();
        assert_eq!(min, "0.3.0");
        assert!(url.starts_with("https://"));
        std::env::remove_var("MIN_APP");
    }
}
