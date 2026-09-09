// ---------------------------------------------------------------------------
// admin.rs — everything the operator's console KNOWS, with no opinion on how it is shown.
//
// This lived inside the ratatui binary until the web console arrived. Two front ends over
// one set of numbers is fine; two copies of "what does the pay snapshot mean" is not — the
// figures decide refunds and go into bookkeeping, and a drift between them would be
// invisible until it mattered. So the data layer is here and the rendering is not.
//
// Read-only against `state.db` (its own connection, opened read-only) except for the three
// deliberate writes the operator can make: toggle the network in .env, mint an invite code,
// void an unredeemed voucher.
// ---------------------------------------------------------------------------

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

pub const TOKU_PER_USD: u64 = 100_000; // one coconut coin = $1 = 100_000 TOKU

// ---- blob shapes (subset; serde ignores the fields we don't name) ----------
#[derive(Deserialize, Default)]
pub struct SessBlob {
    #[serde(default)]
    pub sessions: HashMap<String, Sess>,
}

#[derive(Deserialize, Default, Clone, Copy)]
pub struct Sess {
    #[serde(default)]
    pub balance: u64,
    #[serde(default)]
    pub counter: u64,
}

#[derive(Deserialize, Default)]
pub struct PayBlob {
    #[serde(default)]
    pub invoices: HashMap<String, Inv>,
    #[serde(default)]
    pub entitlements: HashMap<String, u64>,
}

#[derive(Deserialize, Default)]
pub struct Inv {
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub amount_usd: u32,
    #[serde(default)]
    pub amount_toku: u64,
    #[serde(default)]
    pub status: String,
    /// raised as a $1 faucet-paid testnet purchase (TESTNET servers)
    #[serde(default)]
    pub testnet: bool,
    /// rail that served it: "btc" | "nyx" | "card" (Mollie) — absent on pre-card records
    #[serde(default)]
    pub method: String,
    /// ISO-3166 country the payment rail reported. Empty for coin transfers (a chain has
    /// no country) and for everything raised before 2026-09-07.
    #[serde(default)]
    pub country: String,
    /// The rail's own id for the payment: a Mollie payment id, or the NYM memo. This is
    /// what finds the money again — in the dashboard, or on the chain.
    #[serde(default)]
    pub provider_ref: String,
    #[serde(default)]
    pub paid_at: u64,
    #[serde(default)]
    pub consent_version: String,
    /// Paid out as a CODE rather than as entitlement — a purchase made on the website,
    /// with no account behind it. The only kind we can actually invalidate.
    #[serde(default)]
    pub voucher: bool,
}

#[derive(Deserialize, Default)]
pub struct QuorumBlob {
    #[serde(default)]
    pub serials: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub offenses: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub blacklist: HashSet<String>,
}

#[derive(Default)]
pub struct DayRow {
    pub day: String,
    pub prompts: u64,
    pub spent: u64,
    /// raw provider price for the day (no margin) — the number to reconcile with the
    /// provider's own billing view (Google AI Studio buckets days in Pacific time, ours are UTC)
    pub cost: u64,
    pub purchases: u64,
    pub purchased: u64,
    /// most clients served in parallel at one instant that day (chat/catalog/payment in flight)
    pub peak_clients: u64,
    /// most DIFFERENT clients that sent anything within one 60-second window that day
    /// Most DIFFERENT paying sessions inside one hour that day — the same population
    /// `users` counts, so it can never exceed it. (The old `peak_1m` counted SURB reply
    /// tags: one app has several, and every poll carried one, so it read higher than the
    /// day's users and meant nothing. Dropped 2026-09-07.)
    pub peak_1h: u64,
    /// DIFFERENT paying sessions that chatted that day (`daily_users`; 0 before it existed).
    /// One person = one session unless they bump the session index; free-tier chats
    /// carry no session and are not counted.
    pub users: u64,
    /// per model id that day (from `daily_model`; empty for days before it existed)
    pub per_model: std::collections::HashMap<String, ModelDay>,
    /// faucet payments that day (from faucet.db next to state.db; UTC days)
    pub faucet: u64,
}

/// One model's share of a day: prompts, what users paid, what the provider charged us.
#[derive(Default, Clone, Copy)]
pub struct ModelDay {
    pub prompts: u64,
    pub spent: u64,
    pub cost: u64,
}

/// Which invoice a model lands on. Google is reconciled in € (AI Studio, Pacific-day
/// buckets), OpenAI in $ (usage dashboard, UTC days) — hence the separate blocks.
pub fn provider_of(model: &str) -> &'static str {
    if model.starts_with("gemini") || model.starts_with("imagen") || model.starts_with("veo") {
        "GOOGLE"
    } else if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
        "OPENAI"
    } else {
        "OTHER"
    }
}

/// The catalog label for a model ("Nano Banana 2 Lite", "Gemini 3.5 Flash-Lite") from
/// pricing.json; a tidied raw id when the table does not know it.
pub fn model_label(id: &str) -> String {
    static PRICING: std::sync::OnceLock<Option<scrai_core::pricing::PricingTable>> = std::sync::OnceLock::new();
    let table = PRICING.get_or_init(|| scrai_core::pricing::PricingTable::parse(include_str!("../../pricing.json")).ok());
    table
        .as_ref()
        .and_then(|t| t.label(id))
        .map(|l| l.to_string())
        .unwrap_or_else(|| id.trim_start_matches("gemini-").replace('-', " "))
}

#[derive(Default)]
pub struct Metrics {
    pub ok: bool,
    pub err: Option<String>,
    // economy (from the pay blob — current authoritative state)
    pub paying_accounts: usize,
    pub inv_paid: usize,
    pub inv_pending: usize,
    pub inv_expired: usize,
    pub purchased_toku: u64,
    pub purchased_usd: u64,
    // card rail (Mollie): separately visible because it is the one rail with chargebacks
    pub card_paid: usize,
    pub card_pending: usize,
    pub card_usd: u64,
    // Where the money came from. The one that matters is `eu_usd`: cross-border B2C sales
    // inside the EU are what a registration threshold counts — German customers do not.
    pub eu_usd: u64,
    pub de_usd: u64,
    pub row_usd: u64,
    pub unknown_usd: u64,
    pub entitlement_out: u64,
    pub withdrawn_toku: u64,
    // usage
    pub sessions: usize,
    pub session_balance: u64,
    pub charges: u64,
    pub coins_redeemed: u64,
    // integrity
    pub offenders: usize,
    pub blacklisted: usize,
    // lifetime + per-day (from the `daily` table; "since metrics enabled")
    pub total_prompts: u64,
    pub total_spent: u64,
    pub total_cost: u64,
    pub total_purchases: u64,
    pub total_purchased: u64,
    pub daily: Vec<DayRow>,
    pub has_daily: bool,
    /// highest `peak_clients` over all recorded days — the capacity signal (a MAX, not a sum)
    pub peak_clients_max: u64,
    /// model ids seen in the shown days, most-used first — one table column each
    pub models: Vec<String>,
    /// day-boundary zone the server buckets with (METRICS_TZ; "UTC" if unset/old server)
    pub metrics_tz: String,
    // live-grounding queries used this UTC month (Gemini's 5,000/mo free allowance)
    pub grounding_used: u64,
    // testnet faucet (TESTNET servers): invoices flagged testnet + faucet.db claims
    pub testnet_paid: usize,
    pub testnet_pending: usize,
    pub faucet_claims: u64,
    pub faucet_unym: u64,
    pub faucet_stuck: u64,
    /// claim rows stamped today (UTC) — the number `scrai-faucet` compares with its cap
    pub faucet_today: u64,
    /// FAUCET_DAILY_MAX from the env file (default 20, as in scrai-faucet)
    pub faucet_daily_max: u64,
    pub has_faucet: bool,
    /// invite codes (newest first) — minted here with `c`
    pub faucet_codes: Vec<crate::faucet::CodeRow>,
}

/// Gemini's monthly free Grounding allowance — mirror of chat::GROUNDING_FREE_PER_MONTH.
pub const GROUNDING_FREE_PER_MONTH: u64 = 5000;

/// Current UTC month as `YYYY-MM` (same civil_from_days math as the server's today_utc,
/// so we need no date crate) — picks the right `grounding:<month>` counter key.
/// "YYYY-MM-DD" of a unix timestamp in UTC (the faucet stamps claims in UTC).
pub fn civil_day_utc(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn month_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = era * 400 + yoe + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}")
}

// ---- network toggle: comment/uncomment the testnet↔mainnet config in the .env ------
// The ONLY thing scrai-admin writes. state.db stays strictly read-only. Each managed var
// has a `_MAINNET` and a `_TESTNET` line; exactly one is uncommented. Toggling flips them
// and the operator restarts scrai. `net_var()` in the server reads whichever is active.
/// EU member states, for the cross-border B2C figure above. Germany is deliberately NOT in
/// this list: domestic sales do not count towards the threshold that forces registration
/// in the customer's country.
pub const EU: [&str; 26] = [
    "AT", "BE", "BG", "CY", "CZ", "DK", "EE", "ES", "FI", "FR", "GR", "HR", "HU", "IE", "IT", "LT",
    "LU", "LV", "MT", "NL", "PL", "PT", "RO", "SE", "SI", "SK",
];

pub const NET_VARS: &[&str] = &[
    "GEMINI_API_KEY",
    "BTCPAY_URL",
    "BTCPAY_STORE_ID",
    "BTCPAY_API_KEY",
    "NYX_LCD_URL",
    "NYX_RECEIVE_ADDRESS",
];

pub fn env_file_path() -> String {
    // Was hardcoded to /opt/scrai/.env, which stopped existing with the move to
    // /opt/tokumai — the panel then silently showed compiled-in defaults instead of the
    // caps the services run with.
    crate::env_file().display().to_string()
}

/// `KEY=value` from the env file (uncommented lines only; quotes stripped), falling back to
/// the process environment — so the panel shows the cap the services actually run with.
pub fn env_file_value(key: &str) -> Option<String> {
    // Both spellings, newest line wins — a .env that still carries SCRAI_<key> must read
    // the same here as it does in the server (see crate::cfg).
    let legacy = format!("SCRAI_{key}");
    let from_file = std::fs::read_to_string(env_file_path()).ok().and_then(|s| {
        s.lines().rev().find_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let k = k.trim();
            (k == key || k == legacy).then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
        })
    });
    from_file.or_else(|| crate::cfg(key).ok()).filter(|v| !v.is_empty())
}

/// If `line` assigns one of the managed vars, which network slot is it (comment state ignored)?
pub fn managed_suffix(line: &str) -> Option<&'static str> {
    let body = line.trim_start().trim_start_matches('#').trim_start();
    for base in NET_VARS {
        if let Some(rest) = body.strip_prefix(base) {
            for (sfx, net) in [("_MAINNET", "mainnet"), ("_TESTNET", "testnet")] {
                if let Some(after) = rest.strip_prefix(sfx) {
                    if after.trim_start().starts_with('=') {
                        return Some(net);
                    }
                }
            }
        }
    }
    None
}

pub fn set_line_active(line: &str, active: bool) -> String {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let body = line.trim_start().trim_start_matches('#').trim_start();
    if active {
        format!("{indent}{body}")
    } else {
        format!("{indent}# {body}")
    }
}

/// Current network = the slot of the first UNCOMMENTED managed var (default testnet).
pub fn detect_network(text: &str) -> &'static str {
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some(net) = managed_suffix(line) {
            return net;
        }
    }
    "testnet"
}

pub fn rewrite_network(text: &str, target: &str) -> String {
    let trailing_nl = text.ends_with('\n');
    let out = text
        .lines()
        .map(|line| match managed_suffix(line) {
            Some(sfx) => set_line_active(line, sfx == target),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if trailing_nl {
        out + "\n"
    } else {
        out
    }
}

pub fn current_network() -> String {
    std::fs::read_to_string(env_file_path())
        .map(|t| detect_network(&t).to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Flip the env file to the other network; returns a status message for the header.
pub fn toggle_network() -> String {
    let path = env_file_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return format!("cannot read {path}");
    };
    let cur = detect_network(&text);
    let target = if cur == "mainnet" { "testnet" } else { "mainnet" };
    match std::fs::write(&path, rewrite_network(&text, target)) {
        Ok(_) => format!("→ {} — restart scrai to apply", target.to_uppercase()),
        Err(e) => format!("write failed: {e}"),
    }
}

pub fn read_metrics(path: &PathBuf) -> Metrics {
    let mut m = Metrics::default();
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            m.err = Some(format!("cannot open {}: {e}", path.display()));
            return m;
        }
    };
    let blob = |k: &str| -> Option<String> {
        conn.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get::<_, String>(0)).ok()
    };
    // live-grounding counter for the current UTC month (0 if none yet → full allowance)
    m.grounding_used = blob(&format!("grounding:{}", month_utc()))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let sess: SessBlob = blob("sessions").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    let pay: PayBlob = blob("pay").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    // Current layout: kv "quorum_meta" (offenses/blacklist) + quorum_records rows (coins);
    // the legacy whole-store "quorum" blob is read when a server has not migrated yet.
    let quo: QuorumBlob = blob("quorum_meta")
        .or_else(|| blob("quorum"))
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let coins_from_rows: Option<u64> = conn
        .query_row("SELECT COALESCE(SUM(coins), 0) FROM quorum_records", [], |r| r.get::<_, i64>(0))
        .ok()
        .map(|n| n as u64)
        .filter(|n| *n > 0);

    // economy
    let mut payers: HashSet<&str> = HashSet::new();
    for inv in pay.invoices.values() {
        match inv.status.as_str() {
            "paid" => {
                m.inv_paid += 1;
                m.purchased_toku += inv.amount_toku;
                m.purchased_usd += inv.amount_usd as u64;
                if !inv.account_id.is_empty() {
                    payers.insert(inv.account_id.as_str());
                }
                if inv.method == "card" {
                    m.card_paid += 1;
                    m.card_usd += inv.amount_usd as u64;
                }
                let usd = inv.amount_usd as u64;
                match inv.country.as_str() {
                    "" => m.unknown_usd += usd,
                    "DE" => m.de_usd += usd,
                    c if EU.contains(&c) => m.eu_usd += usd,
                    _ => m.row_usd += usd,
                }
            }
            "expired" => m.inv_expired += 1,
            _ => {
                m.inv_pending += 1;
                if inv.method == "card" {
                    m.card_pending += 1;
                }
            }
        }
    }
    for a in pay.entitlements.keys() {
        payers.insert(a.as_str());
    }
    m.paying_accounts = payers.len();
    m.entitlement_out = pay.entitlements.values().sum();
    m.withdrawn_toku = m.purchased_toku.saturating_sub(m.entitlement_out);

    // usage
    m.sessions = sess.sessions.len();
    m.session_balance = sess.sessions.values().map(|s| s.balance).sum();
    m.charges = sess.sessions.values().map(|s| s.counter).sum();
    // NOTE: coins_redeemed is the count of burned ecash SERIALS, an integrity
    // number only — a nym ticketbook holds many tiny coins, so a serial is NOT
    // a $1 coin. Do not dollarize it. Real spend comes from the daily counters.
    m.coins_redeemed = coins_from_rows.unwrap_or(quo.serials.len() as u64);

    // integrity
    m.offenders = quo.offenses.len();
    m.blacklisted = quo.blacklist.len();

    // testnet purchases as the pay blob sees them
    for inv in pay.invoices.values().filter(|i| i.testnet) {
        match inv.status.as_str() {
            "paid" => m.testnet_paid += 1,
            "pending" => m.testnet_pending += 1,
            _ => {}
        }
    }

    // per-day metrics table (may not exist on an un-migrated server)
    // `peak_clients` arrived later — read it as 0 on a db the server hasn't migrated yet
    let has_peak = conn.prepare("SELECT peak_clients FROM daily LIMIT 0").is_ok();
    let peak_col = if has_peak { "peak_clients" } else { "0" };
    // `daily_users` arrived with 0.3.0 — absent on an older db
    let has_users = conn.prepare("SELECT sid FROM daily_users LIMIT 0").is_ok();
    let users_col = if has_users { "(SELECT COUNT(*) FROM daily_users u WHERE u.day = daily.day)" } else { "0" };
    let has_peak1h = conn.prepare("SELECT peak_1h FROM daily LIMIT 0").is_ok();
    let peak1h_col = if has_peak1h { "peak_1h" } else { "0" };
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT day, prompts, spent, purchases, purchased, cost, {peak_col}, {users_col}, {peak1h_col} FROM daily ORDER BY day DESC LIMIT 12"
    )) {
        m.has_daily = true;
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok(DayRow {
                day: r.get(0)?,
                prompts: r.get::<_, i64>(1)? as u64,
                spent: r.get::<_, i64>(2)? as u64,
                purchases: r.get::<_, i64>(3)? as u64,
                purchased: r.get::<_, i64>(4)? as u64,
                cost: r.get::<_, i64>(5)? as u64,
                peak_clients: r.get::<_, i64>(6)? as u64,
                users: r.get::<_, i64>(7)? as u64,
                peak_1h: r.get::<_, i64>(8)? as u64,
                per_model: Default::default(),
                faucet: 0,
            })
        }) {
            m.daily = rows.filter_map(|r| r.ok()).collect();
        }
        if has_peak {
            let _ = conn.query_row("SELECT COALESCE(MAX(peak_clients),0) FROM daily", [], |r| {
                m.peak_clients_max = r.get::<_, i64>(0)? as u64;
                Ok(())
            });
        }
        m.metrics_tz = conn
            .query_row("SELECT v FROM kv WHERE k = 'metrics_tz'", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|_| "UTC".into());
        // per-model prompts for the same days (table may not exist on an older server)
        if let Ok(mut st) = conn.prepare("SELECT day, model, prompts, spent, cost FROM daily_model") {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64, r.get::<_, i64>(4)? as u64))
            }) {
                let mut totals: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
                for (day, model, n, spent, cost) in rows.filter_map(|r| r.ok()) {
                    if let Some(d) = m.daily.iter_mut().find(|d| d.day == day) {
                        let e = d.per_model.entry(model.clone()).or_default();
                        e.prompts += n;
                        e.spent += spent;
                        e.cost += cost;
                        *totals.entry(model).or_insert(0) += n;
                    }
                }
                let mut models: Vec<(String, u64)> = totals.into_iter().collect();
                models.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                m.models = models.into_iter().map(|(k, _)| k).collect();
            }
        }
        let _ = conn.query_row(
            "SELECT COALESCE(SUM(prompts),0), COALESCE(SUM(spent),0), COALESCE(SUM(purchases),0), COALESCE(SUM(purchased),0), COALESCE(SUM(cost),0) FROM daily",
            [],
            |r| {
                m.total_prompts = r.get::<_, i64>(0)? as u64;
                m.total_spent = r.get::<_, i64>(1)? as u64;
                m.total_purchases = r.get::<_, i64>(2)? as u64;
                m.total_purchased = r.get::<_, i64>(3)? as u64;
                m.total_cost = r.get::<_, i64>(4)? as u64;
                Ok(())
            },
        );
    }

    // faucet.db (scrai-faucet, same data dir): one row per funded memo. Read-only, and
    // absent on any server that never ran the faucet.
    if let Some(fdb) = conn.path().map(std::path::Path::new).and_then(|p| p.parent()).map(|d| d.join("faucet.db")) {
        if fdb.exists() {
            if let Ok(fc) = Connection::open_with_flags(&fdb, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) {
                m.has_faucet = true;
                m.faucet_daily_max = env_file_value("FAUCET_DAILY_MAX").and_then(|v| v.parse().ok()).unwrap_or(20);
                let today_start = { let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0); t - t % 86_400 };
                m.faucet_codes = crate::faucet::list_codes(&fc).unwrap_or_default();
                if let Ok(mut st) = fc.prepare("SELECT ts, unym, stage FROM claims") {
                    if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))) {
                        for (ts, unym, stage) in rows.filter_map(|r| r.ok()) {
                            if ts >= today_start {
                                m.faucet_today += 1; // every row counts for the cap, like scrai-faucet's claims_since
                            }
                            if stage == "sent" {
                                m.faucet_claims += 1;
                                m.faucet_unym += unym.max(0) as u64;
                                let day = civil_day_utc(ts);
                                if let Some(d) = m.daily.iter_mut().find(|d| d.day == day) {
                                    d.faucet += 1;
                                }
                            } else if stage == "sending" || stage == "failed" {
                                m.faucet_stuck += 1; // needs a human: broadcast unknown/failed
                            }
                        }
                    }
                }
            }
        }
    }

    m.ok = true;
    m
}

// ---- formatting -----------------------------------------------------------
pub fn grp(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i).is_multiple_of(3) {
            out.push('\u{2009}'); // thin space
        }
        out.push(*c as char);
    }
    out
}

pub fn usd(scrai: u64) -> String {
    format!("${:.2}", scrai as f64 / TOKU_PER_USD as f64)
}

/// Four decimals — the drill-down's per-model figures: a nano-model prompt costs a few
/// thousandths of a cent, and "$0.04 / $0.04" at two decimals hides the margin.
pub fn usd4(scrai: u64) -> String {
    format!("${:.4}", scrai as f64 / TOKU_PER_USD as f64)
}

/// € per $ for the cost columns — Google's AI Studio dashboard and invoice are in EUR at
/// Google's own monthly rate, so the operator sets the rate they see (`FX_EUR_PER_USD`
/// in .env). None → dollars only, no silent conversion.
pub fn eur_per_usd() -> Option<f64> {
    env_file_value("FX_EUR_PER_USD").and_then(|v| v.replace(',', ".").parse::<f64>().ok()).filter(|r| *r > 0.0)
}

pub fn eur(scrai: u64, rate: f64) -> String {
    format!("€{:.2}", scrai as f64 / TOKU_PER_USD as f64 * rate)
}

pub fn clock_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = secs % 86_400;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

#[derive(Clone)]
pub struct Hit {
    pub invoice: String,
    pub receipt: String,
    pub usd: u32,
    pub toku: u64,
    pub paid_at: u64,
    pub method: String,
    pub country: String,
    pub consent: String,
    pub provider_ref: String,
    pub status: String,
    pub is_voucher: bool,
    /// (toku, redeemed_at, void_at) when a code was minted for this invoice
    pub voucher: Option<(u64, Option<u64>, Option<u64>)>,
    /// The voucher's fingerprint, when the search was by code. Set, a void goes by hash —
    /// which is what still works once the purchase link has expired.
    pub hash: String,
    /// In-app purchases: credit still sitting on the account, not yet withdrawn into
    /// blind-signed coins. `None` once the link-window account link has been scrubbed.
    pub entitlement: Option<u64>,
}

impl Hit {
    /// The one question a refund turns on. For a voucher it is answerable for certain; for
    /// an in-app purchase only while the credit has not been withdrawn, because after that
    /// the coins are blind-signed and nobody — us included — can tell whether they were
    /// spent.
    pub fn refundable(&self) -> Result<String, String> {
        if self.status != "paid" {
            return Err(format!("this invoice is {}, so nothing was ever charged", self.status));
        }
        match (self.is_voucher, self.voucher) {
            (true, None) => Err("paid, but no code was ever issued — nothing to void".into()),
            (true, Some((_, Some(at), _))) => {
                Err(format!("the code was REDEEMED on {} — spent credit cannot be clawed back", crate::pay::utc_stamp(at)))
            }
            (true, Some((_, None, Some(at)))) => Err(format!("already voided on {}", crate::pay::utc_stamp(at))),
            (true, Some((toku, None, None))) => Ok(format!("code NOT redeemed — {} TOKU can be voided", grp(toku))),
            (false, _) => match self.entitlement {
                None => Err("in-app purchase, and the account link has expired — refund by hand if you decide to".into()),
                Some(e) if e >= self.toku => Ok(format!(
                    "in-app, {} TOKU still un-withdrawn — provably unspent, but there is nothing to void: \
                     refund the money and the credit stays with the buyer",
                    grp(e)
                )),
                Some(e) => Err(format!(
                    "in-app, only {} TOKU un-withdrawn of {} bought — the rest is blind-signed and unknowable",
                    grp(e),
                    grp(self.toku)
                )),
            },
        }
    }
}

/// Find a purchase by whatever the buyer could plausibly quote. Every branch is a different
/// strength of evidence, and the strongest one is the code itself: holding it is what being
/// entitled to it means.
pub fn find_purchase(state_db: &Path, q: &str) -> Vec<Hit> {
    let raw = q.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let up: String = raw.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();

    let conn = match Connection::open_with_flags(state_db, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let blob: Option<String> = conn.query_row("SELECT v FROM kv WHERE k = 'pay'", [], |r| r.get(0)).ok();
    let pay: PayBlob = blob.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();

    // A pasted code names its row directly — try the keyed fingerprint and the unkeyed one
    // that preceded it, exactly as the redeem path does.
    let mut by_code: Option<String> = None;
    let mut by_hash = String::new();
    if up.starts_with("TOKU") {
        for h in [crate::pay::voucher_hash(raw), crate::pay::voucher_hash_legacy(raw)] {
            if let Some(inv) = crate::store::voucher_invoice_for(state_db, &h) {
                // The link to the purchase has expired: this is a voucher and nothing else.
                // Still a full answer — value, state, voidable — just no receipt behind it.
                if inv.starts_with(crate::store::EXPIRED_LINK) {
                    let st = crate::store::voucher_state_by_hash(state_db, &h);
                    return vec![Hit {
                        invoice: inv,
                        receipt: "-".into(),
                        usd: st.map(|s| (s.0 / TOKU_PER_USD) as u32).unwrap_or(0),
                        toku: st.map(|s| s.0).unwrap_or(0),
                        paid_at: 0,
                        method: "-".into(),
                        country: "--".into(),
                        consent: "-".into(),
                        provider_ref: "purchase link expired — refund against the buyer's receipt".into(),
                        status: "paid".into(),
                        is_voucher: true,
                        voucher: st,
                        entitlement: None,
                        hash: h,
                    }];
                }
                by_code = Some(inv);
                by_hash = h;
                break;
            }
        }
    }
    // A receipt number carries a PREFIX of the invoice id, so it lives under the birthday
    // bound: two purchases can share one. Twelve hex makes that vanishingly unlikely, eight
    // (the old length, still in circulation) did not. Either way, never act on the first
    // match — collect them all and make the operator choose.
    // Any length from the old eight up to a whole id: receipts printed before the number was
    // widened must keep working, and a longer one simply matches more precisely.
    let prefix = up
        .strip_prefix("TKM-")
        .and_then(|r| r.split_once('-'))
        .map(|(_, p)| p.to_ascii_lowercase())
        .filter(|p| (8..=32).contains(&p.len()) && p.chars().all(|c| c.is_ascii_hexdigit()));

    let mut out: Vec<Hit> = Vec::new();
    for (id, inv) in pay.invoices.iter() {
        let matches = match (&by_code, &prefix) {
            (Some(want), _) => id == want,
            (None, Some(p)) => id.to_ascii_lowercase().starts_with(p.as_str()),
            (None, None) => {
                id.eq_ignore_ascii_case(&up) || inv.provider_ref.eq_ignore_ascii_case(raw)
            }
        };
        if !matches {
            continue;
        }
        out.push(Hit {
            receipt: crate::pay::receipt_number(id, inv.paid_at),
            invoice: id.clone(),
            usd: inv.amount_usd,
            toku: inv.amount_toku,
            paid_at: inv.paid_at,
            method: if inv.method.is_empty() { "?".into() } else { inv.method.clone() },
            country: if inv.country.is_empty() { "--".into() } else { inv.country.clone() },
            consent: if inv.consent_version.is_empty() { "-".into() } else { inv.consent_version.clone() },
            provider_ref: inv.provider_ref.clone(),
            status: inv.status.clone(),
            is_voucher: inv.voucher,
            voucher: crate::store::voucher_state(state_db, id),
            entitlement: (!inv.account_id.is_empty()).then(|| *pay.entitlements.get(&inv.account_id).unwrap_or(&0)),
            hash: by_hash.clone(),
        });
    }
    out.sort_by(|a, b| b.paid_at.cmp(&a.paid_at));
    out.truncate(12);
    out
}

/// A hit for a bare fingerprint — what the web console sends back when the operator picked
/// a code whose purchase link has expired.
pub fn hit_by_hash(state_db: &Path, hash: &str) -> Option<Hit> {
    let inv = crate::store::voucher_invoice_for(state_db, hash)?;
    let st = crate::store::voucher_state_by_hash(state_db, hash);
    Some(Hit {
        invoice: inv,
        receipt: "-".into(),
        usd: st.map(|s| (s.0 / TOKU_PER_USD) as u32).unwrap_or(0),
        toku: st.map(|s| s.0).unwrap_or(0),
        paid_at: 0,
        method: "-".into(),
        country: "--".into(),
        consent: "-".into(),
        provider_ref: "by code".into(),
        status: "paid".into(),
        is_voucher: true,
        voucher: st,
        entitlement: None,
        hash: hash.to_string(),
    })
}

pub fn do_void(state_db: &Path, hit: &Hit, reason: &str, evidence: &str) -> String {
    let now = crate::pay::now_ms();
    // By fingerprint when the search was by code — that is the void that outlives the
    // purchase link — and by invoice otherwise.
    let outcome = if hit.hash.is_empty() {
        crate::store::void_voucher(state_db, &hit.invoice, now)
    } else {
        crate::store::void_voucher_by_hash(state_db, &hit.hash, now)
    };
    match outcome {
        Err(e) => format!("could not void: {e}"),
        Ok(crate::store::VoucherVoid::Unknown) => "no code exists for that invoice".into(),
        Ok(crate::store::VoucherVoid::AlreadySpent) => {
            "that code is redeemed or already void — nothing was changed".into()
        }
        Ok(crate::store::VoucherVoid::Voided(_)) => {
            crate::pay::append_refund(&hit.invoice, hit.paid_at, hit.usd, &hit.method, &hit.provider_ref, reason, evidence);
            let how = if hit.method == "card" {
                format!("refund ${} in Mollie against {}", hit.usd, hit.provider_ref)
            } else {
                format!("send ${} worth back to the address that paid memo {}", hit.usd, hit.provider_ref)
            };
            format!("VOIDED {} · logged to refunds.csv · now: {how}", hit.receipt)
        }
    }
}

/// One issued code, of either kind, flattened for a list the operator can scan.
pub struct CodeItem {
    /// "invite" (a faucet code, worth $1, possibly multi-use) or "voucher" (bought on the
    /// website, worth what was paid, single-use).
    pub kind: &'static str,
    /// The invite code itself — or, for a voucher, the first characters of its FINGERPRINT.
    /// Never the voucher code: we do not have it. That is the design, and it is also why a
    /// lost voucher cannot be read back to anybody, by us or by support.
    pub label: String,
    pub note: String,
    pub usd: u32,
    pub issued: u64,
    pub uses: u32,
    pub max_uses: u32,
    pub redeemed_at: u64,
    pub void_at: u64,
    /// The invoice a voucher was minted from — the join to sales.csv, and to a refund.
    pub invoice: String,
    /// Who redeemed it, truncated. Only ever a voucher, only for the ACCOUNT_LINK_DAYS (seven by default) the
    /// account link survives, and never a person: it is an account's own public-key hash.
    /// An invite code has none by construction — the faucet ledger records a memo and an
    /// invoice, and deliberately nothing about who typed the code.
    pub who: String,
    pub open: bool,
}

/// Every code this server has issued, newest first: faucet invite codes and website
/// vouchers in one list, because "is it still outstanding?" is the same question for both.
///
/// What the list can and cannot answer, since somebody will ask it of this screen: WHEN a
/// code was redeemed is known for both kinds. WHO redeemed it is known for a voucher for
/// ACCOUNT_LINK_DAYS (seven by default), as an account id, and for an invite code never.
pub fn list_issued_codes(state_db: &Path) -> Vec<CodeItem> {
    let ro = |p: &Path| {
        Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()
    };
    let mut out: Vec<CodeItem> = Vec::new();

    if let Some(conn) = ro(state_db) {
        if let Ok(mut st) = conn.prepare(
            "SELECT hash, toku, invoice, created_at, redeemed_at, void_at, account \
             FROM vouchers ORDER BY created_at DESC",
        ) {
            if let Ok(rows) = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?.max(0) as u64,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?.max(0) as u64,
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
                    r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                ))
            }) {
                for (hash, toku, invoice, created, redeemed, void, account) in rows.flatten() {
                    out.push(CodeItem {
                        kind: "voucher",
                        label: hash.chars().take(12).collect(),
                        note: String::new(),
                        usd: (toku / TOKU_PER_USD) as u32,
                        issued: created,
                        uses: u32::from(redeemed > 0),
                        max_uses: 1,
                        redeemed_at: redeemed,
                        void_at: void,
                        invoice,
                        who: account.chars().take(12).collect(),
                        open: redeemed == 0 && void == 0,
                    });
                }
            }
        }
    }

    // Invite codes live in the faucet's own ledger next to state.db. Absent on a server
    // that never ran the faucet, which is not an error.
    let fdb = state_db.parent().map(|d| d.join("faucet.db")).unwrap_or_default();
    if fdb.exists() {
        if let Some(fc) = ro(&fdb) {
            let claimed: HashMap<String, u64> = fc
                .prepare("SELECT code, MAX(ts) FROM claims GROUP BY code")
                .ok()
                .and_then(|mut st| {
                    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64)))
                        .ok()
                        .map(|rows| rows.flatten().collect())
                })
                .unwrap_or_default();
            for c in crate::faucet::list_codes(&fc).unwrap_or_default() {
                out.push(CodeItem {
                    kind: "invite",
                    label: c.code.clone(),
                    note: c.note,
                    usd: crate::pay::TESTNET_USD,
                    // faucet.db keeps SECONDS; everything else on this screen is milliseconds.
                    issued: c.created.saturating_mul(1000),
                    uses: c.uses,
                    max_uses: c.max_uses,
                    redeemed_at: claimed.get(&c.code).copied().unwrap_or(0).saturating_mul(1000),
                    void_at: 0,
                    invoice: String::new(),
                    who: String::new(),
                    open: c.max_uses.saturating_sub(c.uses) > 0,
                });
            }
        }
    }

    out.sort_by(|a, b| b.issued.cmp(&a.issued));
    out
}

pub fn mint_invite_code(state_db: &Path) -> String {
    let Some(dir) = state_db.parent() else { return "no data dir".into() };
    let fdb = dir.join("faucet.db");
    match crate::faucet::open_db(&fdb).and_then(|c| crate::faucet::mint(&c, crate::faucet::DEFAULT_CODE_USES, "admin")) {
        Ok(code) => format!("invite code {code} (1 claim) — hand it to a tester"),
        Err(e) => format!("could not mint a code: {e}"),
    }
}
