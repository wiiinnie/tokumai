// ---------------------------------------------------------------------------
// scrai-adminweb — the operator's console, as a page instead of a terminal.
//
// Same numbers and the same three writes as the ratatui admin (both read
// `scrai_server::admin`, so they cannot drift), but a screen has room a terminal row does
// not, and free text — the refund evidence line, a support reply later — stops being
// hand-rolled cursor handling.
//
// THE ONE THING THAT MUST NOT MOVE: this binds to LOOPBACK, always. The box it runs on
// holds `authority.json`, the Coconut issuer key — whoever reaches this reaches a machine
// that can mint unlimited valid credit. There is no login of our own on purpose:
// authentication is the SSH tunnel you came through, and a password form here would be a
// second, weaker way in, written by us. `ADMIN_PORT` moves the port; nothing moves the
// address.
//
//   ssh -N -L 8791:127.0.0.1:8791 <admin>@<vps>   then http://127.0.0.1:8791
// ---------------------------------------------------------------------------

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use scrai_server::admin;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PAGE: &str = include_str!("../../site/admin.html");
const MAX_BODY: usize = 8 * 1024;
const DEFAULT_PORT: u16 = 8791;

/// The port only. Deliberately NOT a full listen address: an `ADMIN_LISTEN=0.0.0.0:8791`
/// in a hurried .env is exactly how this box would end up answering the internet, so the
/// address is not configurable at all.
fn port() -> u16 {
    scrai_server::cfg("ADMIN_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT)
}

struct Req {
    method: String,
    path: String,
    body: Vec<u8>,
}

#[tokio::main]
async fn main() {
    // Stand where the services stand. Their units say WorkingDirectory=/opt/tokumai, and
    // both `DATA` and the `./data` fallback may be RELATIVE — so started by hand from an SSH
    // login (cwd = somebody's home) this would look for the database in the wrong place and
    // exit, while the same binary run by systemd finds it. A console that only works from
    // one directory is a console that fails at the moment it is needed.
    if let Some(root) = scrai_server::install_root() {
        let _ = std::env::set_current_dir(&root);
    }
    let _ = dotenvy::from_path(scrai_server::env_file());
    let db: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| scrai_server::data_dir().join("state.db"));
    if !db.exists() {
        eprintln!("tokumai-adminweb: {} does not exist.", db.display());
        eprintln!("    cwd {}", std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default());
        eprintln!("    Pass the path explicitly if it lives elsewhere:");
        eprintln!("      tokumai-adminweb /opt/tokumai/data/state.db");
        std::process::exit(1);
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port()));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tokumai-adminweb: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("tokumai-adminweb: http://{addr}  ({})", db.display());
    println!("tokumai-adminweb: loopback only — reach it through an SSH tunnel:");
    println!(
        "    ssh -t -L {p}:127.0.0.1:{p} <admin>@<this-host> sudo -u scrai /opt/tokumai/bin/tokumai-adminweb",
        p = port()
    );
    // The `sudo -u scrai` is not decoration. .env and the databases belong to that user, so
    // started as anybody else the console reads fine and every ACTION fails — the network
    // shows "?", minting a code returns a raw sqlite error. The page says so too, but the
    // line somebody copies from should be the one that works.
    let a = access(&db);
    for (what, ok) in [("read .env", a["env"] == true), ("write state.db", a["state"] == true),
                       ("write faucet.db", a["faucet"] == true)] {
        if !ok {
            eprintln!("tokumai-adminweb: cannot {what} as this user — actions will be refused");
        }
    }

    let db = Arc::new(db);
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { continue };
        let db = db.clone();
        tokio::spawn(async move {
            if let Some(req) = read_req(&mut sock).await {
                route(&mut sock, &req, &db).await;
            }
        });
    }
}

async fn read_req(sock: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 16 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_uppercase();
    let path = first.next()?.split('?').next()?.to_string();
    let mut len = 0usize;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if len > MAX_BODY {
        return None;
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Some(Req { method, path, body })
}

async fn send(sock: &mut tokio::net::TcpStream, status: u16, ctype: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.flush().await;
}

async fn reply(sock: &mut tokio::net::TcpStream, v: &Value) {
    send(sock, 200, "application/json", serde_json::to_vec(v).unwrap_or_default().as_slice()).await
}

async fn route(sock: &mut tokio::net::TcpStream, req: &Req, db: &PathBuf) {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => send(sock, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await,

        // Everything the page shows, in one poll. Cheap: it is the same read the TUI does
        // 40 times a minute already.
        ("GET", "/api/state") => reply(sock, &state_json(db)).await,

        // ---- the three writes ------------------------------------------------------
        ("POST", "/api/find") => {
            let hits: Vec<Value> = admin::find_purchase(db, &s("q")).iter().map(hit_json).collect();
            reply(sock, &json!({ "hits": hits })).await
        }
        ("POST", "/api/void") => {
            let (invoice, reason, evidence, hash) = (s("invoice"), s("reason"), s("evidence"), s("hash"));
            // The same two refusals the TUI makes, restated here because a browser is not a
            // trusted caller: a hand-written POST must not be able to skip the evidence line
            // or invent a reason that is not one of the two.
            if evidence.trim().is_empty() {
                reply(sock, &json!({ "error": "say what the buyer showed" })).await;
                return;
            }
            if reason != "technical" && reason != "goodwill" {
                reply(sock, &json!({ "error": "a refund is either technical or goodwill" })).await;
                return;
            }
            let found = if hash.is_empty() {
                admin::find_purchase(db, &invoice).into_iter().find(|h| h.invoice == invoice)
            } else {
                admin::hit_by_hash(db, &hash)
            };
            match found {
                None => reply(sock, &json!({ "error": "no such purchase" })).await,
                Some(hit) => {
                    let msg = admin::do_void(db, &hit, &reason, evidence.trim());
                    reply(sock, &json!({ "message": msg })).await
                }
            }
        }
        ("POST", "/api/code") => reply(sock, &json!({ "message": admin::mint_invite_code(db) })).await,

        // Every code ever issued. Its own route rather than part of /api/state: this grows
        // with the business and the state poll runs every 1.5 s.
        ("GET", "/api/codes") => {
            let items: Vec<Value> = admin::list_issued_codes(db)
                .iter()
                .map(|c| {
                    json!({
                        "kind": c.kind, "label": c.label, "note": c.note, "usd": c.usd,
                        "issued": scrai_server::pay::utc_stamp(c.issued),
                        "uses": c.uses, "maxUses": c.max_uses,
                        "redeemed": if c.redeemed_at > 0 { scrai_server::pay::utc_stamp(c.redeemed_at) } else { String::new() },
                        "void": if c.void_at > 0 { scrai_server::pay::utc_stamp(c.void_at) } else { String::new() },
                        "invoice": c.invoice, "who": c.who, "open": c.open,
                    })
                })
                .collect();
            reply(sock, &json!({ "codes": items })).await
        }
        ("POST", "/api/network") => reply(sock, &json!({ "message": admin::toggle_network() })).await,

        ("GET", _) => send(sock, 404, "text/plain", b"not found").await,
        _ => send(sock, 400, "text/plain", b"bad request").await,
    }
}

fn hit_json(h: &admin::Hit) -> Value {
    let (verdict, ok) = match h.refundable() {
        Ok(v) => (v, true),
        Err(v) => (v, false),
    };
    json!({
        "invoice": h.invoice, "hash": h.hash, "receipt": h.receipt, "usd": h.usd, "toku": h.toku,
        "paid": scrai_server::pay::utc_stamp(h.paid_at),
        "method": h.method, "country": h.country, "consent": h.consent,
        "providerRef": h.provider_ref, "status": h.status, "isVoucher": h.is_voucher,
        "verdict": verdict, "voidable": ok && h.is_voucher,
    })
}

/// Can this process actually do the three things the console offers?
///
/// It runs as whoever opened the SSH session, while `.env` and the databases belong to
/// `scrai` — so the ordinary way to start it is the way that fails, and it fails in three
/// unrelated-looking places: the network reads "?", minting a code says "attempt to write a
/// readonly database", and a void would too. One cause, three cryptic symptoms; the page
/// gets told the cause instead (2026-09-09).
fn access(db: &PathBuf) -> Value {
    let readable = |p: &std::path::Path| std::fs::File::open(p).is_ok();
    let writable = |p: &std::path::Path| std::fs::OpenOptions::new().write(true).open(p).is_ok();
    let env = std::path::PathBuf::from(admin::env_file_path());
    let faucet = db.parent().map(|d| d.join("faucet.db")).unwrap_or_default();
    json!({
        "env": readable(&env), "envPath": env.display().to_string(),
        "state": writable(db),
        "faucet": !faucet.exists() || writable(&faucet),
        "user": std::env::var("USER").unwrap_or_else(|_| "?".into()),
    })
}

fn state_json(db: &PathBuf) -> Value {
    let m = admin::read_metrics(db);
    let days: Vec<Value> = m
        .daily
        .iter()
        .map(|d| {
            json!({
                "day": d.day, "prompts": d.prompts, "spent": d.spent, "cost": d.cost,
                "purchases": d.purchases, "purchased": d.purchased,
                "users": d.users, "peak1h": d.peak_1h, "inflight": d.peak_clients,
                "models": d.per_model.iter().map(|(id, md)| json!({
                    "id": id, "label": admin::model_label(id), "provider": admin::provider_of(id),
                    "prompts": md.prompts, "spent": md.spent, "cost": md.cost,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "ok": m.ok,
        "error": m.err,
        "version": scrai_server::VERSION,
        "db": db.display().to_string(),
        "clock": admin::clock_utc(),
        "network": admin::current_network(),
        "margin": admin::env_file_value("MARGIN").and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.3),
        "access": access(db),
        "usage": {
            "prompts": m.total_prompts, "sessions": m.sessions,
            "sessionCredit": m.session_balance,
            "groundingUsed": m.grounding_used, "groundingFree": admin::GROUNDING_FREE_PER_MONTH,
        },
        "economy": {
            "payingAccounts": m.paying_accounts, "invoices": m.inv_paid + m.inv_pending + m.inv_expired,
            "invoicesPaid": m.inv_paid, "invoicesOpen": m.inv_pending,
            "purchased": m.total_purchased, "entitlement": m.entitlement_out,
            "withdrawn": m.withdrawn_toku, "revenue": m.total_spent, "cost": m.total_cost,
        },
        "integrity": {
            "burned": m.coins_redeemed, "offenders": m.offenders, "blacklisted": m.blacklisted,
            "lifetimeBuys": m.total_purchases,
        },
        "faucet": {
            "on": m.has_faucet, "claims": m.faucet_claims, "unym": m.faucet_unym,
            "today": m.faucet_today, "dailyMax": m.faucet_daily_max,
            "openInvoices": m.testnet_pending,
            "codes": m.faucet_codes.iter().map(|c| json!({
                "code": c.code, "left": c.max_uses.saturating_sub(c.uses), "note": c.note,
            })).collect::<Vec<_>>(),
        },
        "days": days,
    })
}
