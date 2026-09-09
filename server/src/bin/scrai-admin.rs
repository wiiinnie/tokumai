// scrai-admin — a read-only, htop-style dashboard over the server's state.db.
//
// Run it on the box (SSH): `scrai-admin [path/to/state.db]`. It opens the DB READ-ONLY
// (never writes, safe alongside a live server), aggregates the JSON blobs the server
// snapshots (sessions / pay / quorum) plus the per-day `daily` counters, and refreshes
// like htop. Aggregate-only: no account/session ids, no message content — the server
// never stores those anyway.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use scrai_server::admin::*;

/// What the operator is looking at: the selected day row and whether the drawer with the
/// testnet faucet + integrity details is open.
#[derive(Default)]
struct View {
    sel: usize,
    drawer: bool,
    refund: Refund,
}

impl Default for Refund {
    fn default() -> Self {
        Refund::Off
    }
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
        Span::styled("   ↑↓ day · i details · n toggle · c code · v refund · q quit", Style::default().fg(DIM)),
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
        // Where the money came from. Only the first figure counts towards a cross-border
        // EU threshold; the rest is context. "unknown" is every coin payment — a chain
        // reports no country and we deliberately do not ask the buyer for one.
        kv(
            "-> by origin",
            format!(
                "EU ${} · DE ${} · other ${} · unknown ${}",
                grp(m.eu_usd),
                grp(m.de_usd),
                grp(m.row_usd),
                grp(m.unknown_usd)
            ),
            if m.eu_usd > 0 { GOLD } else { DIM },
        ),
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
            format!(
                "{} different paying sessions chatted · {} in the busiest hour",
                grp(m.daily.first().map(|d| d.users).unwrap_or(0)),
                grp(m.daily.first().map(|d| d.peak_1h).unwrap_or(0))
            ),
            BONE,
        ),
        // Load, not people: requests being served at one instant. It counts every kind of
        // request, so it is the number that says whether the Nym client and the provider
        // slots are getting tight — and it is deliberately NOT next to the user counts,
        // because reading it as "how many people" is exactly the mistake it invites.
        kv(
            "inflight (load)",
            format!(
                "{} at once today · {} all-time",
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
    header.extend(["margin", "buys", "buys $", "users", "1 h", "inflight"]);
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
                        Cell::from(Span::styled(grp(d.peak_1h), Style::default().fg(SAGE))),
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
        Constraint::Length(5), // 1 h
        Constraint::Length(8), // inflight
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

    // refund (v): its own popup, over everything. Drawn last so it wins.
    if !matches!(view.refund, Refund::Off) {
        let area = f.area();
        let w = area.width.min(88);
        let h = area.height.min(20);
        let pop = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
        f.render_widget(Clear, pop);
        f.render_widget(Paragraph::new(refund_lines(&view.refund)).block(block("REFUND · void an unredeemed code")), pop);
        return;
    }

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


fn main() -> io::Result<()> {
    // The admin has never needed .env — it only read state.db. The refund screen does: a
    // pasted code is fingerprinted under VOUCHER_KEY, and without the key it would be
    // hashed the old way and never found. Same load the faucet does.
    let _ = dotenvy::from_path(scrai_server::env_file());
    let path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| scrai_server::data_dir().join("state.db"));
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
// ---------------------------------------------------------------------------
// refunds (v): find a purchase, see whether its credit is still untouched, void it
//
// The rule this screen exists to enforce: a receipt number IDENTIFIES a purchase, it does
// not AUTHORISE anything — it is printed on a document that can be photographed, and it is
// the only thing a stranger would have. So the void asks for what the buyer showed, and
// records it. What cannot be got wrong by mistake is the money: a refund always goes back
// the way it came (Mollie to the original method, NYM to the sending address, which is on
// the chain under the memo), never to an address somebody named in a support message.
// ---------------------------------------------------------------------------

/// Where the refund screen is. Deliberately several steps: this destroys a buyer's credit,
/// and the evidence line is the only record of why we believed the person asking.
enum Refund {
    Off,
    Ask(String),
    Pick(Vec<Hit>, usize),
    Evidence(Box<Hit>, String),
    Confirm(Box<Hit>, String),
    Done(String),
}


/// One keystroke on the refund screen. Returns a status line when something happened.
///
/// The steps are not ceremony. Between "I found the purchase" and "the buyer's credit is
/// gone" sit two deliberate acts: naming what they showed, and choosing why. Both end up in
/// `refunds.csv`, because the question somebody will ask months later is not WHETHER we
/// voided a code but why we believed the person asking for it.
/// What one purchase looks like on the refund screen. Everything a decision needs, and the
/// verdict spelled out rather than left to be inferred from a timestamp.
fn hit_lines(h: &Hit) -> Vec<Line<'static>> {
    let (verdict, colour) = match h.refundable() {
        Ok(s) => (s, SAGE),
        Err(s) => (s, RUST),
    };
    let money = if h.method == "card" {
        format!("Mollie {}", h.provider_ref)
    } else {
        format!("memo {} — the sender is on the chain", h.provider_ref)
    };
    vec![
        kv("receipt", h.receipt.clone(), BONE),
        kv("invoice", h.invoice.clone(), DIM),
        kv("paid", format!("{} UTC", scrai_server::pay::utc_stamp(h.paid_at)), BONE),
        kv("amount", format!("${} · {} TOKU", h.usd, grp(h.toku)), GOLD),
        kv("rail", format!("{} · {} · consent {}", h.method, h.country, h.consent), BONE),
        kv("find the money", money, DIM),
        kv("kind", if h.is_voucher { "code bought on the website".into() } else { "in-app purchase".to_string() }, BONE),
        Line::from(""),
        Line::from(Span::styled(verdict, Style::default().fg(colour))),
    ]
}

fn refund_lines(r: &Refund) -> Vec<Line<'static>> {
    let hint = |s: &str| Line::from(Span::styled(s.to_string(), Style::default().fg(DIM)));
    match r {
        Refund::Off => Vec::new(),
        Refund::Ask(q) => vec![
            Line::from("Receipt number, invoice id, the code itself, or a payment reference:"),
            Line::from(""),
            Line::from(Span::styled(format!("  {q}_"), Style::default().fg(BONE))),
            Line::from(""),
            hint("The code is the strongest thing a buyer can show — holding it is what being"),
            hint("entitled to it means. A receipt number identifies a purchase but proves nothing:"),
            hint("it is printed on a document anyone could have photographed."),
            Line::from(""),
            hint("Enter searches · Esc closes"),
        ],
        Refund::Pick(hits, sel) => {
            let mut v = vec![
                Line::from(format!("{} purchases share that receipt prefix — pick one:", hits.len())),
                Line::from(""),
            ];
            for (i, h) in hits.iter().enumerate() {
                let mark = if i == *sel { "▸ " } else { "  " };
                v.push(Line::from(Span::styled(
                    format!("{mark}{} · ${} · {} · {}", h.receipt, h.usd, h.method, scrai_server::pay::utc_stamp(h.paid_at)),
                    Style::default().fg(if i == *sel { BONE } else { DIM }),
                )));
            }
            v.push(Line::from(""));
            v.push(hint("↑↓ choose · Enter opens · Esc closes"));
            v
        }
        Refund::Evidence(h, text) => {
            let mut v = hit_lines(h);
            v.push(Line::from(""));
            if h.refundable().is_err() {
                v.push(hint("Nothing to void here. Esc closes."));
                return v;
            }
            v.push(Line::from("What did the buyer show to prove this purchase is theirs?"));
            v.push(Line::from(Span::styled(format!("  {text}_"), Style::default().fg(BONE))));
            v.push(hint("e.g. \"pasted the code\", \"Mollie tr_… + cardholder\", \"chain tx from bech32…\""));
            v.push(hint("Enter continues · Esc closes"));
            v
        }
        Refund::Confirm(h, evidence) => {
            let mut v = hit_lines(h);
            v.push(Line::from(""));
            v.push(kv("evidence", evidence.clone(), BONE));
            v.push(Line::from(""));
            v.push(Line::from(Span::styled(
                "This destroys the buyer's code. Refund the money the way it came — never to an",
                Style::default().fg(RUST),
            )));
            v.push(Line::from(Span::styled(
                "address someone named in a message.",
                Style::default().fg(RUST),
            )));
            v.push(Line::from(""));
            v.push(hint("t = technical (statutory) · g = goodwill · Esc cancels"));
            v
        }
        Refund::Done(msg) => vec![Line::from(msg.clone()), Line::from(""), hint("Esc closes")],
    }
}

fn refund_key(r: &mut Refund, k: KeyCode, state_db: &Path) -> Option<String> {
    match r {
        Refund::Off => None,
        Refund::Ask(q) => match k {
            KeyCode::Esc => {
                *r = Refund::Off;
                None
            }
            KeyCode::Backspace => {
                q.pop();
                None
            }
            KeyCode::Char(c) if q.len() < 64 => {
                q.push(c);
                None
            }
            KeyCode::Enter => {
                let hits = find_purchase(state_db, q);
                *r = match hits.len() {
                    0 => Refund::Done("nothing matches that receipt, invoice, code or payment reference".into()),
                    1 => Refund::Evidence(Box::new(hits.into_iter().next()?), String::new()),
                    _ => Refund::Pick(hits, 0),
                };
                None
            }
            _ => None,
        },
        // More than one match means a receipt-number PREFIX collided (8 hex = 32 bits, so
        // this happens eventually). Never guess — the operator picks.
        Refund::Pick(hits, sel) => match k {
            KeyCode::Esc => {
                *r = Refund::Off;
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *sel = (*sel + 1).min(hits.len().saturating_sub(1));
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *sel = sel.saturating_sub(1);
                None
            }
            KeyCode::Enter => {
                let hit = hits.remove(*sel);
                *r = Refund::Evidence(Box::new(hit), String::new());
                None
            }
            _ => None,
        },
        Refund::Evidence(hit, text) => match k {
            KeyCode::Esc => {
                *r = Refund::Off;
                None
            }
            KeyCode::Backspace => {
                text.pop();
                None
            }
            KeyCode::Char(c) if text.len() < 60 => {
                text.push(c);
                None
            }
            // No evidence, no void. The receipt number alone is on a piece of paper anyone
            // could be holding, so it can never be the whole basis for destroying credit.
            KeyCode::Enter if text.trim().is_empty() => {
                Some("say what the buyer showed — a code, a Mollie payment, a chain transfer".into())
            }
            KeyCode::Enter => {
                *r = Refund::Confirm(hit.clone(), text.clone());
                None
            }
            _ => None,
        },
        Refund::Confirm(hit, evidence) => match k {
            KeyCode::Esc => {
                *r = Refund::Off;
                None
            }
            KeyCode::Char('t') | KeyCode::Char('g') => {
                let reason = if matches!(k, KeyCode::Char('t')) { "technical" } else { "goodwill" };
                let msg = do_void(state_db, hit, reason, evidence);
                *r = Refund::Done(msg.clone());
                Some(msg)
            }
            _ => None,
        },
        Refund::Done(_) => {
            if matches!(k, KeyCode::Esc | KeyCode::Enter) {
                *r = Refund::Off;
            }
            None
        }
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
                // The refund screen takes EVERY key while it is open: it has a text field,
                // and a stray "q" in the middle of typing a receipt number must not quit.
                if !matches!(view.refund, Refund::Off) {
                    if let Some(msg) = refund_key(&mut view.refund, k.code, path) {
                        status = msg;
                    }
                    continue;
                }
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
                    KeyCode::Char('v') => view.refund = Refund::Ask(String::new()),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(is_voucher: bool, voucher: Option<(u64, Option<u64>, Option<u64>)>, ent: Option<u64>) -> Hit {
        Hit {
            invoice: "abc123".into(), receipt: "TKM-2026-ABC123".into(), usd: 5, toku: 500_000,
            paid_at: 1_757_000_000_000, method: "card".into(), country: "DE".into(),
            consent: "2026-09-07".into(), provider_ref: "tr_x".into(), status: "paid".into(),
            is_voucher, voucher, entitlement: ent, hash: String::new(),
        }
    }

    /// The one decision the refund screen exists to make, and the asymmetry behind it: a
    /// voucher's state is knowable, an in-app purchase's is only knowable while the credit
    /// has not been withdrawn into blind-signed coins.
    #[test]
    fn only_an_unredeemed_code_can_actually_be_voided() {
        // The good case: bought on the website, code never entered anywhere.
        assert!(hit(true, Some((500_000, None, None)), None).refundable().is_ok());

        // Redeemed — the credit has left as coins nobody can trace, ours included.
        let spent = hit(true, Some((500_000, Some(1_757_000_100_000), None)), None);
        assert!(spent.refundable().unwrap_err().contains("REDEEMED"));

        // Already refunded once.
        let void = hit(true, Some((500_000, None, Some(1_757_000_100_000))), None);
        assert!(void.refundable().unwrap_err().contains("already voided"));

        // Paid, but the buyer never asked for the code.
        assert!(hit(true, None, None).refundable().unwrap_err().contains("no code"));

        // In-app, credit still sitting un-withdrawn: provably unspent, but there is nothing
        // to invalidate — the refund is money out, and the buyer keeps the credit.
        let held = hit(false, None, Some(500_000)).refundable().expect("un-withdrawn is answerable");
        assert!(held.contains("nothing to void"), "{held}");

        // In-app, already withdrawn: unknowable.
        assert!(hit(false, None, Some(0)).refundable().is_err());

        // In-app past the fourteen-day scrub: we cannot even find the account any more.
        assert!(hit(false, None, None).refundable().unwrap_err().contains("account link has expired"));

        // Nothing was ever charged.
        let mut unpaid = hit(true, Some((500_000, None, None)), None);
        unpaid.status = "pending".into();
        assert!(unpaid.refundable().unwrap_err().contains("nothing was ever charged"));
    }

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
