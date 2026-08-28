// scrai-admin — a read-only, htop-style dashboard over the server's state.db.
//
// Run it on the box (SSH): `scrai-admin [path/to/state.db]`. It opens the DB READ-ONLY
// (never writes, safe alongside a live server), aggregates the JSON blobs the server
// snapshots (sessions / pay / quorum) plus the per-day `daily` counters, and refreshes
// like htop. Aggregate-only: no account/session ids, no message content — the server
// never stores those anyway.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

const SCRAI_PER_USD: u64 = 100_000; // one coconut coin = $1 = 100_000 SCRAI

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
    amount_scrai: u64,
    #[serde(default)]
    status: String,
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
    purchased_scrai: u64,
    purchased_usd: u64,
    entitlement_out: u64,
    withdrawn_scrai: u64,
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
    // live-grounding queries used this UTC month (Gemini's 5,000/mo free allowance)
    grounding_used: u64,
}

/// Gemini's monthly free Grounding allowance — mirror of chat::GROUNDING_FREE_PER_MONTH.
const GROUNDING_FREE_PER_MONTH: u64 = 5000;

/// Current UTC month as `YYYY-MM` (same civil_from_days math as the server's today_utc,
/// so we need no date crate) — picks the right `grounding:<month>` counter key.
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
    std::env::var("SCRAI_ENV_FILE").unwrap_or_else(|_| "/opt/scrai/.env".into())
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
    let quo: QuorumBlob = blob("quorum").and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();

    // economy
    let mut payers: HashSet<&str> = HashSet::new();
    for inv in pay.invoices.values() {
        match inv.status.as_str() {
            "paid" => {
                m.inv_paid += 1;
                m.purchased_scrai += inv.amount_scrai;
                m.purchased_usd += inv.amount_usd as u64;
                if !inv.account_id.is_empty() {
                    payers.insert(inv.account_id.as_str());
                }
            }
            "expired" => m.inv_expired += 1,
            _ => m.inv_pending += 1,
        }
    }
    for a in pay.entitlements.keys() {
        payers.insert(a.as_str());
    }
    m.paying_accounts = payers.len();
    m.entitlement_out = pay.entitlements.values().sum();
    m.withdrawn_scrai = m.purchased_scrai.saturating_sub(m.entitlement_out);

    // usage
    m.sessions = sess.sessions.len();
    m.session_balance = sess.sessions.values().map(|s| s.balance).sum();
    m.charges = sess.sessions.values().map(|s| s.counter).sum();
    // NOTE: coins_redeemed is the count of burned ecash SERIALS, an integrity
    // number only — a nym ticketbook holds many tiny coins, so a serial is NOT
    // a $1 coin. Do not dollarize it. Real spend comes from the daily counters.
    m.coins_redeemed = quo.serials.len() as u64;

    // integrity
    m.offenders = quo.offenses.len();
    m.blacklisted = quo.blacklist.len();

    // per-day metrics table (may not exist on an un-migrated server)
    if let Ok(mut stmt) = conn.prepare(
        "SELECT day, prompts, spent, purchases, purchased, cost FROM daily ORDER BY day DESC LIMIT 12",
    ) {
        m.has_daily = true;
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok(DayRow {
                day: r.get(0)?,
                prompts: r.get::<_, i64>(1)? as u64,
                spent: r.get::<_, i64>(2)? as u64,
                purchases: r.get::<_, i64>(3)? as u64,
                purchased: r.get::<_, i64>(4)? as u64,
                cost: r.get::<_, i64>(5)? as u64,
            })
        }) {
            m.daily = rows.filter_map(|r| r.ok()).collect();
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
    format!("${:.2}", scrai as f64 / SCRAI_PER_USD as f64)
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

fn ui(f: &mut Frame, m: &Metrics, path: &str, clock: &str, network: &str, status: &str) {
    let root = Layout::vertical([
        Constraint::Length(1),
        // 11 → inner 9 → 8 content lines + gauge, so USAGE's grounding line fits (the
        // bottom row has ample empty space to give up).
        Constraint::Length(11),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .split(f.area());

    // header
    let header = Line::from(vec![
        Span::styled("Scramble", Style::default().fg(BONE).add_modifier(Modifier::BOLD)),
        Span::styled("AI", Style::default().fg(SAGE).add_modifier(Modifier::BOLD)),
        Span::styled("  server admin", Style::default().fg(DIM)),
        Span::styled(format!("   {path}"), Style::default().fg(DIM)),
        Span::styled(format!("   {clock} UTC"), Style::default().fg(GOLD)),
        Span::styled(
            format!("   net:{network}"),
            Style::default().fg(if network == "mainnet" { RUST } else { SAGE }).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   n toggle · q quit", Style::default().fg(DIM)),
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
        kv("purchased", format!("{}  ({} scrai)", usd(m.purchased_scrai), grp(m.purchased_scrai)), GOLD),
        kv("entitlement open", format!("{}  ({} scrai)", usd(m.entitlement_out), grp(m.entitlement_out)), BONE),
        kv("-> withdrawn ecash", format!("{}  ({} scrai)", usd(m.withdrawn_scrai), grp(m.withdrawn_scrai)), SAGE),
    ];
    let eb = block("ECONOMY · money in");
    let ei = eb.inner(top[0]);
    f.render_widget(eb, top[0]);
    let er = Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).split(ei);
    f.render_widget(Paragraph::new(econ), er[0]);
    let g1 = ratio(m.withdrawn_scrai, m.purchased_scrai);
    f.render_widget(
        Gauge::default().gauge_style(Style::default().fg(SAGE)).ratio(g1).label(format!("withdrawn {:.0}%", g1 * 100.0)),
        er[1],
    );

    let usage = vec![
        kv("chat prompts (total)", grp(m.total_prompts), SAGE),
        kv("active sessions", grp(m.sessions as u64), BONE),
        kv("session credit", format!("{}  ({} scrai)", usd(m.session_balance), grp(m.session_balance)), GOLD),
        kv("chat revenue", format!("{}  ({} scrai)", usd(m.total_spent), grp(m.total_spent)), GOLD),
        kv("provider cost", format!("-{}  ({} scrai)", usd(m.total_cost), grp(m.total_cost)), BONE),
        kv("= profit", format!("{}  ({} scrai)", usd(m.total_spent.saturating_sub(m.total_cost)), grp(m.total_spent.saturating_sub(m.total_cost))), SAGE),
        kv("  since metrics deploy", String::new(), DIM),
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
    let g2 = ratio(m.total_spent, m.purchased_scrai);
    f.render_widget(
        Gauge::default().gauge_style(Style::default().fg(GOLD)).ratio(g2).label(format!("spent of purchased {:.0}%", g2 * 100.0)),
        ur[1],
    );

    // bottom row: DAILY | INTEGRITY
    let bot = Layout::horizontal([Constraint::Min(40), Constraint::Length(30)]).split(root[2]);

    let header_row = Row::new(vec!["day", "prompts", "spent", "cost", "margin", "buys", "buys $"])
        .style(Style::default().fg(DIM));
    let rows: Vec<Row> = if m.daily.is_empty() {
        vec![Row::new(vec![Cell::from(Span::styled(
            if m.has_daily { "no activity yet today" } else { "server not yet redeployed with metrics" },
            Style::default().fg(DIM),
        ))])]
    } else {
        m.daily
            .iter()
            .map(|d| {
                Row::new(vec![
                    Cell::from(Span::styled(d.day.clone(), Style::default().fg(BONE))),
                    Cell::from(Span::styled(grp(d.prompts), Style::default().fg(SAGE))),
                    Cell::from(Span::styled(usd(d.spent), Style::default().fg(GOLD))),
                    Cell::from(Span::styled(usd(d.cost), Style::default().fg(BONE))),
                    Cell::from(Span::styled(
                        if d.cost > 0 { format!("+{:.1}%", (d.spent as f64 / d.cost as f64 - 1.0) * 100.0) } else { "—".into() },
                        Style::default().fg(SAGE),
                    )),
                    Cell::from(Span::styled(grp(d.purchases), Style::default().fg(BONE))),
                    Cell::from(Span::styled(usd(d.purchased), Style::default().fg(GOLD))),
                ])
            })
            .collect()
    };
    let widths = [
        Constraint::Length(12),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(9),
    ];
    f.render_widget(
        Table::new(rows, widths).header(header_row).block(block("DAILY · per UTC day")),
        bot[0],
    );

    let integ = vec![
        kv("burned coin serials", grp(m.coins_redeemed), BONE),
        kv("offenders", grp(m.offenders as u64), if m.offenders > 0 { RUST } else { SAGE }),
        kv("blacklisted", grp(m.blacklisted as u64), if m.blacklisted > 0 { RUST } else { SAGE }),
        Line::from(""),
        kv("lifetime prompts", grp(m.total_prompts), SAGE),
        kv("lifetime spend", usd(m.total_spent), GOLD),
        kv("lifetime buys", format!("{} · {}", grp(m.total_purchases), usd(m.total_purchased)), GOLD),
    ];
    f.render_widget(Paragraph::new(integ).block(block("INTEGRITY · double-spend")), bot[1]);

    // footer
    let note = Line::from(Span::styled(
        "accounts = distinct paying pubkeys (anonymous) · sessions are unlinkable to accounts · spend & prompts are real daily counters, started at metrics deploy · burned-serial count is integrity only, not a $ value",
        Style::default().fg(DIM),
    ));
    f.render_widget(Paragraph::new(note), root[3]);
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
        let data = std::env::var("SCRAI_DATA").unwrap_or_else(|_| "./data".into());
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

fn run<B: Backend>(term: &mut Terminal<B>, path: &PathBuf, path_str: &str) -> io::Result<()> {
    let mut status = String::new();
    loop {
        let m = read_metrics(path);
        let clock = clock_utc();
        let network = current_network();
        term.draw(|f| ui(f, &m, path_str, &clock, &network, &status))?;
        if event::poll(Duration::from_millis(1500))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('n') => status = toggle_network(),
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
        assert_eq!(managed_suffix("SCRAI_GATEWAY_MAINNET=z"), None); // not in the managed set
        assert_eq!(managed_suffix("NOTES_MAINNET is a sentence"), None); // no '='
    }
}
