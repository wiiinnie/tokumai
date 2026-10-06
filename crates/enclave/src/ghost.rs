//! Redemptions that nobody made, for the hours in which nobody does.
//!
//! The host sees the enclave's side of the mixnet without cover (`server::mix`): on a
//! door's tunnel a redemption is one packet in, three records to the book, a wait for the
//! host's confirmation, one packet out — and no call to a provider. In a busy hour that
//! shape is one among many; in a thin hour it is the one event, and its time is the time
//! an account renewed, which the merchant's records can be held against.
//!
//! So in thin hours the enclave makes that shape itself. One door sends a request-sized
//! packet to another door through the mixnet (out at the sender, in at the receiver — the
//! sender's side looks like the self-ping every door posts every two minutes anyway); the
//! receiving door writes four records of a redemption's sizes that change nothing, waits
//! for the host as a real redemption would, and sends a reply-sized packet back. The host
//! sees a redemption arrive. The book grows by four sealed no-ops; no row changes, no
//! count moves, nothing is charged.
//!
//! The packet carries a MAC under a key only the enclave has, over the door to answer and
//! the minute: a ghost is answered only when it is ours and of this moment. Without that
//! (until 2026-10-06, audit M4) anyone on the mixnet could make a door write records and
//! count a ghost — four an hour, and the enclave's own ghosts switched themselves off,
//! leaving every redemption-shaped event in the hour a real one. And the ghost wrote three
//! records where a redemption writes four (it had no rails row), so the two were told
//! apart by counting.
//!
//! The rule: in any hour with fewer than [`GHOST_FLOOR`] redemptions, real and made
//! together, the scheduler (`server::mix::ghosts`) fires one with the probability that
//! brings the hour up to the floor, each minute at random. Real traffic above the floor
//! switches it off by itself. Cost: three journal records and a few mixnet packets.
//!
//! What it does not do: it does not pad the enclave's provider calls (a real first
//! question has its own cover, `cover`), and with a single door open it cannot run — a
//! packet from a door to itself has the wrong direction pattern and is left alone.

use crate::service::Enclave;
#[cfg(test)]
use crate::state::Store as _;

/// The redemption-shaped events an hour should hold at least.
pub const GHOST_FLOOR: usize = 4;

/// What a one-note redemption's request and reply weigh on the wire, roughly, before
/// Sphinx packs them: the account's key and signature, a note with its signature, the
/// sealing; and a plan summary back.
pub const GHOST_REQUEST_BYTES: usize = 900;
pub const GHOST_REPLY_BYTES: usize = 400;

/// The four records a redemption writes, as the journal sees them (sealed JSON of the
/// change, measured 2026-10-06): the spent-note row, the rails row, the allowance, the
/// plan (which grows a little with each period's usage step).
pub const GHOST_RECORD_BYTES: [usize; 4] = [156, 249, 379, 520];

/// How far a ghost's minute may be from the receiving door's clock: the mixnet's delay,
/// and a little.
const GHOST_MINUTES: u64 = 3;

fn mac(key: &[u8; 32], reply_to: &str, minute: u64) -> String {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha2::Sha256>>::new_from_slice(key).expect("hmac takes any key length");
    m.update(reply_to.as_bytes());
    m.update(b"\n");
    m.update(&minute.to_be_bytes());
    hex::encode(&m.finalize().into_bytes()[..16])
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Redeemed,
    Ghost,
}

impl Enclave {
    /// A ghost request for `reply_to`, signed for the doors of this enclave.
    pub fn ghost_request(&self, reply_to: &str) -> Vec<u8> {
        request(&self.ghost_key, reply_to, crate::now_ms())
    }

    /// Where an arriving ghost request wants its reply, if it is ours and of this moment.
    pub fn ghost_reply_to(&self, message: &[u8]) -> Option<String> {
        reply_to(&self.ghost_key, message, crate::now_ms())
    }

    /// Write what a redemption writes, change nothing, wait as a redemption waits.
    pub async fn ghost_redemption(&self) -> Result<(), String> {
        {
            let l = self.ledger.lock().map_err(|_| "ledger unavailable".to_string())?;
            l.ghost_records(&GHOST_RECORD_BYTES)?;
        }
        self.wait_for_host().await;
        self.stats.event(crate::now_ms(), Event::Ghost);
        Ok(())
    }

    /// Redemption-shaped events in the last hour, real and made.
    pub fn redemption_shaped_last_hour(&self) -> (usize, usize) {
        let since = crate::now_ms().saturating_sub(3_600_000);
        (self.stats.events_since(since, Event::Redeemed), self.stats.events_since(since, Event::Ghost))
    }

    /// How many ghosts an hour at this level of real traffic: the floor less what the hour
    /// already holds, never below zero.
    pub fn ghosts_wanted(&self) -> usize {
        let (real, made) = self.redemption_shaped_last_hour();
        GHOST_FLOOR.saturating_sub(real + made)
    }
}

/// The bytes a ghost request carries: the marker, the address to answer to, the minute,
/// a MAC over both under `key`, padding.
pub fn request(key: &[u8; 32], reply_to: &str, now_ms: u64) -> Vec<u8> {
    let minute = now_ms / 60_000;
    let mut out = format!("tokumai/ghost/req {reply_to} {minute} {}\n", mac(key, reply_to, minute)).into_bytes();
    out.resize(GHOST_REQUEST_BYTES.max(out.len()), 0);
    out
}

/// Where a ghost request wants its reply — if the bytes are one, of this moment, and ours.
pub fn reply_to(key: &[u8; 32], message: &[u8], now_ms: u64) -> Option<String> {
    let text = message.strip_prefix(b"tokumai/ghost/req ")?;
    let end = text.iter().position(|b| *b == b'\n')?;
    let line = std::str::from_utf8(&text[..end]).ok()?;
    let mut parts = line.split(' ');
    let (reply_to, minute, tag) = (parts.next()?, parts.next()?.parse::<u64>().ok()?, parts.next()?);
    if parts.next().is_some() || (now_ms / 60_000).abs_diff(minute) > GHOST_MINUTES {
        return None;
    }
    let want = mac(key, reply_to, minute);
    // Every byte compared, whatever the first said.
    if want.len() != tag.len() || want.bytes().zip(tag.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) != 0 {
        return None;
    }
    Some(reply_to.to_string())
}

pub fn reply() -> Vec<u8> {
    let mut out = b"tokumai/ghost/reply\n".to_vec();
    out.resize(GHOST_REPLY_BYTES, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Db, Platform};

    #[tokio::test]
    async fn a_ghost_writes_a_redemptions_records_and_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("tokumai-ghost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = std::sync::Arc::new(crate::state::FileStore::new(&dir).unwrap());
        let e = Enclave::start(Platform {
            attester: Box::new(tokumai_attest::sim::SimAttester::new([5; 32], "image-1")),
            keys: Box::new(crate::seal::FixedKeyProvider([9; 32])),
            providers: crate::provider::Providers::mock(),
            db: Db::Kept(store.clone()),
            pricing: tokumai_core::pricing::PricingTable::parse(crate::policy::PRICING_JSON).unwrap(),
            dev_mode: false,
            stripe: None,
            apple_api: None,
            witness: None,
            admin: None,
        })
        .unwrap();
        let now = 1_790_000_000_000;
        e.ledger.lock().unwrap().grant_allowance("acct", now, now + 86_400_000, 1_000).unwrap();
        e.ledger.lock().unwrap().flush(std::time::Duration::from_secs(5)).unwrap();
        let before = crate::state::split(&store.journal().unwrap()).unwrap().records.len();
        assert_eq!(e.ghosts_wanted(), GHOST_FLOOR, "a quiet hour wants the whole floor");
        e.ghost_redemption().await.unwrap();
        e.ledger.lock().unwrap().flush(std::time::Duration::from_secs(5)).unwrap();
        let journal = crate::state::split(&store.journal().unwrap()).unwrap().records;
        assert_eq!(journal.len(), before + 4, "four records, like a redemption");
        // Sealed sizes track the sizes a redemption's records have (plus the seal's 28 bytes).
        let sizes: Vec<usize> = journal[before..].iter().map(|r| r.len()).collect();
        for (got, want) in sizes.iter().zip(GHOST_RECORD_BYTES.iter()) {
            assert!((*got as i64 - *want as i64 - 28).abs() <= 2, "record of {got} bytes for a wanted {want}");
        }
        assert_eq!(e.ledger.lock().unwrap().balance("acct", now).unwrap().allowance, 1_000, "nothing changed");
        assert_eq!(e.redemption_shaped_last_hour(), (0, 1));
        assert_eq!(e.ghosts_wanted(), GHOST_FLOOR - 1);
        // The book replays the no-ops without complaint.
        drop(e);
        let again = crate::ledger::Ledger::open_sealed(store, [9u8; 32], None).unwrap();
        assert_eq!(again.balance("acct", now).unwrap().allowance, 1_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wire_shape_round_trips_only_for_our_own_of_this_moment() {
        let key = [3u8; 32];
        let now = 1_790_000_000_000;
        let req = request(&key, "DoorB.address@gateway", now);
        assert_eq!(req.len(), GHOST_REQUEST_BYTES);
        assert_eq!(reply_to(&key, &req, now).as_deref(), Some("DoorB.address@gateway"));
        assert_eq!(reply_to(&key, &req, now + 2 * 60_000).as_deref(), Some("DoorB.address@gateway"), "the mixnet takes a moment");
        assert_eq!(reply_to(&key, &req, now + 10 * 60_000), None, "not an old one again");
        assert_eq!(reply_to(&[4u8; 32], &req, now), None, "not under another key");
        assert_eq!(reply_to(&key, b"tokumai/still-there", now), None);
        assert_eq!(reply_to(&key, b"tokumai/ghost/req DoorB.address@gateway\n", now), None, "the old, unsigned form");
        let mut bent = req.clone();
        bent[40] ^= 1;
        assert_eq!(reply_to(&key, &bent, now), None);
        assert_eq!(reply().len(), GHOST_REPLY_BYTES);
    }
}
