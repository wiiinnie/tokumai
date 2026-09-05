// scrai-admin — a read-only, htop-style dashboard over the server's state.db.
//
// Run it on the box (SSH): `scrai-admin [path/to/state.db]`. It opens the DB READ-ONLY
// (never writes, safe alongside a live server), aggregates the JSON blobs the server
// snapshots (sessions / pay / quorum) plus the per-day `daily` counters, and refreshes
// like htop. Aggregate-only: no account/session ids, no message content — the server
// never stores those anyway.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

const TOKU_PER_USD: u64 = 100_000; // one coconut coin = $1 = 100_000 TOKU

// ---- blob shapes (subset; serde ignores the fields we don't name) ----------
#[derive(Deserialize, Default)]
struct SessBlob {
    #[serde(default)]
    sessions: HashMap<String, Sess>,
}
#[derive(Deserialize, Default, Clone, Copy)]
struct Sess {
    #[serde(default)]
    balance: u64,
    #[serde(default)]
    counter: u64,
}
#[derive(Deserialize, Default)]
struct PayBlob {
    #[serde(default)]
    invoices: HashMap<String, Inv>,
    #[serde(default)]
    entitlements: HashMap<String, u64>,
}
#[derive(Deserialize, Default)]
struct Inv {
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    amount_usd: u32,
    #[serde(default)]
    amount_toku: u64,
    #[serde(default)]
    status: String,
    /// raised as a $1 faucet-paid testnet purchase (TESTNET servers)
    #[serde(default)]
    testnet: bool,
    /// rail that served it: "btc" | "nyx" | "card" (Mollie) — absent on pre-card records
    #[serde(default)]
    method: String,
}
#[derive(Deserialize, Default)]
struct QuorumBlob {
    #[serde(default)]
    serials: HashMap<String, serde_json::Value>,
    #[serde(default)]
    offenses: HashMap<String, serde_json::Value>,
    #[serde(default)]
    blacklist: HashSet<String>,
}

/// Column header for a model: the catalog label from pricing.json ("Nano Banana 2 Lite",
/// "Gemini 3.5 Flash-Lite"), wrapped onto two lines of ≤ 12 chars so neighbouring models
/// stay tellable apart; the raw id only when the table doesn't know the model.
fn model_header(id: &str) -> String {
    static PRICING: std::sync::OnceLock<Option<scrai_core::pricing::PricingTable>> = std::sync::OnceLock::new();
    let table = PRICING.get_or_init(|| scrai_core::pricing::PricingTable::parse(include_str!("../../../pricing.json")).ok());
    let label = table
        .as_ref()
        .and_then(|t| t.label(id))
        .map(|l| l.to_string())
        .unwrap_or_else(|| id.trim_start_matches("gemini-").replace('-', " "));
    two_lines(&label, 12)
}

/// Wrap `text` at word boundaries onto at most two lines of ≤ `width` chars (a longer
/// tail is cut with "…"), e.g. "Gemini 3.5 Flash-Lite" → "Gemini 3.5\nFlash-Lite".
fn two_lines(text: &str, width: usize) -> String {
    let mut lines: Vec<String> = vec![String::new()];
    for word in text.split_whitespace() {
        let last = lines.len() - 1;
        let fits = lines[last].chars().count() + 1 + word.chars().count() <= width;
        if lines[last].is_empty() {
            lines[last].push_str(word);
        } else if !fits && lines.len() < 2 {
            lines.push(word.to_string());
        } else {
            lines[last].push(' ');
            lines[last].push_str(word);
        }
    }
    lines
        .into_iter()
        .map(|l| if l.chars().count() > width { l.chars().take(width - 1).collect::<String>() + "…" } else { l })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Default)]
struct DayRow {
    day: String,
    prompts: u64,
    spent: u64,
    /// raw provider price for the day (no margin) — the number to reconcile with the
    /// provider's own billing view (Google AI Studio buckets days in Pacific time, ours are UTC)
    cost: u64,
    purchases: u64,
    purchased: u64,
    /// most clients served in parallel at one instant that day (chat/catalog/payment in flight)
    peak_clients: u64,
    /// most DIFFERENT clients that sent anything within one 60-second window that day
    peak_1m: u64,
    /// DIFFERENT paying sessions that chatted that day (`daily_users`; 0 before it existed).
    /// One person = one session unless they bump the session index; free-tier chats
    /// carry no session and are not counted.
    users: u64,
    /// per model id that day (from `daily_model`; empty for days before it existed)
    per_model: std::collections::HashMap<String, ModelDay>,
    /// faucet payments that day (from faucet.db next to state.db; UTC days)
    faucet: u64,
}

/// One model's share of a day: prompts, what users paid, what the provider charged us.
#[derive(Default, Clone, Copy)]
struct ModelDay {
    prompts: u64,
    spent: u64,
    cost: u64,
}

/// Which invoice a model lands on. Google is reconciled in € (AI Studio, Pacific-day
/// buckets), OpenAI in $ (usage dashboard, UTC days) — hence the separate blocks.
fn provider_of(model: &str) -> &'static str {
    if model.starts_with("gemini") || model.starts_with("imagen") || model.starts_with("veo") {
        "GOOGLE"
    } else if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4") {
        "OPENAI"
    } else {
        "OTHER"
    }
}

/// Single-line catalog label for the drill-down rows ("Nano Banana 2 Lite"), raw id as fallback.
fn model_label(id: &str) -> String {
    model_header(id).replace('\n', " ")
}

/// What the operator is looking at: the selected day row and whether the drawer with the
/// testnet faucet + integrity details is open.
#[derive(Default)]
struct View {
    sel: usize,
    drawer: bool,
}

#[derive(Default)]
struct Metrics {
    ok: bool,
    err: Option<String>,
    // economy (from the pay blob — current authoritative state)
    paying_accounts: usize,
    inv_paid: usize,
    inv_pending: usize,
    inv_expired: usize,
    purchased_toku: u64,
    purchased_usd: u64,
    // card rail (Mollie): separately visible because it is the one rail with chargebacks
    card_paid: usize,
    card_pending: usize,
    card_usd: u64,
    entitlement_out: u64,
    withdrawn_toku: u64,
    // usage
    sessions: usize,
    session_balance: u64,
    charges: u64,
    coins_redeemed: u64,
    // integrity
    offenders: usize,
    blacklisted: usize,
    // lifetime + per-day (from the `daily` table; "since metrics enabled")
    total_prompts: u64,
    total_spent: u64,
    total_cost: u64,
    total_purchases: u64,
    total_purchased: u64,
    daily: Vec<DayRow>,
    has_daily: bool,
    /// highest `peak_clients` over all recorded days — the capacity signal (a MAX, not a sum)
    peak_clients_max: u64,
    /// model ids seen in the shown days, most-used first — one table column each
    models: Vec<String>,
    /// day-boundary zone the server buckets with (METRICS_TZ; "UTC" if unset/old server)
    metrics_tz: String,
    // live-grounding queries used this UTC month (Gemini's 5,000/mo free allowance)
    grounding_used: u64,
    // testnet faucet (TESTNET servers): invoices flagged testnet + faucet.db claims
    testnet_paid: usize,
    testnet_pending: usize,
    faucet_claims: u64,
    faucet_unym: u64,
    faucet_stuck: u64,
    /// claim rows stamped today (UTC) — the number `scrai-faucet` compares with its cap
    faucet_today: u64,
    /// FAUCET_DAILY_MAX from the env file (default 20, as in scrai-faucet)
    faucet_daily_max: u64,
    has_faucet: bool,
    /// invite codes (newest first) — minted here with `c`
    faucet_codes: Vec<scrai_server::faucet::CodeRow>,
}

/// Gemini's monthly free Grounding allowance — mirror of chat::GROUNDING_FREE_PER_MONTH.
const GROUNDING_FREE_PER_MONTH: u64 = 5000;

/// Current UTC month as `YYYY-MM` (same civil_from_days math as the server's today_utc,
/// so we need no date crate) — picks the right `grounding:<month>` counter key.
/// "YYYY-MM-DD" of a unix timestamp in UTC (the faucet stamps claims in UTC).
fn civil_day_utc(secs: i64) -> String {
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

fn month_utc() -> String {
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
const NET_VARS: &[&str] = &[
    "GEMINI_API_KEY",
    "BTCPAY_URL",
    "BTCPAY_STORE_ID",
    "BTCPAY_API_KEY",
    "NYX_LCD_URL",
    "NYX_RECEIVE_ADDRESS",
];

fn env_file_path() -> String {
    scrai_server::cfg("ENV_FILE").unwrap_or_else(|_| "/opt/scrai/.env".into())
}

/// `KEY=value` from the env file (uncommented lines only; quotes stripped), falling back to
/// the process environment — so the panel shows the cap the services actually run with.
fn env_file_value(key: &str) -> Option<String> {
    // Both spellings, newest line wins — a .env that still carries SCRAI_<key> must read
    // the same here as it does in the server (see scrai_server::cfg).
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
    from_file.or_else(|| scrai_server::cfg(key).ok()).filter(|v| !v.is_empty())
}

/// If `line` assigns one of the managed vars, which network slot is it (comment state ignored)?
fn managed_suffix(line: &str) -> Option<&'static str> {
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

fn set_line_active(line: &str, active: bool) -> String {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let body = line.trim_start().trim_start_matches('#').trim_start();
    if active {
        format!("{indent}{body}")
    } else {
        format!("{indent}# {body}")
    }
}

/// Current network = the slot of the first UNCOMMENTED managed var (default testnet).
fn detect_network(text: &str) -> &'static str {
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

fn rewrite_network(text: &str, target: &str) -> String {
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

fn current_network() -> String {
    std::fs::read_to_string(env_file_path())
        .map(|t| detect_network(&t).to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Flip the env file to the other network; returns a status message for the header.
fn toggle_network() -> String {
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

fn read_metrics(path: &PathBuf) -> Metrics {
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
    let has_peak1m = conn.prepare("SELECT peak_1m FROM daily LIMIT 0").is_ok();
    let peak1m_col = if has_peak1m { "peak_1m" } else { "0" };
    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT day, prompts, spent, purchases, purchased, cost, {peak_col}, {users_col}, {peak1m_col} FROM daily ORDER BY day DESC LIMIT 12"
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
                peak_1m: r.get::<_, i64>(8)? as u64,
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
                m.faucet_codes = scrai_server::faucet::list_codes(&fc).unwrap_or_default();
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
fn grp(n: u64) -> String {
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
fn usd(scrai: u64) -> String {
    format!("${:.2}", scrai as f64 / TOKU_PER_USD as f64)
}
/// Four decimals — the drill-down's per-model figures: a nano-model prompt costs a few
/// thousandths of a cent, and "$0.04 / $0.04" at two decimals hides the margin.
fn usd4(scrai: u64) -> String {
    format!("${:.4}", scrai as f64 / TOKU_PER_USD as f64)
}

/// € per $ for the cost columns — Google's AI Studio dashboard and invoice are in EUR at
/// Google's own monthly rate, so the operator sets the rate they see (`FX_EUR_PER_USD`
/// in .env). None → dollars only, no silent conversion.
fn eur_per_usd() -> Option<f64> {
    env_file_value("FX_EUR_PER_USD").and_then(|v| v.replace(',', ".").parse::<f64>().ok()).filter(|r| *r > 0.0)
}

fn eur(scrai: u64, rate: f64) -> String {
    format!("€{:.2}", scrai as f64 / TOKU_PER_USD as f64 * rate)
}

const GOLD: Color = Color::Rgb(203, 161, 78);
const SAGE: Color = Color::Rgb(138, 160, 107);
const RUST: Color = Color::Rgb(193, 90, 67);
const BONE: Color = Color::Rgb(236, 230, 220);
const DIM: Color = Color::Rgb(120, 112, 100);

fn kv<'a>(label: &'a str, val: String, c: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<20}"), Style::default().fg(DIM)),
        Span::styled(val, Style::default().fg(c).add_modifier(Modifier::BOLD)),
    ])
}

fn block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ))
}

fn ui(f: &mut Frame, m: &Metrics, view: &View, path: &str, clock: &str, network: &str, status: &str) {
    let root = Layout::vertical([
        Constraint::Length(1),
        // 11 → inner 9 → 8 content lines + gauge, so USAGE's grounding line fits (the
        // bottom row has ample empty space to give up).
        Constraint::Length(11),
        Constraint::Min(6),
        Constraint::Length(1), // strip: faucet + integrity one-liners, drawer key
        Constraint::Length(1), // footer note / status
    ])
    .split(f.area());

    // header
    let header = Line::from(vec![
        Span::styled("Scramble", Style::default().fg(BONE).add_modifier(Modifier::BOLD)),
        Span::styled("AI", Style::default().fg(SAGE).add_modifier(Modifier::BOLD)),
        Span::styled("  server admin", Style::default().fg(DIM)),
        Span::styled(format!("  v{}", scrai_server::VERSION), Style::default().fg(DIM)),
        Span::styled(format!("   {path}"), Style::default().fg(DIM)),
        Span::styled(format!("   {clock} UTC"), Style::default().fg(GOLD)),
        Span::styled(
            format!("   net:{network}"),
            Style::default().fg(if network == "mainnet" { RUST } else { SAGE }).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ↑↓ day · i details · n toggle · c code · q quit", Style::default().fg(DIM)),
        Span::styled(
            if status.is_empty() { String::new() } else { format!("   {status}") },
            Style::default().fg(GOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header), root[0]);

    if !m.ok {
        let e = m.err.clone().unwrap_or_else(|| "no state".into());
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(e, Style::default().fg(RUST))))
                .block(block("state.db")),
            root[1],
        );
        return;
    }

    // top row: ECONOMY | USAGE
    let top = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(root[1]);

    let econ = vec![
        kv("paying accounts", grp(m.paying_accounts as u64), SAGE),
        kv("invoices", format!("{} paid · {} pending · {} exp", m.inv_paid, m.inv_pending, m.inv_expired), BONE),
        kv("purchased", format!("{}  ({} scrai)", usd(m.purchased_toku), grp(m.purchased_toku)), GOLD),
        // cards (Mollie): the only rail money can be pulled back from — watch it separately
        kv(
            "-> by card",
            if m.card_paid + m.card_pending > 0 {
                format!("{} paid · ${} · {} open", m.card_paid, grp(m.card_usd), m.card_pending)
            } else {
                "none".into()
            },
            if m.card_paid > 0 { GOLD } else { DIM },
        ),
        kv("entitlement open", format!("{}  ({} scrai)", usd(m.entitlement_out), grp(m.entitlement_out)), BONE),
        kv("-> withdrawn ecash", format!("{}  ({} scrai)", usd(m.withdrawn_toku), grp(m.withdrawn_toku)), SAGE),
        // testnet faucet: how many $1 test buys were funded, and what that cost in NYM
        kv(
            "testnet faucet",
            if m.has_faucet || m.testnet_paid + m.testnet_pending > 0 {
                format!(
                    "{} funded · {:.1} NYM · {} paid · {} open{}",
                    m.faucet_claims,
                    m.faucet_unym as f64 / 1e6,
                    m.testnet_paid,
                    m.testnet_pending,
                    if m.faucet_stuck > 0 { format!(" · {} STUCK", m.faucet_stuck) } else { String::new() }
                ) + &if m.has_faucet && m.faucet_today >= m.faucet_daily_max { format!(" · DAILY LIMIT {}/{}", m.faucet_today, m.faucet_daily_max) } else { String::new() }
            } else {
                "off".into()
            },
            if m.faucet_stuck > 0 || (m.has_faucet && m.faucet_today >= m.faucet_daily_max) { RUST } else if m.has_faucet { GOLD } else { DIM },
        ),
    ];
    let eb = block("ECONOMY · money in");
    let ei = eb.inner(top[0]);
    f.render_widget(eb, top[0]);
    let er = Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).split(ei);
    f.render_widget(Paragraph::new(econ), er[0]);
    let g1 = ratio(m.withdrawn_toku, m.purchased_toku);
    f.render_widget(
        Gauge::default().gauge_style(Style::default().fg(SAGE)).ratio(g1).label(format!("withdrawn {:.0}%", g1 * 100.0)),
        er[1],
    );

    let usage = vec![
        kv("chat prompts (total)", grp(m.total_prompts), SAGE),
        kv("active sessions", grp(m.sessions as u64), BONE),
        kv("session credit", format!("{}  ({} scrai)", usd(m.session_balance), grp(m.session_balance)), GOLD),
        kv("chat revenue", format!("{}  ({} scrai)", usd(m.total_spent), grp(m.total_spent)), GOLD),
        kv(
            "provider cost",
            match eur_per_usd() {
                Some(r) => format!("-{}  ≈ {}  ({} scrai)", usd(m.total_cost), eur(m.total_cost, r), grp(m.total_cost)),
                None => format!("-{}  ({} scrai)", usd(m.total_cost), grp(m.total_cost)),
            },
            BONE,
        ),
        kv(
            "  € rate",
            match eur_per_usd() {
                Some(r) => format!("{r:.3} €/$ — FX_EUR_PER_USD, Google's invoice rate"),
                None => "not set (FX_EUR_PER_USD) — dollars only".into(),
            },
            DIM,
        ),
        kv("= profit", format!("{}  ({} scrai)", usd(m.total_spent.saturating_sub(m.total_cost)), grp(m.total_spent.saturating_sub(m.total_cost))), SAGE),
        kv("  since metrics deploy", String::new(), DIM),
        kv(
            "users today",
            format!("{} different paying sessions chatted", grp(m.daily.first().map(|d| d.users).unwrap_or(0))),
            BONE,
        ),
        kv(
            "max simultaneous clients",
            format!(
                "{} today · {} all-time",
                grp(m.daily.first().map(|d| d.peak_clients).unwrap_or(0)),
                grp(m.peak_clients_max)
            ),
            SAGE,
        ),
        kv(
            "live search free left",
            format!(
                "{} of {}  ({} used this month)",
                grp(GROUNDING_FREE_PER_MONTH.saturating_sub(m.grounding_used)),
                grp(GROUNDING_FREE_PER_MONTH),
                grp(m.grounding_used)
            ),
            if m.grounding_used >= GROUNDING_FREE_PER_MONTH { GOLD } else { SAGE },
        ),
    ];
    let ub = block("USAGE · chats & credit");
    let ui_area = ub.inner(top[1]);
    f.render_widget(ub, top[1]);
    let ur = Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).split(ui_area);
    f.render_widget(Paragraph::new(usage), ur[0]);
    // honest gauge: real spend (daily counters) against what was purchased
    let g2 = ratio(m.total_spent, m.purchased_toku);
    f.render_widget(
        Gauge::default().gauge_style(Style::default().fg(GOLD)).ratio(g2).label(format!("spent of purchased {:.0}%", g2 * 100.0)),
        ur[1],
    );

    // bottom row: DAILY totals (↑↓ picks a day) | that day by provider and model
    let bot = Layout::horizontal([Constraint::Min(60), Constraint::Length(74)]).split(root[2]);
    let fx = eur_per_usd();
    let margin_txt = |spent: u64, cost: u64| -> (String, Color) {
        if cost > 0 {
            let pct = (spent as f64 / cost as f64 - 1.0) * 100.0;
            (format!("{pct:+.1}%"), if pct > 100.0 || pct < 0.0 { RUST } else { SAGE })
        } else {
            ("—".into(), DIM)
        }
    };

    let mut header: Vec<&str> = vec!["day", "prompts", "spent", "cost"];
    if fx.is_some() {
        header.push("cost €"); // next to the $ figure — the number to match with AI Studio
    }
    header.extend(["margin", "buys", "buys $", "users", "1 min", "peak"]);
    let header_row = Row::new(header).style(Style::default().fg(DIM));
    let rows: Vec<Row> = if m.daily.is_empty() {
        vec![Row::new(vec![Cell::from(Span::styled(
            if m.has_daily { "no activity yet today" } else { "server not yet redeployed with metrics" },
            Style::default().fg(DIM),
        ))])]
    } else {
        m.daily
            .iter()
            .map(|d| {
                let (mt, mc) = margin_txt(d.spent, d.cost);
                Row::new(
                    vec![
                        Cell::from(Span::styled(d.day.clone(), Style::default().fg(BONE))),
                        Cell::from(Span::styled(grp(d.prompts), Style::default().fg(SAGE))),
                        Cell::from(Span::styled(usd(d.spent), Style::default().fg(GOLD))),
                        Cell::from(Span::styled(usd(d.cost), Style::default().fg(BONE))),
                    ]
                    .into_iter()
                    .chain(fx.map(|r| Cell::from(Span::styled(eur(d.cost, r), Style::default().fg(GOLD)))))
                    .chain([
                        Cell::from(Span::styled(mt, Style::default().fg(mc))),
                        Cell::from(Span::styled(grp(d.purchases), Style::default().fg(BONE))),
                        Cell::from(Span::styled(usd(d.purchased), Style::default().fg(GOLD))),
                        Cell::from(Span::styled(grp(d.users), Style::default().fg(BONE))),
                        Cell::from(Span::styled(grp(d.peak_1m), Style::default().fg(SAGE))),
                        Cell::from(Span::styled(grp(d.peak_clients), Style::default().fg(SAGE))),
                    ])
                    .collect::<Vec<Cell>>(),
                )
            })
            .collect()
    };
    let mut widths = vec![
        Constraint::Length(12), // day
        Constraint::Length(8),  // prompts
        Constraint::Length(8),  // spent
        Constraint::Length(8),  // cost $
    ];
    if fx.is_some() {
        widths.push(Constraint::Length(8)); // cost €
    }
    widths.extend([
        Constraint::Length(8), // margin
        Constraint::Length(5), // buys
        Constraint::Length(8), // buys $
        Constraint::Length(6), // users
        Constraint::Length(6), // 1 min
        Constraint::Length(5), // peak
    ]);
    let mut ts = TableState::default();
    if !m.daily.is_empty() {
        ts.select(Some(view.sel.min(m.daily.len() - 1)));
    }
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header_row)
            .highlight_style(Style::default().bg(Color::Rgb(38, 34, 32)).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ")
            .block(block(&format!("DAILY · totals · per {} day", m.metrics_tz))),
        bot[0],
        &mut ts,
    );

    // the selected day, one block per provider, models underneath, month-to-date at the foot
    let sel_day = m.daily.get(view.sel.min(m.daily.len().saturating_sub(1)));
    let mut drill: Vec<Line> = Vec::new();
    let col = |a: &str, b: &str, c: &str, d: &str, e: &str| {
        if fx.is_some() {
            format!("{a:<22}{b:>7} {c:>10} {d:>10} {e:>9}")
        } else {
            format!("{a:<22}{b:>7} {c:>10} {d:>10}")
        }
    };
    drill.push(Line::from(Span::styled(col("", "prompts", "spent", "cost", "cost €") + "   margin", Style::default().fg(DIM))));
    match sel_day {
        None => drill.push(Line::from(Span::styled("no day selected", Style::default().fg(DIM)))),
        Some(d) if d.per_model.is_empty() => {
            drill.push(Line::from(Span::styled("no per-model figures for this day (older server)", Style::default().fg(DIM))))
        }
        Some(d) => {
            let mut providers: Vec<&str> = d.per_model.keys().map(|k| provider_of(k)).collect();
            providers.sort();
            providers.dedup();
            for prov in providers {
                let mut models: Vec<(&String, &ModelDay)> = d.per_model.iter().filter(|(k, _)| provider_of(k) == prov).collect();
                models.sort_by(|a, b| b.1.spent.cmp(&a.1.spent).then(a.0.cmp(b.0)));
                let (p, sp, co) = models.iter().fold((0, 0, 0), |acc, (_, v)| (acc.0 + v.prompts, acc.1 + v.spent, acc.2 + v.cost));
                let ce = match (prov, fx) {
                    ("GOOGLE", Some(r)) => eur(co, r),
                    _ => "—".into(),
                };
                let (mt, mc) = margin_txt(sp, co);
                drill.push(Line::from(vec![
                    Span::styled(col(prov, &grp(p), &usd4(sp), &usd4(co), &ce), Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("   {mt}"), Style::default().fg(mc)),
                ]));
                for (id, v) in models {
                    let ce = match (prov, fx) {
                        ("GOOGLE", Some(r)) => eur(v.cost, r),
                        _ => "—".into(),
                    };
                    let (mt, mc) = margin_txt(v.spent, v.cost);
                    let label: String = model_label(id).chars().take(19).collect();
                    let dim = v.prompts == 0;
                    drill.push(Line::from(vec![
                        Span::styled(
                            col(&format!("  {label}"), &grp(v.prompts), &usd4(v.spent), &usd4(v.cost), &ce),
                            Style::default().fg(if dim { DIM } else { BONE }),
                        ),
                        Span::styled(format!("   {mt}"), Style::default().fg(if dim { DIM } else { mc })),
                    ]));
                }
            }
            // month to date per provider — the figures to reconcile with the invoices
            let month = &d.day[..d.day.len().min(7)];
            let mut mtd: Vec<(&str, u64, u64)> = Vec::new();
            for row in m.daily.iter().filter(|r| r.day.starts_with(month)) {
                for (id, v) in &row.per_model {
                    let prov = provider_of(id);
                    match mtd.iter_mut().find(|e| e.0 == prov) {
                        Some(e) => {
                            e.1 += v.spent;
                            e.2 += v.cost;
                        }
                        None => mtd.push((prov, v.spent, v.cost)),
                    }
                }
            }
            mtd.sort();
            drill.push(Line::from(""));
            drill.push(Line::from(Span::styled(format!("{month} to date · spent / cost   (4 decimals; totals on the left round to cents)"), Style::default().fg(DIM))));
            for (prov, sp, co) in mtd {
                let extra = match (prov, fx) {
                    ("GOOGLE", Some(r)) => format!(" ≈ {}  ← AI Studio (Pacific days)", eur(co, r)),
                    ("OPENAI", _) => "  ← OpenAI usage (UTC days)".to_string(),
                    _ => String::new(),
                };
                drill.push(Line::from(vec![
                    Span::styled(format!("{prov:<8}"), Style::default().fg(GOLD)),
                    Span::styled(format!("{} / {}", usd4(sp), usd4(co)), Style::default().fg(BONE)),
                    Span::styled(extra, Style::default().fg(DIM)),
                ]));
            }
        }
    }
    let drill_title = sel_day.map(|d| format!("{} · by provider and model", d.day)).unwrap_or_else(|| "by provider and model".into());
    f.render_widget(Paragraph::new(drill).block(block(&drill_title)), bot[1]);

    // strip: the one-line summaries of what used to be two side panels
    let mut strip: Vec<Span> = vec![Span::styled(" INTEGRITY ", Style::default().fg(GOLD).add_modifier(Modifier::BOLD))];
    strip.push(Span::styled(
        format!(
            "burned {} · offenders {} · blacklisted {}",
            grp(m.coins_redeemed),
            m.offenders,
            m.blacklisted
        ),
        Style::default().fg(if m.offenders > 0 || m.blacklisted > 0 { RUST } else { DIM }),
    ));
    if m.has_faucet {
        strip.push(Span::styled("   FAUCET ", Style::default().fg(GOLD).add_modifier(Modifier::BOLD)));
        strip.push(Span::styled(
            format!("{} funded · today {}/{} · {} open", m.faucet_claims, m.faucet_today, m.faucet_daily_max, m.testnet_pending),
            Style::default().fg(if m.faucet_today >= m.faucet_daily_max { RUST } else { DIM }),
        ));
    }
    strip.push(Span::styled("   i details", Style::default().fg(BONE)));
    f.render_widget(Paragraph::new(Line::from(strip)), root[3]);

    // drawer (i): the full INTEGRITY + FAUCET panels as a popup over the bottom row
    if view.drawer {
        let area = root[2];
        let w = area.width.min(96);
        let h = area.height.min(22);
        let pop = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
        f.render_widget(Clear, pop);
        let cols = if m.has_faucet {
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(pop)
        } else {
            Layout::horizontal([Constraint::Percentage(100)]).split(pop)
        };
        let integ = vec![
            kv("burned coin serials", grp(m.coins_redeemed), BONE),
            kv("offenders", grp(m.offenders as u64), if m.offenders > 0 { RUST } else { SAGE }),
            kv("blacklisted", grp(m.blacklisted as u64), if m.blacklisted > 0 { RUST } else { SAGE }),
            Line::from(""),
            kv("lifetime prompts", grp(m.total_prompts), SAGE),
            kv("lifetime spend", usd(m.total_spent), GOLD),
            kv("lifetime buys", format!("{} · {}", grp(m.total_purchases), usd(m.total_purchased)), GOLD),
            Line::from(""),
            Line::from(Span::styled("i or Esc closes", Style::default().fg(DIM))),
        ];
        f.render_widget(Paragraph::new(integ).block(block("INTEGRITY · double-spend")), cols[0]);
        if m.has_faucet {
            let mut lines: Vec<Line> = vec![
                kv("funded", format!("{} · {:.1} NYM", m.faucet_claims, m.faucet_unym as f64 / 1e6), GOLD),
                kv(
                    "today",
                    format!("{} / {}{}", m.faucet_today, m.faucet_daily_max, if m.faucet_today >= m.faucet_daily_max { "  LIMIT — raise FAUCET_DAILY_MAX" } else { "" }),
                    if m.faucet_today >= m.faucet_daily_max { RUST } else if m.faucet_today > 0 { GOLD } else { DIM },
                ),
                kv("open invoices", grp(m.testnet_pending as u64), if m.testnet_pending > 0 { GOLD } else { DIM }),
                Line::from(Span::styled("codes · uses left · note", Style::default().fg(DIM))),
            ];
            let avail = cols[1].height.saturating_sub(2 + lines.len() as u16 + 1) as usize;
            for c in m.faucet_codes.iter().take(avail.max(1)) {
                let left = c.left();
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<16}", c.code), Style::default().fg(if left > 0 { BONE } else { DIM })),
                    Span::styled(format!("{:>2}  ", left), Style::default().fg(if left > 0 { SAGE } else { DIM })),
                    Span::styled(c.note.chars().take(12).collect::<String>(), Style::default().fg(DIM)),
                ]));
            }
            if m.faucet_codes.len() > avail.max(1) {
                lines.push(Line::from(Span::styled(format!("… {} more (scrai-faucet code list)", m.faucet_codes.len() - avail.max(1)), Style::default().fg(DIM))));
            }
            lines.push(Line::from(Span::styled("c = new code (1 claim)", Style::default().fg(GOLD))));
            f.render_widget(Paragraph::new(lines).block(block("FAUCET · testnet")), cols[1]);
        }
    }

    // footer
    let note = Line::from(Span::styled(
        "accounts = distinct paying pubkeys (anonymous) · sessions are unlinkable to accounts · spend & prompts are real daily counters, started at metrics deploy · peak = most clients with a chat/purchase/catalog request in flight at one instant · burned-serial count is integrity only, not a $ value",
        Style::default().fg(DIM),
    ));
    f.render_widget(Paragraph::new(note), root[4]);
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 / whole as f64).clamp(0.0, 1.0)
    }
}

fn clock_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = secs % 86_400;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

fn main() -> io::Result<()> {
    let path: PathBuf = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        let data = scrai_server::cfg("DATA").unwrap_or_else(|_| "./data".into());
        PathBuf::from(data).join("state.db")
    });
    let path_str = path.display().to_string();

    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let res = run(&mut term, &path, &path_str);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}

/// `c`: mint an invite code into faucet.db (next to state.db). Shown in the status line
/// so it can be copied; also listed in the FAUCET panel afterwards.
fn mint_invite_code(state_db: &Path) -> String {
    let Some(dir) = state_db.parent() else { return "no data dir".into() };
    let fdb = dir.join("faucet.db");
    match scrai_server::faucet::open_db(&fdb).and_then(|c| scrai_server::faucet::mint(&c, scrai_server::faucet::DEFAULT_CODE_USES, "admin")) {
        Ok(code) => format!("invite code {code} (1 claim) — hand it to a tester"),
        Err(e) => format!("could not mint a code: {e}"),
    }
}

fn run<B: Backend>(term: &mut Terminal<B>, path: &PathBuf, path_str: &str) -> io::Result<()> {
    let mut status = String::new();
    let mut view = View::default();
    loop {
        let m = read_metrics(path);
        let clock = clock_utc();
        let network = current_network();
        term.draw(|f| ui(f, &m, &view, path_str, &clock, &network, &status))?;
        if event::poll(Duration::from_millis(1500))? {
            if let Event::Key(k) = event::read()? {
                let last = m.daily.len().saturating_sub(1);
                match k.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Esc if view.drawer => view.drawer = false,
                    KeyCode::Esc => return Ok(()),
                    KeyCode::Char('i') => view.drawer = !view.drawer,
                    KeyCode::Down | KeyCode::Char('j') => view.sel = (view.sel + 1).min(last),
                    KeyCode::Up | KeyCode::Char('k') => view.sel = view.sel.saturating_sub(1),
                    KeyCode::Home => view.sel = 0,
                    KeyCode::End => view.sel = last,
                    KeyCode::Char('n') => status = toggle_network(),
                    KeyCode::Char('c') => status = mint_invite_code(path),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_toggle_flips_only_managed_lines() {
        let env = "\
# a comment
FOO=bar
GEMINI_API_KEY_MAINNET=abc
# GEMINI_API_KEY_TESTNET=def
NYX_LCD_URL_MAINNET=https://main
#   NYX_LCD_URL_TESTNET=https://test
OTHER=1
";
        assert_eq!(detect_network(env), "mainnet");

        let t = rewrite_network(env, "testnet");
        assert_eq!(detect_network(&t), "testnet");
        assert!(t.contains("# GEMINI_API_KEY_MAINNET=abc"));
        assert!(t.contains("\nGEMINI_API_KEY_TESTNET=def"));
        assert!(t.contains("# NYX_LCD_URL_MAINNET=https://main"));
        assert!(t.contains("\nNYX_LCD_URL_TESTNET=https://test"));
        // untouched non-managed lines
        assert!(t.contains("\nFOO=bar"));
        assert!(t.contains("\nOTHER=1"));
        assert!(t.contains("# a comment"));

        // round-trips cleanly
        let m = rewrite_network(&t, "mainnet");
        assert_eq!(detect_network(&m), "mainnet");
        assert!(m.contains("\nGEMINI_API_KEY_MAINNET=abc"));
        assert!(m.contains("# GEMINI_API_KEY_TESTNET=def"));
    }

    #[test]
    fn managed_suffix_ignores_lookalikes() {
        assert_eq!(managed_suffix("GEMINI_API_KEY_MAINNET=x"), Some("mainnet"));
        assert_eq!(managed_suffix("# NYX_LCD_URL_TESTNET=y"), Some("testnet"));
        assert_eq!(managed_suffix("GEMINI_API_KEY=legacy"), None); // unsuffixed = not managed
        assert_eq!(managed_suffix("GATEWAY_MAINNET=z"), None); // not in the managed set
        assert_eq!(managed_suffix("NOTES_MAINNET is a sentence"), None); // no '='
    }
}

#[cfg(test)]
mod header_tests {
    use super::*;
    #[test]
    fn model_headers_wrap_and_fall_back() {
        assert_eq!(model_header("gemini-3.1-flash-lite-image"), "Nano Banana\n2 Lite");
        assert_eq!(model_header("gemini-3.5-flash-lite"), "Gemini 3.5\nFlash-Lite");
        assert_eq!(two_lines("Nano Banana 2", 12), "Nano Banana\n2");
        assert_eq!(model_header("gemini-9-imaginary"), "9 imaginary");
        assert_eq!(model_label("gemini-3.1-flash-lite-image"), "Nano Banana 2 Lite");
    }

    #[test]
    fn providers_by_model_prefix() {
        assert_eq!(provider_of("gemini-3.6-flash"), "GOOGLE");
        assert_eq!(provider_of("imagen-4"), "GOOGLE");
        assert_eq!(provider_of("gpt-5.4-mini"), "OPENAI");
        assert_eq!(provider_of("mistral-large"), "OTHER");
    }
}
