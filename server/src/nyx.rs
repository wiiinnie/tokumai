// nyx.rs — accept NYM natively on the Nyx chain (Cosmos SDK), no payment
// processor. Ported from src/money/nyx-gateway.ts, but SIMPLER on purpose:
//
// The TS version ran a background watcher (WebSocket push, HTTP-poll fallback,
// in-memory pending map). This server's payment flow is already poll-shaped —
// the client polls `invoice.status` while the pay screen is open, and the
// entitlement sweep re-checks every open invoice — so the chain is queried ON
// DEMAND via the LCD REST API instead. No task, no socket, no volatile state:
// the expected amount lives in the persisted invoice record, so a half-paid
// invoice even survives a server restart (the TS watcher could not).
//
// WHY A MEMO, NOT A PER-INVOICE ADDRESS: Cosmos has no cheap sub-addresses.
// One receive address + a unique TOKU-XXXXXXXX memo is how nym.com correlates
// payments too. The UI shows the memo as prominently as the amount.
//
// PRIVACY: the SERVER queries the price feed and the LCD. The buyer's wallet
// talks to the chain, never to us — the payment leaks nothing about the buyer
// to the scrai-server beyond what the chain itself records. Querying a public
// LCD reveals OUR receive address to that endpoint; running our own nyxd node
// removes even that (NYX_LCD_URL is env-swappable on purpose).

use serde_json::Value;
use std::sync::Mutex;

const MICRO: u64 = 1_000_000; // 1 NYM = 1_000_000 unym

pub struct Nyx {
    pub receive_address: String,
    lcd_url: String,
    /// NYX_PRICE_URL override (tests, own feed); None → built-in feed chain.
    price_url: Option<String>,
    /// (usd_per_nym, fetched_at_ms) — one price-feed hit per minute is plenty.
    price: Mutex<(f64, u64)>,
    /// Chain-watch health for the pay screen's indicator: (last query ok,
    /// checked_at_ms, latest block height). Updated by every on-demand check —
    /// there is no background watcher, so "live" means "the last poll worked".
    watch: Mutex<(bool, u64, Option<u64>)>,
}

impl Nyx {
    /// Configured via NYX_RECEIVE_ADDRESS + NYX_LCD_URL (both required — an
    /// issuer must never watch a guessed endpoint). NYX_PRICE_URL overrides the
    /// CoinGecko feed, mostly for tests.
    pub fn from_env() -> Option<Nyx> {
        // The TS server took a Tendermint RPC (NYX_RPC_HTTP); this watcher needs
        // an LCD/REST endpoint instead. Catch the stale var so the rail doesn't
        // just silently stay off after the migration.
        if crate::net_var("NYX_LCD_URL").is_none()
            && crate::cfg("NYX_RPC_HTTP").is_ok_and(|s| !s.trim().is_empty())
        {
            eprintln!(
                "scrai-server: NYX_RPC_HTTP is the old TS variable and is ignored — set NYX_LCD_URL \
                 to an LCD/REST endpoint (mainnet: https://api.nymtech.net, \
                 sandbox: https://validator-sandbox-1.nymtech.net/api)"
            );
        }
        // Network-scoped (NYX_*_MAINNET / _TESTNET, legacy plain fallback).
        let addr = crate::net_var("NYX_RECEIVE_ADDRESS")?;
        let lcd = crate::net_var("NYX_LCD_URL")?;
        Some(Nyx {
            receive_address: addr.trim().to_string(),
            lcd_url: lcd.trim().trim_end_matches('/').to_string(),
            price_url: crate::cfg("NYX_PRICE_URL").ok().filter(|s| !s.trim().is_empty()),
            price: Mutex::new((0.0, 0)),
            watch: Mutex::new((false, 0, None)),
        })
    }

    /// USD per NYM, cached for 60s so an invoice burst can't rate-limit us.
    ///
    /// Sources are tried IN ORDER until one answers — public price APIs love to
    /// rate-limit or geo-block datacenter IPs (CoinGecko notably 429s the VPS),
    /// so a single feed is a single point of failure for the whole NYM rail.
    /// The USDT pairs are treated as USD — fine at pricing granularity.
    async fn usd_per_nym(&self) -> Result<f64, String> {
        let now = crate::pay::now_ms();
        {
            let p = self.price.lock().unwrap(); // nosemgrep: scrai-unwrap-in-server-hot-path -- poison-only: the guarded value is a plain tuple, no code runs under the lock
            if p.0 > 0.0 && now - p.1 < 60_000 {
                return Ok(p.0);
            }
        }
        // (name, url, JSON pointer to the rate — number or numeric string).
        // CoinGecko leads as the multi-venue aggregate; Bitfinex is the deepest
        // actual NYM/USD market (a true USD pair — its ticker is a bare array,
        // LAST_PRICE at index 6); then the USDT venues.
        let builtin: [(&str, &str, &str); 5] = [
            ("coingecko", "https://api.coingecko.com/api/v3/simple/price?ids=nym&vs_currencies=usd", "/nym/usd"),
            ("bitfinex", "https://api-pub.bitfinex.com/v2/ticker/tNYMUSD", "/6"),
            ("coinpaprika", "https://api.coinpaprika.com/v1/tickers/nym-nym?quotes=USD", "/quotes/USD/price"),
            ("gate.io", "https://api.gateio.ws/api/v4/spot/tickers?currency_pair=NYM_USDT", "/0/last"),
            ("mexc", "https://api.mexc.com/api/v3/ticker/price?symbol=NYMUSDT", "/price"),
        ];
        let override_url = self.price_url.as_deref();
        let sources: Vec<(&str, &str, &str)> = match override_url {
            // A configured NYX_PRICE_URL (tests, own feed) replaces the chain and
            // must answer in the CoinGecko shape.
            Some(u) => vec![("override", u, "/nym/usd")],
            None => builtin.to_vec(),
        };

        let mut errors = Vec::new();
        for (name, url, pointer) in sources {
            match self.fetch_rate(url, pointer).await {
                Ok(usd) => {
                    if name != "coingecko" && name != "override" {
                        eprintln!("scrai-server: NYM/USD via fallback feed {name} ({usd})");
                    }
                    *self.price.lock().unwrap() = (usd, now); // nosemgrep: scrai-unwrap-in-server-hot-path
                    return Ok(usd);
                }
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        Err(format!("no NYM price feed answered — {}", errors.join(" · ")))
    }

    async fn fetch_rate(&self, url: &str, pointer: &str) -> Result<f64, String> {
        let j: Value = crate::http::client()
            .get(url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("unreachable: {e}"))?
            .json()
            .await
            .map_err(|e| format!("non-JSON: {e}"))?;
        let v = j.pointer(pointer).ok_or("no rate in reply")?;
        let usd = v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0.0);
        if usd <= 0.0 {
            return Err("no NYM/USD rate in reply".into());
        }
        Ok(usd)
    }

    /// Quote an invoice: fix the exact unym the buyer must send at the current
    /// rate, rounded UP to a WHOLE NYM (234.5678 → 235) — a round number is what
    /// a human actually types into a wallet, and ceiling means rounding can only
    /// ever favour the operator, never undercharge. Returns (raised, expected_unym).
    pub async fn create_invoice(&self, cents: u32) -> Result<(crate::pay::RaisedInvoice, u64), String> {
        let usd = cents as f64 / 100.0;
        let rate = self.usd_per_nym().await?;
        // L4: a near-zero (or non-finite) rate from a hostile/broken price feed would blow
        // usd/rate up to a huge whole_nym and then wrap `* MICRO` in release, settling the
        // invoice for a fraction of a NYM. Floor the rate and saturate the multiply.
        if !(rate.is_finite() && rate > 1e-6) {
            return Err(format!("implausible NYM price ({rate} USD/NYM) — refusing to quote"));
        }
        let whole_nym = (usd / rate).ceil() as u64;
        let expected_unym = whole_nym.saturating_mul(MICRO);
        let nym_amount = whole_nym.to_string();
        let memo = new_memo();
        let expires_at = crate::pay::now_ms() + 15 * 60_000;
        let raised = crate::pay::RaisedInvoice {
            provider_ref: memo.clone(),
            pay_to: self.receive_address.clone(),
            instruction: format!(
                "Send exactly {nym_amount} NYM to the address below AND include the memo. \
                 The payment cannot be credited without the memo."
            ),
            options: serde_json::json!([{
                "method": "NYM",
                "destination": self.receive_address,
                // Nyx has no universal payment URI — the QR is the bare address.
                "uri": self.receive_address,
                "amount": nym_amount,
                "currency": "NYM",
                "memo": memo,
            }]),
            expires_at,
        };
        Ok((raised, expected_unym))
    }

    /// "paid" | "pending": search recent transfers to our address on the LCD and
    /// look for a successful tx carrying this memo with enough unym. Underpaid
    /// stays pending (the buyer can top up with a second memo-carrying transfer
    /// only via a NEW tx — so in practice: pending until the full amount landed
    /// in one transfer, mirroring the TS behaviour).
    /// `required_sender`: when set, only transfers FROM that address count (testnet
    /// invoices — sandbox NYM is free, so only the faucet wallet may fund them).
    pub async fn check_paid(&self, memo: &str, expected_unym: u64, required_sender: Option<&str>) -> Result<String, String> {
        // A live payment always lands at (or just below) the chain tip, but the tx-search
        // returns EVERY past transfer to our address — including old ones on blocks the
        // public Nyx endpoints have since PRUNED (they keep only a rolling window). The
        // LCD errors out ("height N is not available, lowest height is M") the moment it
        // tries to hydrate a pruned match, so the WHOLE search fails and a fresh payment
        // never gets returned. Bounding the search to recent heights skips the pruned
        // tail; a real payment is always well inside the window (we poll every ~10s while
        // the pay screen is open), so this never misses one.
        let tip = self.latest_height().await;
        let base = format!("transfer.recipient='{}'", self.receive_address);
        // Height floor: stay comfortably inside a ~100k-block pruning window (≈ days on
        // Nyx) while never reaching below the pruned boundary. `query=` (modern Cosmos)
        // supports the `AND tx.height>=` condition; the unbounded forms are last-ditch
        // fallbacks for an endpoint that rejects the modern param.
        let bounded = tip.map(|h| format!("{base} AND tx.height>={}", h.saturating_sub(30_000)));
        let attempts: Vec<(&str, String)> = match &bounded {
            Some(f) => vec![("query", f.clone()), ("query", base.clone()), ("events", base.clone())],
            None => vec![("query", base.clone()), ("events", base.clone())],
        };
        let mut last_err = String::new();
        for (param, filter) in attempts {
            let url = format!(
                "{}/cosmos/tx/v1beta1/txs?{param}={}&order_by=ORDER_BY_DESC&pagination.limit=100",
                self.lcd_url,
                urlencoding::encode(&filter),
            );
            match self.lcd_txs(&url).await {
                Ok(j) => {
                    // Feed the pay screen's health indicator with the tip we already have.
                    *self.watch.lock().unwrap() = (true, crate::pay::now_ms(), tip); // nosemgrep: scrai-unwrap-in-server-hot-path
                    return Ok(scan_txs(&j, &self.receive_address, memo, expected_unym, required_sender));
                }
                Err(e) => last_err = e,
            }
        }
        let h = self.watch.lock().unwrap().2; // nosemgrep: scrai-unwrap-in-server-hot-path
        *self.watch.lock().unwrap() = (false, crate::pay::now_ms(), h); // nosemgrep: scrai-unwrap-in-server-hot-path
        Err(format!("Nyx LCD query failed: {last_err}"))
    }

    /// Latest block height from the LCD, best-effort (the indicator survives
    /// a miss — it just shows no number).
    async fn latest_height(&self) -> Option<u64> {
        let j: Value = crate::http::client()
            .get(format!("{}/cosmos/base/tendermint/v1beta1/blocks/latest", self.lcd_url))
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        j.pointer("/block/header/height").and_then(|h| h.as_str()).and_then(|h| h.parse().ok())
    }

    /// Chain-watch state for the pay screen (same shape the TS gateway sent).
    /// "connected" = the last on-demand poll succeeded and is fresh — with the
    /// client polling every ~10s while the pay screen is open, that is a live
    /// health signal, not a stale flag.
    pub fn watch_state(&self) -> Value {
        let (ok, at, height) = *self.watch.lock().unwrap(); // nosemgrep: scrai-unwrap-in-server-hot-path
        let age = crate::pay::now_ms().saturating_sub(at);
        serde_json::json!({
            "connected": ok && at > 0 && age < 90_000,
            "mode": "polling",
            "height": height,
            "lastBlockAgoMs": if at > 0 { Value::from(age) } else { Value::Null },
        })
    }

    async fn lcd_txs(&self, url: &str) -> Result<Value, String> {
        let res = crate::http::client()
            .get(url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("unreachable: {e}"))?;
        let status = res.status();
        let j: Value = res.json().await.map_err(|e| format!("non-JSON: {e}"))?;
        if !status.is_success() {
            let msg = j.get("message").and_then(|m| m.as_str()).unwrap_or("");
            return Err(format!("{status}: {}", msg.chars().take(200).collect::<String>()));
        }
        Ok(j)
    }
}

/// Scan an LCD tx-search reply for a successful tx whose memo matches and which
/// moved at least `expected_unym` to `addr`. Pure so it's testable offline.
/// `required_sender`: None = anyone may pay (mainnet); Some(addr) = only transfers whose
/// `sender` is that address are summed (testnet: the faucet wallet, nobody else).
pub fn scan_txs(j: &Value, addr: &str, memo: &str, expected_unym: u64, required_sender: Option<&str>) -> String {
    let empty = Vec::new();
    let txs = j.get("txs").and_then(|t| t.as_array()).unwrap_or(&empty);
    let resps = j.get("tx_responses").and_then(|t| t.as_array()).unwrap_or(&empty);
    for (tx, resp) in txs.iter().zip(resps.iter()) {
        if resp.get("code").and_then(|c| c.as_u64()).unwrap_or(0) != 0 {
            continue; // failed tx moved no money
        }
        let tx_memo = tx.pointer("/body/memo").and_then(|m| m.as_str()).unwrap_or("").trim();
        if tx_memo != memo {
            continue;
        }
        if received_unym(resp, addr, required_sender) >= expected_unym {
            return "paid".into();
        }
        eprintln!("scrai-server: nyx invoice {memo} underpaid — left pending");
    }
    "pending".into()
}

/// Sum every unym this tx actually delivered to `addr`, reading the flat
/// `events` list (new LCD) and the `logs[].events` shape (older SDKs).
fn received_unym(resp: &Value, addr: &str, required_sender: Option<&str>) -> u64 {
    let flat = resp
        .get("events")
        .and_then(|e| e.as_array())
        .map(|e| scan_events(e, addr, required_sender))
        .unwrap_or(0);
    if flat > 0 {
        return flat;
    }
    resp.get("logs")
        .and_then(|l| l.as_array())
        .map(|logs| {
            logs.iter()
                .filter_map(|log| log.get("events").and_then(|e| e.as_array()))
                .map(|e| scan_events(e, addr, required_sender))
                .sum()
        })
        .unwrap_or(0)
}

fn scan_events(events: &[Value], addr: &str, required_sender: Option<&str>) -> u64 {
    let mut sum = 0u64;
    for ev in events {
        if ev.get("type").and_then(|t| t.as_str()) != Some("transfer") {
            continue;
        }
        let empty = Vec::new();
        let attrs = ev.get("attributes").and_then(|a| a.as_array()).unwrap_or(&empty);
        let to_us = attrs.iter().any(|a| {
            a.get("key").and_then(|k| k.as_str()) == Some("recipient")
                && a.get("value").and_then(|v| v.as_str()) == Some(addr)
        });
        if !to_us {
            continue;
        }
        if let Some(want) = required_sender {
            let from_faucet = attrs.iter().any(|a| {
                a.get("key").and_then(|k| k.as_str()) == Some("sender")
                    && a.get("value").and_then(|v| v.as_str()) == Some(want)
            });
            if !from_faucet {
                continue; // right memo, right amount, wrong wallet — does not count
            }
        }
        for a in attrs {
            if a.get("key").and_then(|k| k.as_str()) == Some("amount") {
                sum += parse_unym(a.get("value").and_then(|v| v.as_str()).unwrap_or(""));
            }
        }
    }
    sum
}

/// "1000000unym" or "100ibc/ABC…,1000000unym" → unym only. Native NYM only —
/// an IBC-wrapped denom is somebody else's token and never settles an invoice.
fn parse_unym(amount: &str) -> u64 {
    amount
        .split(',')
        .filter_map(|part| part.trim().strip_suffix("unym"))
        .filter_map(|n| n.parse::<u64>().ok())
        .sum()
}

/// TOKUXXXXXXXX — short, unique, human-copyable. Uppercase + digits only
/// (no 0/O/1/I in the random part) so it survives a wallet's memo field without
/// ambiguity, and
/// STRICTLY alphanumeric: the Nym GUI wallet rejects a memo containing a
/// hyphen ("only alphanumeric characters and white spaces are allowed").
fn new_memo() -> String {
    use rand::RngCore;
    const ABC: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    let s: String = b.iter().map(|x| ABC[(*x as usize) % ABC.len()] as char).collect();
    format!("TOKU{s}")
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lcd_reply(code: u64, memo: &str, amount: &str, recipient: &str) -> Value {
        json!({
            "txs": [ { "body": { "memo": memo } } ],
            "tx_responses": [ {
                "code": code,
                "events": [ {
                    "type": "transfer",
                    "attributes": [
                        { "key": "recipient", "value": recipient },
                        { "key": "sender", "value": "n1sender" },
                        { "key": "amount", "value": amount }
                    ]
                } ]
            } ]
        })
    }

    #[test]
    fn matching_memo_with_enough_unym_settles() {
        let j = lcd_reply(0, "SCRAI-ABC23456", "5000000unym", "n1ourselves");
        assert_eq!(scan_txs(&j, "n1ourselves", "SCRAI-ABC23456", 5_000_000, None), "paid");
    }

    #[test]
    fn underpaid_wrong_memo_failed_tx_or_wrong_recipient_stay_pending() {
        let under = lcd_reply(0, "SCRAI-ABC23456", "4999999unym", "n1ourselves");
        assert_eq!(scan_txs(&under, "n1ourselves", "SCRAI-ABC23456", 5_000_000, None), "pending");
        let wrong_memo = lcd_reply(0, "SCRAI-OTHER222", "5000000unym", "n1ourselves");
        assert_eq!(scan_txs(&wrong_memo, "n1ourselves", "SCRAI-ABC23456", 5_000_000, None), "pending");
        let failed = lcd_reply(5, "SCRAI-ABC23456", "5000000unym", "n1ourselves");
        assert_eq!(scan_txs(&failed, "n1ourselves", "SCRAI-ABC23456", 5_000_000, None), "pending");
        let not_ours = lcd_reply(0, "SCRAI-ABC23456", "5000000unym", "n1somebodyelse");
        assert_eq!(scan_txs(&not_ours, "n1ourselves", "SCRAI-ABC23456", 5_000_000, None), "pending");
    }

    /// Testnet: sandbox NYM is free, so a memo+amount match from any wallet but the
    /// faucet's must NOT settle — otherwise anyone self-funds $1 credits without a code.
    #[test]
    fn testnet_settles_only_from_the_faucet_wallet() {
        let j = lcd_reply(0, "SCRAI-ABC23456", "5000000unym", "n1ourselves"); // sender n1sender
        assert_eq!(scan_txs(&j, "n1ourselves", "SCRAI-ABC23456", 5_000_000, Some("n1sender")), "paid");
        assert_eq!(scan_txs(&j, "n1ourselves", "SCRAI-ABC23456", 5_000_000, Some("n1faucet")), "pending");
        // legacy `logs[].events` shape honours the pin too
        let legacy = json!({
            "txs": [ { "body": { "memo": "SCRAI-ABC23456" } } ],
            "tx_responses": [ { "code": 0, "logs": [ { "events": [ {
                "type": "transfer",
                "attributes": [
                    { "key": "recipient", "value": "n1ourselves" },
                    { "key": "sender", "value": "n1stranger" },
                    { "key": "amount", "value": "5000000unym" }
                ] } ] } ] } ]
        });
        assert_eq!(scan_txs(&legacy, "n1ourselves", "SCRAI-ABC23456", 5_000_000, None), "paid");
        assert_eq!(scan_txs(&legacy, "n1ourselves", "SCRAI-ABC23456", 5_000_000, Some("n1faucet")), "pending");
    }

    #[test]
    fn parse_unym_sums_native_denom_and_ignores_ibc() {
        assert_eq!(parse_unym("1000000unym"), 1_000_000);
        assert_eq!(parse_unym("100ibc/ABCDEF,2500000unym"), 2_500_000);
        assert_eq!(parse_unym("100ibc/ABCDEF"), 0);
        assert_eq!(parse_unym(""), 0);
    }

    #[test]
    fn memos_are_prefixed_and_unambiguous() {
        let m = new_memo();
        assert!(m.starts_with("TOKU"));
        assert_eq!(m.len(), "TOKU".len() + 8);
        // Strictly alphanumeric — the Nym GUI wallet rejects anything else.
        assert!(m.chars().all(|c| c.is_ascii_alphanumeric()));
        let suffix = &m["TOKU".len()..];
        assert!(!suffix.contains('0') && !suffix.contains('O') && !suffix.contains('1') && !suffix.contains('I'));
    }
}
