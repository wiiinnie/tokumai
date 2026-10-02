//! What the operator may know, and how it is asked.
//!
//! The enclave keeps no record of who asked what. What it can tell its operator is counts:
//! requests and TOKU per hour and per model, plans by tier and rail, notes minted and spent
//! per month, how far the book has got, how it is doing. Nothing here names an account,
//! except the one lookup the operator makes by hand for a support case, with an id the
//! person sent in (`admin.account`).
//!
//! The operator is an account like any other, named in the image (`TOKUMAI_ADMIN_ACCOUNT`):
//! every `admin.*` operation is signed by it and refused for anyone else. The panel that
//! asks these questions is `tokumai-admin` (crates/server), on the operator's own machine,
//! over the mixnet like the app.
//!
//! Counts that must outlive a restart (the daily ones, the notes') go into the book's
//! `tally` table — aggregates under keys like `d:<day>:<model>:<kind>:toku`, flushed from
//! memory on the tick so a chat does not add three journal records of its own. The hourly
//! ring stays in memory: it is for the last two days and a restart may lose it.

use crate::service::{error, Enclave};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

/// How many hours the in-memory ring keeps.
const HOURS_KEPT: usize = 48;
/// How many days of daily tallies an `admin.usage` answer carries.
const DAYS_SHOWN: u64 = 30;

/// The kind of request, as the operator sees it.
pub fn kind_of(model: &str) -> &'static str {
    if crate::gemini::is_image_model(model) {
        "picture"
    } else {
        "text"
    }
}

#[derive(Default, Clone, Copy, serde::Serialize)]
pub struct Count {
    pub requests: u64,
    pub toku: u64,
    pub declined: u64,
}

/// One hour of counts, by (model, kind).
struct Hour {
    hour: u64,
    by: HashMap<(String, &'static str), Count>,
}

/// Everything the enclave counts for the operator.
pub struct Stats {
    hours: Mutex<VecDeque<Hour>>,
    /// Deltas for the book's `tally` table, waiting for the next tick.
    pending: Mutex<HashMap<String, i64>>,
    /// Accounts (by their stored name) seen today and yesterday: a count, not a list,
    /// is what leaves here.
    active: Mutex<(u64, HashSet<String>, HashSet<String>)>,
    /// Redemption-shaped events of the last day: (when, which), for the ghosts' rule.
    events: Mutex<VecDeque<(u64, crate::ghost::Event)>>,
    pub started: std::time::Instant,
}

impl Default for Stats {
    fn default() -> Stats {
        Stats {
            hours: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            active: Mutex::new((0, HashSet::new(), HashSet::new())),
            events: Mutex::new(VecDeque::new()),
            started: std::time::Instant::now(),
        }
    }
}

impl Stats {
    /// A request was answered (or declined): counted for its hour and its day.
    pub fn request(&self, now_ms: u64, model: &str, toku: u64, declined: bool) {
        let kind = kind_of(model);
        let hour = now_ms / 3_600_000;
        if let Ok(mut hours) = self.hours.lock() {
            if hours.back().map(|h| h.hour != hour).unwrap_or(true) {
                hours.push_back(Hour { hour, by: HashMap::new() });
                while hours.len() > HOURS_KEPT {
                    hours.pop_front();
                }
            }
            let c = hours.back_mut().expect("just pushed").by.entry((model.to_string(), kind)).or_default();
            c.requests += 1;
            c.toku += toku;
            c.declined += declined as u64;
        }
        let day = now_ms / 86_400_000;
        self.add(&format!("d:{day}:{model}:{kind}:requests"), 1);
        self.add(&format!("d:{day}:{model}:{kind}:toku"), toku as i64);
        if declined {
            self.add(&format!("d:{day}:{model}:{kind}:declined"), 1);
        }
    }

    /// A redemption happened (real) or was made (a ghost).
    pub fn event(&self, now_ms: u64, what: crate::ghost::Event) {
        if let Ok(mut e) = self.events.lock() {
            e.push_back((now_ms, what));
            while e.front().map(|(at, _)| now_ms.saturating_sub(*at) > 86_400_000).unwrap_or(false) {
                e.pop_front();
            }
        }
    }

    pub fn events_since(&self, since_ms: u64, what: crate::ghost::Event) -> usize {
        self.events.lock().map(|e| e.iter().filter(|(at, w)| *at >= since_ms && *w == what).count()).unwrap_or(0)
    }

    /// A count for the book, added at the next flush.
    pub fn add(&self, key: &str, n: i64) {
        if let Ok(mut p) = self.pending.lock() {
            *p.entry(key.to_string()).or_insert(0) += n;
        }
    }

    /// An account was heard from today.
    pub fn seen(&self, account_key: &str, now_ms: u64) {
        let day = now_ms / 86_400_000;
        if let Ok(mut a) = self.active.lock() {
            if a.0 != day {
                let today = std::mem::take(&mut a.1);
                a.2 = if a.0 + 1 == day { today } else { HashSet::new() };
                a.0 = day;
            }
            a.1.insert(account_key.to_string());
        }
    }

    fn active_counts(&self, now_ms: u64) -> (usize, usize) {
        let day = now_ms / 86_400_000;
        match self.active.lock() {
            Ok(a) if a.0 == day => (a.1.len(), a.2.len()),
            Ok(a) if a.0 + 1 == day => (0, a.1.len()),
            _ => (0, 0),
        }
    }

    /// Take what is waiting for the book.
    pub fn drain(&self) -> Vec<(String, i64)> {
        match self.pending.lock() {
            Ok(mut p) => p.drain().collect(),
            Err(_) => Vec::new(),
        }
    }

    fn hours_json(&self) -> Vec<Value> {
        let Ok(hours) = self.hours.lock() else { return Vec::new() };
        let mut out = Vec::new();
        for h in hours.iter() {
            for ((model, kind), c) in &h.by {
                out.push(json!({ "hour": h.hour, "model": model, "kind": kind, "requests": c.requests, "toku": c.toku, "declined": c.declined }));
            }
        }
        out
    }
}

/// The plan as the operator sees it: no account, no rail id — tier, rail kind, state, dates.
fn plan_row(p: &crate::plans::Plan, now: u64) -> Value {
    json!({
        "tier": p.tier, "yearly": p.yearly,
        "rail": if p.is_note() { "note" } else if p.is_app_store() { "appstore" } else { "stripe" },
        "active": p.paid_at(now), "periodStart": p.period_start_ms, "paidUntil": p.paid_until_ms,
        "disputed": p.disputed_at_ms > 0,
    })
}

impl Enclave {
    /// Is this the operator? Every `admin.*` operation starts here.
    pub(crate) fn is_admin(&self, account: &str) -> bool {
        self.admin.as_deref() == Some(account)
    }

    /// Daily tallies into the book, from memory; called from the tick.
    pub(crate) fn flush_tallies(&self) {
        let pending = self.stats.drain();
        if pending.is_empty() {
            return;
        }
        if let Ok(l) = self.ledger.lock() {
            for (key, n) in &pending {
                if l.tally_add(key, *n).is_err() {
                    // Put back for the next tick rather than lost.
                    self.stats.add(key, *n);
                }
            }
        } else {
            for (key, n) in pending {
                self.stats.add(&key, n);
            }
        }
    }

    pub(crate) fn admin_dispatch(&self, account: &str, op: &str, body: &str, now: u64) -> Value {
        if !self.is_admin(account) {
            return error("not for this account");
        }
        match op {
            "admin.health" => self.admin_health(now),
            "admin.usage" => self.admin_usage(now),
            "admin.plans" => self.admin_plans(now),
            "admin.account" => self.admin_account(body, now),
            _ => error("unknown operation"),
        }
    }

    fn admin_health(&self, now: u64) -> Value {
        let (replies, reply_bytes) = self.replies.lock().map(|r| (r.len(), r.values().map(|(_, b)| b.len()).sum::<usize>())).unwrap_or((0, 0));
        let strikes_today = self.strikes.lock().map(|s| s.iter().filter(|(k, _)| k.1 == now / 86_400_000).map(|(_, n)| *n as u64).sum::<u64>()).unwrap_or(0);
        let (book, flushed, holds, since) = match self.ledger.lock() {
            Ok(l) => {
                let flushed = l.flushed().map(|f| f.position()).unwrap_or((0, 0));
                (l.mark(), flushed, l.holds_open().unwrap_or(0), l.since_snapshot())
            }
            Err(_) => ((0, 0), (0, 0), 0, 0),
        };
        let (prices_at, prices) = self.plan_prices.lock().map(|p| (p.1, p.0.clone())).unwrap_or((0, Vec::new()));
        json!({
            "kind": "admin.health",
            "now": now,
            "uptimeS": self.stats.started.elapsed().as_secs(),
            "replayed": self.replayed,
            "devMode": self.dev_mode,
            "appleSandbox": crate::policy::APPLE_SANDBOX,
            "address": self.address.lock().map(|a| a.clone()).unwrap_or_default(),
            "book": { "generation": book.0, "record": book.1, "flushedGeneration": flushed.0, "flushedRecord": flushed.1, "sinceSnapshot": since, "holdsOpen": holds },
            "replies": { "count": replies, "bytes": reply_bytes },
            "working": self.working.load(std::sync::atomic::Ordering::Relaxed),
            "coverWaiting": self.cover.waiting_count(),
            "redemptionsLastHour": self.redemption_shaped_last_hour().0,
            "ghostsLastHour": self.redemption_shaped_last_hour().1,
            "ghostsToday": self.stats.events_since(now.saturating_sub(86_400_000), crate::ghost::Event::Ghost),
            "strikesToday": strikes_today,
            "stripe": { "configured": self.stripe.is_some(), "pricesReadMs": prices_at, "prices": prices },
            "appleApi": self.apple_api.is_some(),
            "pricingVersion": self.pricing.version(),
            "models": crate::catalog::models(&self.pricing, &self.providers, self.dev_mode),
            "notesWindow": crate::notes::Mint::window(now),
        })
    }

    fn admin_usage(&self, now: u64) -> Value {
        let day = now / 86_400_000;
        let mut days: Vec<Value> = Vec::new();
        if let Ok(l) = self.ledger.lock() {
            // d:<day>:<model>:<kind>:<what> → one row per (day, model, kind)
            let mut rows: HashMap<(u64, String, String), Count> = HashMap::new();
            for (key, n) in l.tally_read("d:").unwrap_or_default() {
                let parts: Vec<&str> = key.splitn(5, ':').collect();
                if parts.len() != 5 {
                    continue;
                }
                let Ok(d) = parts[1].parse::<u64>() else { continue };
                if d + DAYS_SHOWN < day {
                    continue;
                }
                let c = rows.entry((d, parts[2].to_string(), parts[3].to_string())).or_default();
                match parts[4] {
                    "requests" => c.requests += n.max(0) as u64,
                    "toku" => c.toku += n.max(0) as u64,
                    "declined" => c.declined += n.max(0) as u64,
                    _ => {}
                }
            }
            let mut rows: Vec<_> = rows.into_iter().collect();
            rows.sort_by(|a, b| b.0 .0.cmp(&a.0 .0).then(a.0 .1.cmp(&b.0 .1)));
            for ((d, model, kind), c) in rows {
                days.push(json!({ "day": d, "model": model, "kind": kind, "requests": c.requests, "toku": c.toku, "declined": c.declined }));
            }
        }
        // What is counted but not yet in the book, so today is complete.
        let pending: Vec<Value> = self.stats.pending.lock().map(|p| p.iter().map(|(k, n)| json!({ "key": k, "n": n })).collect()).unwrap_or_default();
        let (today, yesterday) = self.stats.active_counts(now);
        json!({
            "kind": "admin.usage",
            "now": now,
            "hours": self.stats.hours_json(),
            "days": days,
            "pending": pending,
            "activeToday": today,
            "activeYesterday": yesterday,
            "margin": crate::policy::MARGIN,
            "tokuPerUsd": tokumai_core::billing::TOKU_PER_USD,
        })
    }

    fn admin_plans(&self, now: u64) -> Value {
        let Ok(l) = self.ledger.lock() else { return error("ledger unavailable") };
        let plans: Vec<Value> = l.plans_all().unwrap_or_default().iter().map(|p| plan_row(p, now)).collect();
        let notes: Vec<Value> = l
            .tally_read("notes:")
            .unwrap_or_default()
            .into_iter()
            .map(|(k, n)| json!({ "key": k, "n": n }))
            .collect();
        let (accounts, lots, lots_left) = l.account_counts().unwrap_or((0, 0, 0));
        json!({
            "kind": "admin.plans",
            "now": now,
            "accounts": accounts,
            "plans": plans,
            "notes": notes,
            "lots": { "count": lots, "left": lots_left },
            "tiers": tokumai_core::subscription::TIERS.iter().map(|(toku, cents)| json!({ "toku": toku, "cents": cents })).collect::<Vec<_>>(),
        })
    }

    /// One account, by the id the person sent in for a support case.
    fn admin_account(&self, body: &str, now: u64) -> Value {
        let b: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let Some(account) = b.get("account").and_then(|a| a.as_str()).filter(|a| !a.trim().is_empty()) else { return error("which account?") };
        let account = account.trim();
        let Ok(l) = self.ledger.lock() else { return error("ledger unavailable") };
        let balance = match l.balance(account, now) {
            Ok(b) => b,
            Err(e) => return error(&e),
        };
        let plan = l.plan_get(&l.acct_key(account)).ok().flatten();
        json!({
            "kind": "admin.account",
            "balance": balance,
            "plan": plan.as_ref().map(|p| plan_row(p, now)),
            "usage": plan.as_ref().map(|p| p.usage.clone()).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Db, Platform};

    fn enclave(admin: Option<&str>) -> &'static Enclave {
        let e = Enclave::start(Platform {
            attester: Box::new(tokumai_attest::sim::SimAttester::new([5; 32], "image-1")),
            keys: Box::new(crate::seal::FixedKeyProvider([9; 32])),
            providers: crate::provider::Providers::mock(),
            db: Db::Memory,
            pricing: tokumai_core::pricing::PricingTable::parse(crate::policy::PRICING_JSON).unwrap(),
            dev_mode: false,
            stripe: None,
            apple_api: None,
            witness: None,
            admin: admin.map(str::to_string),
        })
        .unwrap();
        Box::leak(Box::new(e))
    }

    #[test]
    fn only_the_operator_is_answered_and_only_with_counts() {
        let e = enclave(Some("the-operator"));
        let now = 1_790_000_000_000;
        assert_eq!(e.admin_dispatch("someone", "admin.health", "{}", now)["kind"], "error");
        assert_eq!(enclave(None).admin_dispatch("the-operator", "admin.health", "{}", now)["kind"], "error", "no operator named: nobody");
        let h = e.admin_dispatch("the-operator", "admin.health", "{}", now);
        assert_eq!(h["kind"], "admin.health");
        assert_eq!(h["working"], 0);

        // Three requests today, one declined, one of them a picture; an account seen.
        e.stats.request(now, "gemini-3.5-flash-lite", 120, false);
        e.stats.request(now, "gemini-3.5-flash-lite", 0, true);
        e.stats.request(now, "gemini-3.5-flash-image", 90_000, false);
        e.stats.seen("acct-key-1", now);
        e.stats.seen("acct-key-1", now);
        e.stats.seen("acct-key-2", now);
        e.stats.add("notes:minted:9:1", 2);
        e.stats.add("notes:spent:9:1", 1);
        e.flush_tallies();
        let u = e.admin_dispatch("the-operator", "admin.usage", "{}", now);
        assert_eq!(u["activeToday"], 2);
        let days = u["days"].as_array().unwrap();
        let text = days.iter().find(|d| d["kind"] == "text").unwrap();
        assert_eq!((text["requests"].as_u64(), text["toku"].as_u64(), text["declined"].as_u64()), (Some(2), Some(120), Some(1)));
        let picture = days.iter().find(|d| d["kind"] == "picture").unwrap();
        assert_eq!(picture["toku"], 90_000);
        assert_eq!(u["hours"].as_array().unwrap().len(), 2, "two (model, kind) rows this hour");
        let p = e.admin_dispatch("the-operator", "admin.plans", "{}", now);
        let notes = p["notes"].as_array().unwrap();
        assert!(notes.iter().any(|n| n["key"] == "notes:minted:9:1" && n["n"] == 2));
        assert_eq!(p["accounts"], 0);
        // The support lookup names an account the operator typed in, and nothing else does.
        let a = e.admin_dispatch("the-operator", "admin.account", r#"{"account":"acct-x"}"#, now);
        assert_eq!(a["kind"], "admin.account");
        assert_eq!(a["balance"]["total"], 0);
        assert!(serde_json::to_string(&u).unwrap().contains("acct-key") == false, "no account name leaves in the usage");
    }
}
