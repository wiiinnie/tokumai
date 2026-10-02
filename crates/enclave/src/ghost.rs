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
//! receiving door writes three records of a redemption's sizes that change nothing, waits
//! for the host as a real redemption would, and sends a reply-sized packet back. The host
//! sees a redemption arrive. The book grows by three sealed no-ops; no row changes, no
//! count moves, nothing is charged.
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

/// The three records a redemption writes, as the journal sees them (sealed JSON of the
/// change): the spent-note row, the plan, the allowance.
pub const GHOST_RECORD_BYTES: [usize; 3] = [165, 520, 300];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Redeemed,
    Ghost,
}

impl Enclave {
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

/// The bytes a ghost request carries: the marker, the address to answer to, padding.
pub fn request(reply_to: &str) -> Vec<u8> {
    let mut out = format!("tokumai/ghost/req {reply_to}\n").into_bytes();
    out.resize(GHOST_REQUEST_BYTES.max(out.len()), 0);
    out
}

/// Where a ghost request wants its reply, if the bytes are one.
pub fn reply_to(message: &[u8]) -> Option<String> {
    let text = message.strip_prefix(b"tokumai/ghost/req ")?;
    let end = text.iter().position(|b| *b == b'\n')?;
    std::str::from_utf8(&text[..end]).ok().map(str::to_string)
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
        assert_eq!(journal.len(), before + 3, "three records, like a redemption");
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
    fn the_wire_shape_round_trips() {
        let req = request("DoorB.address@gateway");
        assert_eq!(req.len(), GHOST_REQUEST_BYTES);
        assert_eq!(reply_to(&req).as_deref(), Some("DoorB.address@gateway"));
        assert_eq!(reply_to(b"tokumai/still-there"), None);
        assert_eq!(reply().len(), GHOST_REPLY_BYTES);
    }
}
