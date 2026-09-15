// ---------------------------------------------------------------------------
// mint.rs — the server's issuing authorities, one per denomination.
//
// Compact ecash has no value field inside a coin: what a coin is worth is decided by the
// KEY that signed it. Two denominations therefore mean two authorities, each with its own
// key share, its own epoch material and its own ticketbook size. A note says which one it
// belongs to, and that claim is not trusted — it only picks the key the payment is
// verified against, so a note claiming to be coarse while carrying fine coins fails.
//
// Why two at all: a payment costs ~490 bytes and ~4 ms of pairings PER COIN, and a request
// must tender its CEILING. A 9 ¢ picture at 0.1 ¢ per coin put ninety coins (44 KB) on the
// table — more notes than a tender may carry. Nine coarse coins plus a fine remainder is
// the same 9 ¢, exact to a tenth of a cent, in a fifth of the bytes.
// ---------------------------------------------------------------------------
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use scrai_core::coconut::{COARSE_TOKU, COIN_TOKU, DENOMS};
use scrai_core::federation::{self, Authority};
use scrai_core::tender::Note;

/// Every authority this server issues and verifies with, by TOKU per coin.
pub struct Mint {
    by_denom: BTreeMap<u64, Arc<Authority>>,
}

impl Mint {
    pub fn new(authorities: Vec<Authority>) -> Self {
        Self {
            by_denom: authorities.into_iter().map(|a| (a.denom_toku(), Arc::new(a))).collect(),
        }
    }

    /// The authority for one denomination, if this server issues it.
    pub fn get(&self, denom_toku: u64) -> Option<&Arc<Authority>> {
        self.by_denom.get(&denom_toku)
    }

    /// The fine authority — the default for anything that does not name a denomination
    /// (the legacy session redeem, and apps older than the second denomination).
    pub fn fine(&self) -> &Arc<Authority> {
        self.by_denom.get(&COIN_TOKU).expect("a mint always has a fine authority")
    }

    /// The one to answer a request with. `denom_toku` of 0 means "unspecified".
    pub fn for_request(&self, denom_toku: u64) -> &Arc<Authority> {
        self.by_denom.get(&denom_toku).unwrap_or_else(|| self.fine())
    }

    pub fn denoms(&self) -> Vec<u64> {
        self.by_denom.keys().copied().collect()
    }

    /// Verify one note against the authority of ITS denomination. An unknown denomination
    /// is refused rather than quietly verified against the wrong key.
    pub fn verify(&self, n: &Note) -> Result<(), String> {
        let pi = n.pay_info()?;
        let Some(a) = self.get(n.denom_toku) else {
            return Err(format!("this server does not issue {}-TOKU coins", n.denom_toku));
        };
        a.verify_payment(&n.payment, &pi, n.spend_date).map_err(|e| format!("invalid coin: {e}"))
    }
}

/// Load every denomination's authority, bootstrapping any that is missing.
///
/// The fine one keeps the historical file name, so an existing deployment is not disturbed
/// by the coarse one appearing beside it.
pub fn load(dir: &Path, coins_per_book: u64, expiration_date: u32, n: usize) -> Mint {
    let mut out = Vec::new();
    for denom in DENOMS {
        let path = dir.join(file_for(denom));
        out.push(load_one(&path, coins_per_book, expiration_date, n, denom));
    }
    Mint::new(out)
}

fn file_for(denom_toku: u64) -> &'static str {
    if denom_toku == COARSE_TOKU {
        "authority-coarse.json"
    } else {
        "authority.json"
    }
}

/// Load one authority, or bootstrap and persist it on first run. A persisted authority
/// whose book size or denomination differs from what this mode wants is NOT silently
/// replaced: every book a client holds of it would die unannounced.
fn load_one(path: &Path, coins_per_book: u64, expiration_date: u32, n: usize, denom_toku: u64) -> Authority {
    if let Ok(json) = std::fs::read_to_string(path) {
        match Authority::restore(&json) {
            Ok(a) if a.total_coins() != coins_per_book || a.denom_toku() != denom_toku => {
                eprintln!(
                    "scrai-server: {} holds a {}-coin/{}-TOKU authority but this mode needs \
                     {}-coin/{}-TOKU books. Re-bootstrapping invalidates every ticketbook \
                     clients hold of it. To proceed on purpose: stop the server, move that \
                     file away, start again.",
                    path.display(),
                    a.total_coins(),
                    a.denom_toku(),
                    coins_per_book,
                    denom_toku
                );
                std::process::exit(1);
            }
            Ok(a) => return a,
            Err(e) => eprintln!("scrai-server: couldn't restore {} ({e}) — re-bootstrapping", path.display()),
        }
    }
    let book = coins_per_book * denom_toku;
    println!(
        "scrai-server: bootstrapping a {coins_per_book}-coin authority at {denom_toku} TOKU per coin (book = ${:.2})",
        book as f64 / scrai_core::coconut::TOKU_PER_USD as f64
    );
    let a = federation::bootstrap(n, n as u64, coins_per_book, expiration_date, denom_toku)
        .expect("bootstrap authority")
        .into_iter()
        .next()
        .expect("one authority");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::write_secret(path, &a.persist().expect("persist authority")).expect("write authority file");
    println!("scrai-server: bootstrapped a fresh authority → {}", path.display());
    a
}
