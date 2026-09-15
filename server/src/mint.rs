// ---------------------------------------------------------------------------
// mint.rs — the server's issuing authorities: one per denomination, several per
// denomination alive at once.
//
// TWO DENOMINATIONS. Compact ecash has no value field inside a coin: what a coin is worth
// is decided by the KEY that signed it. So each denomination is its own authority, and a
// note names its denomination only to pick the key it is verified against — one that lies
// about it fails. (Why two at all: a payment costs ~490 B and ~4 ms of pairings PER COIN
// and a request tenders its CEILING, so a 9 ¢ picture was ninety coins at one size.)
//
// ROLLING EPOCHS. An authority's expiration date is fixed when it is created, and every
// book it issues dies on that date — so a single authority means one cliff for everybody,
// and a book drawn the day before it lives a day. Worse, past that date NOTHING can be
// issued any more: the authority would be signing against a date in the past.
//
// So a fresh authority is created every `ROLL_EVERY_DAYS` and the older ones are kept for
// as long as their books can still be spent. New books always come from the newest, which
// is why they always have nearly the full `BOOK_VALIDITY_DAYS` of life. The server CANNOT
// move existing books into a new epoch: blind signatures mean it never saw them, and
// nothing links a book to an account. Only the device that holds a book can swap it.
// ---------------------------------------------------------------------------
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU, DENOMS};
use scrai_core::federation::{self, Authority};
use scrai_core::tender::Note;

/// How often a fresh authority is created. It is half of the promise in the terms: an
/// epoch is stamped with the promised validity PLUS one roll, so the OLDEST a book can be
/// when drawn is one roll, and it still has the full promise ahead of it. Defined beside
/// the promise in the core, never separately.
pub use scrai_core::coconut::ROLL_EVERY_DAYS;
/// Kept past its expiration for as long as a payment from it can still verify
/// (`federation::SPEND_DATE_PAST_SECS` is two days), plus a day of slack.
const KEEP_PAST_EXPIRY_DAYS: u64 = 3;
const DAY: u64 = 86_400;

/// Every authority this server issues and verifies with, keyed by denomination and
/// expiration date. Interior mutability because rolling happens while the server runs.
pub struct Mint {
    dir: PathBuf,
    coins_per_book: u64,
    n: usize,
    by_denom: RwLock<BTreeMap<u64, Vec<Arc<Authority>>>>,
}

impl Mint {
    /// Load every authority on disk, bootstrapping a first one per denomination if none is
    /// there. Also rolls straight away, so a server that was down over a rotation comes up
    /// with a current epoch rather than an expiring one.
    pub fn load(dir: &Path, coins_per_book: u64, n: usize) -> Self {
        let mut by_denom: BTreeMap<u64, Vec<Arc<Authority>>> = BTreeMap::new();
        for a in read_all(dir) {
            if a.total_coins() == coins_per_book {
                by_denom.entry(a.denom_toku()).or_default().push(Arc::new(a));
            } else {
                // The book size lives in the keys. Never silently replace such an
                // authority: every book a client holds of it would die unannounced.
                eprintln!(
                    "scrai-server: ignoring a {}-coin authority (this mode issues {coins_per_book}-coin books)",
                    a.total_coins()
                );
            }
        }
        for v in by_denom.values_mut() {
            v.sort_by_key(|a| a.expiration_date());
        }
        let mint = Self { dir: dir.to_path_buf(), coins_per_book, n, by_denom: RwLock::new(by_denom) };
        mint.roll();
        mint
    }

    /// Create what is missing and drop what is spent: one authority per denomination whose
    /// expiration is at least `BOOK_VALIDITY_DAYS - ROLL_EVERY_DAYS` away, and no authority
    /// whose books can no longer be spent. Cheap and idempotent — safe to call on a timer.
    pub fn roll(&self) {
        let now = now_secs();
        let fresh_enough = now + (crate::BOOK_VALIDITY_DAYS - ROLL_EVERY_DAYS) * DAY;
        let dead_before = now.saturating_sub(KEEP_PAST_EXPIRY_DAYS * DAY);
        for denom in DENOMS {
            let (newest, count) = {
                let g = self.by_denom.read().expect("mint lock");
                let v = g.get(&denom);
                (v.and_then(|v| v.last().map(|a| a.expiration_date())).unwrap_or(0), v.map_or(0, |v| v.len()))
            };
            if (newest as u64) < fresh_enough {
                let exp = (now / DAY) * DAY + crate::BOOK_VALIDITY_DAYS * DAY;
                // A second authority with the SAME expiration would be a pointless twin —
                // this only happens if the clock jumps backwards.
                if exp as u32 != newest {
                    println!(
                        "scrai-server: rolling a new {denom}-TOKU epoch ({} coins/book, expires {})",
                        self.coins_per_book,
                        day_str(exp)
                    );
                    let a = federation::bootstrap(self.n, self.n as u64, self.coins_per_book, exp as u32, denom)
                        .expect("bootstrap authority")
                        .into_iter()
                        .next()
                        .expect("one authority");
                    let path = self.dir.join(file_for(denom, a.expiration_date()));
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match a.persist() {
                        Ok(json) => {
                            if let Err(e) = crate::write_secret(&path, &json) {
                                // Refuse to issue from an authority that is not on disk: a
                                // restart would forget the key and strand every book of it.
                                eprintln!("scrai-server: could not write {} ({e}) — not using this epoch", path.display());
                                continue;
                            }
                        }
                        Err(e) => {
                            eprintln!("scrai-server: could not serialise a fresh authority ({e})");
                            continue;
                        }
                    }
                    let mut g = self.by_denom.write().expect("mint lock");
                    let v = g.entry(denom).or_default();
                    v.push(Arc::new(a));
                    v.sort_by_key(|a| a.expiration_date());
                }
            }
            let _ = count;
            // Retire what nothing can spend any more — but NEVER the last one. An empty
            // list would leave the server unable to issue or verify anything at all, and a
            // stale key that nobody's books match still refuses them honestly.
            let mut g = self.by_denom.write().expect("mint lock");
            if let Some(v) = g.get_mut(&denom) {
                let newest = v.last().map(|a| a.expiration_date()).unwrap_or(0);
                v.retain(|a| {
                    let exp = a.expiration_date();
                    let dead = (exp as u64) < dead_before && exp != newest;
                    if dead {
                        println!("scrai-server: retiring the {denom}-TOKU epoch that expired {}", day_str(exp as u64));
                        let _ = std::fs::remove_file(self.dir.join(file_for(denom, exp)));
                    }
                    !dead
                });
            }
        }
    }

    /// The authority that issues NEW books of a denomination: the newest one.
    pub fn issuer(&self, denom_toku: u64) -> Arc<Authority> {
        let g = self.by_denom.read().expect("mint lock");
        g.get(&denom_toku)
            .and_then(|v| v.last().cloned())
            .or_else(|| g.get(&COIN_TOKU).and_then(|v| v.last().cloned()))
            .expect("a mint always has an authority")
    }

    /// The authority of one epoch, or the issuer when the caller did not name one.
    pub fn at(&self, denom_toku: u64, expiration_date: u32) -> Option<Arc<Authority>> {
        if expiration_date == 0 {
            return Some(self.issuer(denom_toku));
        }
        let g = self.by_denom.read().expect("mint lock");
        g.get(&denom_toku)?.iter().find(|a| a.expiration_date() == expiration_date).cloned()
    }

    /// The one to answer a request with — `denom_toku` 0 means "did not say".
    pub fn for_request(&self, denom_toku: u64) -> Arc<Authority> {
        self.issuer(if denom_toku == 0 { COIN_TOKU } else { denom_toku })
    }

    pub fn denoms(&self) -> Vec<u64> {
        DENOMS.to_vec()
    }

    /// What every live epoch of every denomination is, for the boot line and the admin.
    pub fn epochs(&self) -> Vec<(u64, u32)> {
        let g = self.by_denom.read().expect("mint lock");
        g.iter().flat_map(|(d, v)| v.iter().map(|a| (*d, a.expiration_date()))).collect()
    }

    /// Verify one note against the key of ITS denomination AND ITS epoch. Both are only
    /// selectors: a note that names the wrong one fails the verification that follows.
    pub fn verify(&self, n: &Note) -> Result<(), String> {
        let pi = n.pay_info()?;
        let Some(a) = self.at(n.denom_toku, n.exp_date) else {
            return Err(format!(
                "this server has no {}-TOKU key for the epoch ending {} — that book has expired",
                n.denom_toku,
                day_str(n.exp_date as u64)
            ));
        };
        if a.denom_toku() != n.denom_toku {
            return Err(format!("this server does not issue {}-TOKU coins", n.denom_toku));
        }
        a.verify_payment(&n.payment, &pi, n.spend_date).map_err(|e| format!("invalid coin: {e}"))
    }
}

/// `authority-<denom>-<expiration>.json`. The two pre-rotation names are still read, so a
/// running deployment keeps the authorities it already issued books from.
fn file_for(denom_toku: u64, expiration_date: u32) -> String {
    format!("authority-{denom_toku}-{expiration_date}.json")
}

fn legacy_files() -> [(&'static str, u64); 2] {
    [("authority.json", COIN_TOKU), ("authority-coarse.json", COARSE_TOKU)]
}

fn read_all(dir: &Path) -> Vec<Authority> {
    let mut out = Vec::new();
    let mut seen: Vec<(u64, u32)> = Vec::new();
    let take = |a: Authority, out: &mut Vec<Authority>, seen: &mut Vec<(u64, u32)>| {
        let k = (a.denom_toku(), a.expiration_date());
        if !seen.contains(&k) {
            seen.push(k);
            out.push(a);
        }
    };
    for (name, denom) in legacy_files() {
        if let Ok(json) = std::fs::read_to_string(dir.join(name)) {
            match Authority::restore(&json) {
                Ok(a) if a.denom_toku() == denom => take(a, &mut out, &mut seen),
                Ok(a) => eprintln!("scrai-server: {name} holds a {}-TOKU authority, expected {denom} — ignored", a.denom_toku()),
                Err(e) => eprintln!("scrai-server: couldn't restore {name} ({e})"),
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("authority-") && name.ends_with(".json")) || name == "authority-coarse.json" {
            continue;
        }
        match std::fs::read_to_string(e.path()).ok().map(|j| Authority::restore(&j)) {
            Some(Ok(a)) => take(a, &mut out, &mut seen),
            Some(Err(err)) => eprintln!("scrai-server: couldn't restore {name} ({err})"),
            None => {}
        }
    }
    out
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A unix day as `YYYY-MM-DD`, for log lines about epochs.
fn day_str(secs: u64) -> String {
    let days = secs / DAY;
    let (mut y, mut d) = (1970i64, days as i64);
    loop {
        let len = if leap(y) { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let months = [31, if leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while m < 12 && d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!("{y}-{:02}-{:02}", m + 1, d + 1)
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own per test. The seconds-resolution clock is NOT enough on its
    /// own: two tests starting in the same second shared a directory and each saw the
    /// other's authorities.
    fn tmp() -> PathBuf {
        use rand::RngCore;
        let d = std::env::temp_dir().join(format!(
            "tokumai-mint-{}-{:08x}",
            std::process::id(),
            rand::thread_rng().next_u32()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A fresh server must come up able to issue BOTH denominations, and calling roll again
    /// must not mint a second epoch of anything — it runs on a timer.
    /// The terms promise a number of days (§6a). Every book of an epoch dies on the same
    /// date, and one can be drawn the moment before the next epoch starts — so the WORST
    /// case a customer can be handed must still be the full promise. This is a legal
    /// statement as much as a technical one: the stamp carries the promise plus a roll.
    #[test]
    fn the_shortest_life_a_book_can_be_given_is_the_one_the_terms_promise() {
        use scrai_core::coconut::{BOOK_VALIDITY_DAYS, PROMISED_VALIDITY_DAYS, ROLL_EVERY_DAYS};
        let worst = BOOK_VALIDITY_DAYS - ROLL_EVERY_DAYS; // drawn just before the next roll
        assert!(
            worst >= PROMISED_VALIDITY_DAYS,
            "a book drawn at the worst moment lives {worst} days, the terms say {PROMISED_VALIDITY_DAYS}"
        );
        // …and the best case is the promise plus one roll, not more: books should not
        // quietly live twice as long as the document says either.
        assert_eq!(BOOK_VALIDITY_DAYS, PROMISED_VALIDITY_DAYS + ROLL_EVERY_DAYS);
    }

    #[test]
    fn a_fresh_mint_has_one_live_epoch_per_denomination_and_rolling_is_idempotent() {
        let dir = tmp();
        let mint = Mint::load(&dir, 10, 1);
        let epochs = mint.epochs();
        assert_eq!(epochs.len(), DENOMS.len(), "one epoch per denomination: {epochs:?}");
        for d in DENOMS {
            let a = mint.issuer(d);
            assert_eq!(a.denom_toku(), d);
            assert_eq!(a.total_coins(), 10);
            let life = (a.expiration_date() as u64).saturating_sub(now_secs()) / DAY;
            assert!(life >= crate::BOOK_VALIDITY_DAYS - 1, "a fresh book lives {life} days");
            // On disk under its own name, or a restart would forget the key and strand
            // every book issued from it.
            assert!(dir.join(file_for(d, a.expiration_date())).exists(), "{d} epoch persisted");
        }
        mint.roll();
        assert_eq!(mint.epochs().len(), epochs.len(), "rolling twice in a day changes nothing");

        // And a restart finds exactly what was left behind.
        let again = Mint::load(&dir, 10, 1);
        assert_eq!(again.epochs(), epochs, "the same epochs come back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pre-rotation file names still hold the authorities a running deployment issued
    /// books from. Losing them would strand every book on every device.
    #[test]
    fn the_authorities_from_before_rotation_are_still_read() {
        let dir = tmp();
        let fine = federation::bootstrap(1, 1, 10, (now_secs() + 40 * DAY) as u32, COIN_TOKU)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let exp = fine.expiration_date();
        std::fs::write(dir.join("authority.json"), fine.persist().unwrap()).unwrap();

        let mint = Mint::load(&dir, 10, 1);
        assert!(mint.at(COIN_TOKU, exp).is_some(), "the old epoch is still served");
        // …and it rolled a current one beside it, because that one is over a week old.
        assert!(mint.issuer(COIN_TOKU).expiration_date() > exp, "a fresher epoch was rolled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_day_prints_as_a_date() {
        assert_eq!(day_str(0), "1970-01-01");
        assert_eq!(day_str(1_758_240_000), "2025-09-19");
        // A leap day, and the day after.
        assert_eq!(day_str(1_709_164_800), "2024-02-29");
        assert_eq!(day_str(1_709_251_200), "2024-03-01");
    }

    #[test]
    fn an_epoch_file_names_its_denomination_and_date() {
        assert_eq!(file_for(1000, 1_760_400_000), "authority-1000-1760400000.json");
    }
}
