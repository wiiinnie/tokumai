#![allow(dead_code)]
// ---------------------------------------------------------------------------
// ecash.rs — client-side blind-signature ecash (BDHKE over secp256k1), ported
// from blind.ts / ecash-client.ts.
//
// Byte-compatible with the TS client so the same scrai-server verifies these
// tokens: same hash-to-curve, same compressed-point encoding, same DLEQ hash.
// The client blinds secrets, unblinds the issuer's signatures (checking the
// DLEQ), and decomposes amounts into tier packets. The mint (private) half
// lives on the server.
// ---------------------------------------------------------------------------

use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use k256::{AffinePoint, EncodedPoint, FieldBytes, ProjectivePoint, Scalar, U256};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---- wire types (shared with the protocol) --------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct BlindedOutput {
    pub amount: u64,
    #[serde(rename = "B_")]
    pub b_: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SignedOutput {
    pub amount: u64,
    #[serde(rename = "C_")]
    pub c_: String,
    pub e: String,
    pub s: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Proof {
    pub amount: u64,
    pub secret: String,
    #[serde(rename = "C")]
    pub c: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PublicKey {
    pub amount: u64,
    pub pubkey: String,
}

/// Per-output secret kept between blinding and unblinding.
pub struct OutputState {
    pub amount: u64,
    pub secret: [u8; 32],
    pub r: Scalar,
    pub b_: ProjectivePoint,
}

// ---- primitives -----------------------------------------------------------

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn scalar_from_bytes(b: &[u8; 32]) -> Scalar {
    <Scalar as Reduce<U256>>::reduce_bytes(FieldBytes::from_slice(b))
}

fn rand_scalar() -> Scalar {
    let mut rng = rand::thread_rng();
    loop {
        let mut b = [0u8; 32];
        rng.fill_bytes(&mut b);
        let s = scalar_from_bytes(&b);
        if !bool::from(s.is_zero()) {
            return s;
        }
    }
}

fn point_to_hex(p: &ProjectivePoint) -> String {
    hex::encode(p.to_affine().to_encoded_point(true).as_bytes())
}

fn point_from_bytes(bytes: &[u8]) -> Option<ProjectivePoint> {
    let ep = EncodedPoint::from_bytes(bytes).ok()?;
    let aff: Option<AffinePoint> = AffinePoint::from_encoded_point(&ep).into();
    aff.map(ProjectivePoint::from)
}

fn point_from_hex(h: &str) -> Option<ProjectivePoint> {
    point_from_bytes(&hex::decode(h).ok()?)
}

fn scalar_from_hex(h: &str) -> Option<Scalar> {
    let v = hex::decode(h).ok()?;
    if v.len() != 32 {
        return None;
    }
    let mut b = [0u8; 32];
    b.copy_from_slice(&v);
    Some(scalar_from_bytes(&b))
}

/// Map a secret to a curve point — Cashu NUT-00 hash-to-curve. Must match the TS
/// client exactly, or the server recomputes a different point and rejects.
const H2C_DOMAIN: &[u8] = b"Secp256k1_HashToCurve_Cashu_";

pub fn hash_to_curve(secret: &[u8]) -> ProjectivePoint {
    let msg_hash = sha256(&[H2C_DOMAIN, secret]);
    for counter in 0u32..0x10000 {
        let ctr = counter.to_le_bytes();
        let cand = sha256(&[&msg_hash, &ctr]);
        let mut comp = [0u8; 33];
        comp[0] = 0x02;
        comp[1..].copy_from_slice(&cand);
        if let Some(p) = point_from_bytes(&comp) {
            return p;
        }
    }
    panic!("hash_to_curve: no valid point");
}

pub fn new_secret() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// Blind one secret: B_ = Y + r·G.
pub fn blind(secret: &[u8; 32]) -> (ProjectivePoint, Scalar) {
    let y = hash_to_curve(secret);
    let r = rand_scalar();
    (y + ProjectivePoint::GENERATOR * r, r)
}

/// Unblind: C = C_ − r·A = a·Y. Returns the compressed C, hex.
pub fn unblind(c_bytes_hex: &str, r: &Scalar, a_hex: &str) -> Option<String> {
    let c_ = point_from_hex(c_bytes_hex)?;
    let a = point_from_hex(a_hex)?;
    Some(point_to_hex(&(c_ - a * *r)))
}

/// DLEQ Fiat–Shamir challenge = H(R1 ‖ R2 ‖ A ‖ C_) reduced to a scalar.
fn dleq_challenge(r1: &ProjectivePoint, r2: &ProjectivePoint, a: &ProjectivePoint, c_: &ProjectivePoint) -> Scalar {
    let h = sha256(&[
        r1.to_affine().to_encoded_point(true).as_bytes(),
        r2.to_affine().to_encoded_point(true).as_bytes(),
        a.to_affine().to_encoded_point(true).as_bytes(),
        c_.to_affine().to_encoded_point(true).as_bytes(),
    ]);
    scalar_from_bytes(&h)
}

/// Verify the issuer's DLEQ against the PUBLISHED key — the anti-tagging check.
pub fn verify_dleq(a_hex: &str, b_hex: &str, sig: &SignedOutput) -> bool {
    let (Some(a), Some(b_), Some(c_), Some(e), Some(s)) = (
        point_from_hex(a_hex),
        point_from_hex(b_hex),
        point_from_hex(&sig.c_),
        scalar_from_hex(&sig.e),
        scalar_from_hex(&sig.s),
    ) else {
        return false;
    };
    if bool::from(e.is_zero()) || bool::from(s.is_zero()) {
        return false;
    }
    let g = ProjectivePoint::GENERATOR;
    let r1 = g * s - a * e;
    let r2 = b_ * s - c_ * e;
    dleq_challenge(&r1, &r2, &a, &c_) == e
}

// ---- packet helpers (client flow) -----------------------------------------

/// Power-of-two denominations, 1 .. 2^30.
pub fn denominations() -> Vec<u64> {
    (0..31).map(|i| 1u64 << i).collect()
}

pub fn decompose(amount: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut rem = amount;
    for d in denominations().into_iter().rev() {
        while rem >= d {
            out.push(d);
            rem -= d;
        }
    }
    out
}

/// Split an entitlement into purchase-tier packets (largest first).
pub fn tier_packets(entitlement_scrai: u64, tiers_scrai: &[u64]) -> Vec<u64> {
    let mut sorted: Vec<u64> = tiers_scrai.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = Vec::new();
    let mut rem = entitlement_scrai;
    for t in sorted {
        while rem >= t {
            out.push(t);
            rem -= t;
        }
    }
    if rem > 0 {
        out.push(rem);
    }
    out
}

/// Blind one packet (a tier's worth): a fresh secret per denomination.
pub fn blind_packet(amount_scrai: u64) -> (Vec<BlindedOutput>, Vec<OutputState>) {
    let mut outputs = Vec::new();
    let mut state = Vec::new();
    for denom in decompose(amount_scrai) {
        let secret = new_secret();
        let (b_, r) = blind(&secret);
        outputs.push(BlindedOutput { amount: denom, b_: point_to_hex(&b_) });
        state.push(OutputState { amount: denom, secret, r, b_ });
    }
    (outputs, state)
}

/// Unblind the issuer's signatures into proofs, verifying the DLEQ first.
pub fn unblind_packet(state: &[OutputState], signatures: &[SignedOutput], keys: &[PublicKey]) -> Result<Vec<Proof>, String> {
    let mut out = Vec::new();
    for (i, sig) in signatures.iter().enumerate() {
        let st = state.get(i).ok_or("issuer returned too many signatures")?;
        if sig.amount != st.amount {
            return Err("issuer returned mismatched signatures".into());
        }
        let a_hex = keys
            .iter()
            .find(|k| k.amount == st.amount)
            .map(|k| k.pubkey.clone())
            .ok_or_else(|| format!("issuer has no key for denomination {}", st.amount))?;
        if !verify_dleq(&a_hex, &point_to_hex(&st.b_), sig) {
            return Err("the issuer's DLEQ proof failed — refusing the token (possible tagging)".into());
        }
        let c = unblind(&sig.c_, &st.r, &a_hex).ok_or("unblind failed")?;
        out.push(Proof { amount: st.amount, secret: hex::encode(st.secret), c });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Reference values generated from the TS client (src/money/blind.ts) for a
    // fixed secret 01..20 and mint scalar a. If any drift, the Rust client would
    // produce tokens the scrai-server rejects.
    const SECRET_HEX: &str = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
    const A: &str = "0307288cbce2c8e09ea540a3ac5d3967908fd9ea9213c15a7a9ae6f58458890c3d";
    const Y: &str = "022e110e61615225832bcd673eed3aabf7ee1da3581a3a79cd91b7cc22d9b899dc";
    const B_: &str = "033a4c066edaba6937e46c19387daf32e645aba971fae81a59df4adf10b991587c";
    const R: &str = "ad5c9c500805cfc654d6a6b386cd1ccbb37319dedf7650725ca2b3d056a61504";
    const C_: &str = "0275b4b574c2265ef4394ee6385edf314de15ac6317aa536443b937f1973ee8762";
    const E: &str = "0cee0522af0834355c82ae05eca5c81fb0f1b19fc0249f967a1a658657f3398c";
    const S: &str = "95b1faea6d8bf40fd69d66ec695555c4f6477cd1403b5471c4ba101b1ae4c0c7";
    const C: &str = "030ee6ab02bb7e3df1c4f059f341184abaa4cf3e8aebbfc1bc759e8831f954c90c";

    fn secret() -> [u8; 32] {
        let mut b = [0u8; 32];
        b.copy_from_slice(&hex::decode(SECRET_HEX).unwrap());
        b
    }

    #[test]
    fn hash_to_curve_matches_ts() {
        assert_eq!(point_to_hex(&hash_to_curve(&secret())), Y);
    }

    #[test]
    fn unblind_matches_ts() {
        let r = scalar_from_hex(R).unwrap();
        assert_eq!(unblind(C_, &r, A).unwrap(), C);
    }

    #[test]
    fn verify_dleq_accepts_ts_proof() {
        let sig = SignedOutput { amount: 0, c_: C_.into(), e: E.into(), s: S.into() };
        assert!(verify_dleq(A, B_, &sig));
        // A tampered response must fail.
        let bad = SignedOutput { amount: 0, c_: C_.into(), e: E.into(), s: R.into() };
        assert!(!verify_dleq(A, B_, &bad));
    }

    #[test]
    fn full_roundtrip_self_consistent() {
        // Blind a fresh secret, "sign" with a known scalar (server's job), unblind,
        // and check C == a·H2C(secret) — proving the client math is internally sound.
        let a_scalar = scalar_from_hex(&format!("{:0>64}", "2a3b07")).unwrap();
        let a_point = ProjectivePoint::GENERATOR * a_scalar;
        let a_hex = point_to_hex(&a_point);
        let sec = new_secret();
        let (b_, r) = blind(&sec);
        let c_ = b_ * a_scalar; // server signs
        let c = unblind(&point_to_hex(&c_), &r, &a_hex).unwrap();
        let expected = point_to_hex(&(hash_to_curve(&sec) * a_scalar));
        assert_eq!(c, expected);
    }

    #[test]
    fn decompose_sums_back() {
        for amt in [1u64, 7, 1000, 100_000, 999_999] {
            assert_eq!(decompose(amt).iter().sum::<u64>(), amt);
        }
        assert_eq!(decompose(1000), vec![512, 256, 128, 64, 32, 8]);
    }
}
