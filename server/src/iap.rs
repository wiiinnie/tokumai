// ---------------------------------------------------------------------------
// iap.rs — App Store purchases (StoreKit 2), verified here, credited like a voucher.
//
// The app hands us the transaction exactly as Apple signed it: a JWS whose header carries
// Apple's certificate chain (`x5c`). Nothing is fetched from Apple and no API key is
// involved — the chain is checked against the Apple Root CA G3 pinned into this binary,
// the leaf's key checks the signature, and the payload says what was bought. What Apple
// learns from this design is what it already knew: that somebody bought a product. It
// does not learn which tokumai account, and this server does not learn who at Apple.
//
// Same shape as a voucher afterwards: the transaction's fingerprint is burned in the
// database (once, atomically), the entitlement is credited in the pay snapshot, and the
// account link is dropped after ACCOUNT_LINK_DAYS like every other purchase.
// ---------------------------------------------------------------------------

use base64::Engine;
use serde_json::Value;
use x509_parser::prelude::*;

pub const IAP_KIND: &str = "iap.verify";

/// Apple Root CA - G3 (DER), from https://www.apple.com/certificateauthority/ —
/// SHA-256 63:34:3A:BF:B8:9A:6A:03:EB:B5:7E:9B:3F:5F:A7:BE:7C:4F:5C:75:6F:30:17:B3:A8:C4:88:C3:65:3E:91:79.
/// Every App Store JWS chain ends here; a chain that does not is not Apple's.
const APPLE_ROOT_CA_G3: &[u8] = include_bytes!("apple_root_ca_g3.der");

/// What one verified transaction says. Only the fields a credit decision needs.
#[derive(Debug, Clone, PartialEq)]
pub struct AppleTx {
    pub bundle_id: String,
    pub product_id: String,
    pub transaction_id: String,
    pub original_transaction_id: String,
    /// "Sandbox" or "Production".
    pub environment: String,
    /// "Consumable", "Non-Consumable", …
    pub kind: String,
    pub quantity: u32,
    pub purchased_at_ms: u64,
    pub revoked: bool,
    pub ownership: String,
    /// Apple's storefront (ISO 3166 alpha-3), for the bookkeeping country column.
    pub storefront: String,
    /// When an auto-renewable subscription's paid period ends. 0 for a consumable.
    pub expires_at_ms: u64,
}

/// The product-id prefix the tiles hang off: `<prefix><usd>` — `com.tokumai.app.credit.10`.
pub fn product_prefix() -> String {
    crate::cfg("IAP_PRODUCT_PREFIX")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "com.tokumai.app.credit.".to_string())
}

/// The bundle id a transaction must name (`IAP_BUNDLE_ID`, default com.tokumai.app).
pub fn bundle_id() -> String {
    crate::cfg("IAP_BUNDLE_ID")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "com.tokumai.app".to_string())
}

/// Tiles sold through the App Store (`IAP_TIERS`, default 10,20,50 — the $5 tile is
/// deliberately not on iOS). Each must also be a purchase tier, so the credited amount
/// is one the other rails produce too: an App Store buyer must not be recognisable by a
/// bucket size of their own.
pub fn tiers() -> Vec<u32> {
    let all = crate::pay::purchase_tiers();
    crate::cfg("IAP_TIERS")
        .ok()
        .map(|s| s.split(',').filter_map(|t| t.trim().parse().ok()).collect::<Vec<u32>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![10, 20, 50])
        .into_iter()
        .filter(|t| all.contains(t))
        .collect()
}

/// The product ids the app asks StoreKit for, in tile order.
pub fn product_ids() -> Vec<String> {
    let p = product_prefix();
    tiers().into_iter().map(|t| format!("{p}{t}")).collect()
}

/// Which tile a product id is — None for anything not on sale.
pub fn product_usd(product_id: &str) -> Option<u32> {
    let p = product_prefix();
    let rest = product_id.strip_prefix(p.as_str())?;
    let usd: u32 = rest.parse().ok()?;
    tiers().contains(&usd).then_some(usd)
}

/// The product-id prefix for the monthly plans: `<prefix><euro>` — `com.tokumai.app.plan.10`,
/// with `.year` appended for the annual version of the same tier.
pub fn plan_prefix() -> String {
    crate::cfg("IAP_PLAN_PREFIX")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "com.tokumai.app.plan.".to_string())
}

/// The subscription product ids, in tier order: monthly then yearly.
pub fn plan_ids() -> Vec<String> {
    let p = plan_prefix();
    let monthly = scrai_core::subscription::TIERS.iter().map(|(_, cents)| format!("{p}{}", cents / 100));
    let yearly: Vec<String> =
        scrai_core::subscription::TIERS.iter().map(|(_, cents)| format!("{p}{}.year", cents / 100)).collect();
    monthly.chain(yearly).collect()
}

/// Which plan a product id is: (tier index, yearly?). None for anything that is not a plan.
pub fn product_plan(product_id: &str) -> Option<(usize, bool)> {
    let rest = product_id.strip_prefix(plan_prefix().as_str())?;
    let (euro, yearly) = match rest.strip_suffix(".year") {
        Some(e) => (e, true),
        None => (rest, false),
    };
    let cents: u64 = euro.parse::<u64>().ok()? * 100;
    let tier = scrai_core::subscription::TIERS.iter().position(|(_, c)| *c == cents)?;
    Some((tier, yearly))
}

/// Does a verified transaction buy a SUBSCRIPTION here, and which one? The checks mirror
/// `credit_for`; what differs is the product type and that a lapsed period is refused —
/// Apple keeps handing the app the last transaction of a subscription that has ended, and
/// treating that as "still paid" would give away months.
pub fn plan_for(tx: &AppleTx, now_ms: u64) -> Result<(usize, bool), String> {
    if tx.bundle_id != bundle_id() {
        return Err("this purchase belongs to another app".into());
    }
    if tx.kind != "Auto-Renewable Subscription" {
        return Err("this purchase is not a plan".into());
    }
    if tx.revoked {
        return Err("this subscription was refunded by Apple".into());
    }
    if tx.ownership != "PURCHASED" {
        return Err("this subscription is not yours".into());
    }
    if tx.original_transaction_id.is_empty() {
        return Err("this subscription has no transaction number".into());
    }
    match tx.environment.as_str() {
        "Production" => {}
        "Sandbox" if allow_sandbox() => {}
        "Sandbox" => return Err("sandbox purchases are not accepted by this server".into()),
        _ => return Err("unknown App Store environment".into()),
    }
    if tx.expires_at_ms == 0 || tx.expires_at_ms <= now_ms {
        return Err("this subscription has ended".into());
    }
    product_plan(&tx.product_id).ok_or_else(|| "this plan is not on sale here".into())
}

/// Sandbox transactions credit real entitlement only when the operator says so
/// (`IAP_ALLOW_SANDBOX=1`) — on the production server a sandbox receipt is free money.
pub fn allow_sandbox() -> bool {
    crate::cfg("IAP_ALLOW_SANDBOX").map(|v| v.trim() == "1").unwrap_or(false)
}

/// The fingerprint the table is keyed by. Apple's transaction ids are sequential-looking
/// numbers that mean nothing without Apple; the hash keeps them out of a dump anyway.
pub fn tx_hash(transaction_id: &str) -> String {
    let mut bytes = b"apple-tx:".to_vec();
    bytes.extend_from_slice(transaction_id.as_bytes());
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
}

fn b64url(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .map_err(|_| "malformed transaction (base64)".to_string())
}

/// Verify an App Store JWS and read the transaction it carries. `now_ms` is the clock the
/// certificate validity is checked against.
///
/// Checks, in order: three-part compact JWS; `alg` ES256; an `x5c` chain of at least two
/// certificates in which each is signed by the next and the last is (or is signed by) the
/// pinned Apple root; every certificate valid now; the JWS signature under the leaf key.
/// Then the payload is parsed — the caller decides what to do with it.
pub fn verify_jws(jws: &str, now_ms: u64) -> Result<AppleTx, String> {
    let mut parts = jws.split('.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => return Err("malformed transaction (not a compact JWS)".into()),
    };
    let header: Value = serde_json::from_slice(&b64url(h)?).map_err(|_| "malformed transaction (header)")?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("ES256") {
        return Err("unexpected signature algorithm".into());
    }
    let chain_b64 = header
        .get("x5c")
        .and_then(|x| x.as_array())
        .filter(|x| x.len() >= 2 && x.len() <= 5)
        .ok_or("transaction carries no certificate chain")?;
    let mut chain: Vec<Vec<u8>> = Vec::with_capacity(chain_b64.len());
    for c in chain_b64 {
        let s = c.as_str().ok_or("malformed certificate chain")?;
        chain.push(
            base64::engine::general_purpose::STANDARD
                .decode(s.trim())
                .map_err(|_| "malformed certificate chain (base64)".to_string())?,
        );
    }
    let certs: Vec<X509Certificate> = chain
        .iter()
        .map(|der| X509Certificate::from_der(der).map(|(_, c)| c).map_err(|_| "malformed certificate".to_string()))
        .collect::<Result<_, _>>()?;
    let (_, root) = X509Certificate::from_der(APPLE_ROOT_CA_G3).map_err(|_| "pinned root unreadable".to_string())?;
    let at = ASN1Time::from_timestamp((now_ms / 1000) as i64).map_err(|_| "clock".to_string())?;
    for (i, cert) in certs.iter().enumerate() {
        if !cert.validity().is_valid_at(at) {
            return Err("a certificate in the chain is not valid now".into());
        }
        let issuer = certs.get(i + 1);
        match issuer {
            Some(next) => cert
                .verify_signature(Some(next.public_key()))
                .map_err(|_| "certificate chain does not verify".to_string())?,
            None => {
                // The last one: Apple includes the root itself; accept that (byte-equal to
                // the pin) or a chain that stops one short and is signed by the pin.
                if chain[i] != APPLE_ROOT_CA_G3 {
                    cert.verify_signature(Some(root.public_key()))
                        .map_err(|_| "certificate chain does not end at Apple's root".to_string())?;
                }
            }
        }
    }
    // The leaf signs the JWS: ES256 = ECDSA P-256 over SHA-256, signature as r||s (64 bytes).
    let leaf_key = certs[0].public_key().subject_public_key.data.as_ref();
    let sig = b64url(s)?;
    if sig.len() != 64 {
        return Err("malformed transaction (signature)".into());
    }
    let signed = format!("{h}.{p}");
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, leaf_key)
        .verify(signed.as_bytes(), &sig)
        .map_err(|_| "transaction signature does not verify".to_string())?;

    let body: Value = serde_json::from_slice(&b64url(p)?).map_err(|_| "malformed transaction (payload)")?;
    let str_of = |k: &str| body.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    Ok(AppleTx {
        bundle_id: str_of("bundleId"),
        product_id: str_of("productId"),
        transaction_id: str_of("transactionId"),
        original_transaction_id: str_of("originalTransactionId"),
        environment: str_of("environment"),
        kind: str_of("type"),
        quantity: body.get("quantity").and_then(|q| q.as_u64()).unwrap_or(1).min(100) as u32,
        purchased_at_ms: body.get("purchaseDate").and_then(|d| d.as_u64()).unwrap_or(0),
        revoked: body.get("revocationDate").is_some(),
        ownership: str_of("inAppOwnershipType"),
        storefront: str_of("storefront"),
        expires_at_ms: body.get("expiresDate").and_then(|d| d.as_u64()).unwrap_or(0),
    })
}

/// Does a verified transaction buy credit here, and how much? Everything that is not a
/// plain, unrevoked, owned consumable of one of our tiles for our bundle is refused with a
/// reason the app can show.
pub fn credit_for(tx: &AppleTx) -> Result<u64, String> {
    if tx.bundle_id != bundle_id() {
        return Err("this purchase belongs to another app".into());
    }
    if tx.kind != "Consumable" {
        return Err("this purchase is not a credit product".into());
    }
    if tx.revoked {
        return Err("this purchase was refunded by Apple".into());
    }
    if tx.ownership != "PURCHASED" {
        return Err("this purchase is not yours to redeem".into());
    }
    if tx.transaction_id.is_empty() {
        return Err("this purchase has no transaction number".into());
    }
    match tx.environment.as_str() {
        "Production" => {}
        "Sandbox" if allow_sandbox() => {}
        "Sandbox" => return Err("sandbox purchases are not accepted by this server".into()),
        _ => return Err("unknown App Store environment".into()),
    }
    let usd = product_usd(&tx.product_id).ok_or("this product is not on sale here")?;
    let q = tx.quantity.clamp(1, 10) as u64;
    Ok(usd as u64 * scrai_core::coconut::TOKU_PER_USD * q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(product: &str) -> AppleTx {
        AppleTx {
            bundle_id: "com.tokumai.app".into(),
            product_id: product.into(),
            transaction_id: "2000000123456789".into(),
            original_transaction_id: "2000000123456789".into(),
            environment: "Production".into(),
            kind: "Consumable".into(),
            quantity: 1,
            purchased_at_ms: 1_757_500_000_000,
            revoked: false,
            ownership: "PURCHASED".into(),
            storefront: "DEU".into(),
            expires_at_ms: 0,
        }
    }

    #[test]
    fn product_ids_follow_the_tiles_and_only_known_ones_price() {
        let ids = product_ids();
        assert_eq!(ids, vec!["com.tokumai.app.credit.10", "com.tokumai.app.credit.20", "com.tokumai.app.credit.50"]);
        assert_eq!(product_usd("com.tokumai.app.credit.20"), Some(20));
        assert_eq!(product_usd("com.tokumai.app.credit.5"), None, "the $5 tile is not on iOS");
        assert_eq!(product_usd("com.tokumai.app.credit.10x"), None);
        assert_eq!(product_usd("com.other.app.credit.10"), None);
    }

    #[test]
    fn credit_is_the_tile_times_the_unit_and_refuses_what_is_not_ours() {
        assert_eq!(credit_for(&tx("com.tokumai.app.credit.10")), Ok(10 * scrai_core::coconut::TOKU_PER_USD));
        let mut t = tx("com.tokumai.app.credit.10");
        t.bundle_id = "com.example.other".into();
        assert!(credit_for(&t).is_err());
        let mut t = tx("com.tokumai.app.credit.10");
        t.revoked = true;
        assert!(credit_for(&t).unwrap_err().contains("refunded"));
        let mut t = tx("com.tokumai.app.credit.10");
        t.environment = "Sandbox".into();
        assert!(credit_for(&t).unwrap_err().contains("sandbox"), "sandbox is refused unless the operator allows it");
        let mut t = tx("com.tokumai.app.credit.10");
        t.kind = "Non-Consumable".into();
        assert!(credit_for(&t).is_err());
        assert!(credit_for(&tx("com.tokumai.app.credit.5")).is_err());
    }

    /// A plan transaction, valid for another month.
    fn plan_tx(product: &str) -> AppleTx {
        let mut t = tx(product);
        t.kind = "Auto-Renewable Subscription".into();
        t.original_transaction_id = "2000000111222333".into();
        t.expires_at_ms = 2_000_000_000_000;
        t
    }

    #[test]
    fn plan_ids_follow_the_tiers_and_only_known_ones_map_back() {
        let ids = plan_ids();
        assert!(ids.contains(&"com.tokumai.app.plan.10".to_string()));
        assert!(ids.contains(&"com.tokumai.app.plan.50.year".to_string()));
        assert_eq!(ids.len(), scrai_core::subscription::TIERS.len() * 2);
        assert_eq!(product_plan("com.tokumai.app.plan.10"), Some((0, false)));
        assert_eq!(product_plan("com.tokumai.app.plan.20"), Some((1, false)));
        assert_eq!(product_plan("com.tokumai.app.plan.50.year"), Some((2, true)));
        // A price we do not sell, and a credit tile, are not plans.
        assert_eq!(product_plan("com.tokumai.app.plan.99"), None);
        assert_eq!(product_plan("com.tokumai.app.credit.10"), None);
    }

    #[test]
    fn a_plan_is_accepted_only_while_it_is_paid_for() {
        let now = 1_789_000_000_000;
        assert_eq!(plan_for(&plan_tx("com.tokumai.app.plan.20"), now), Ok((1, false)));

        // Apple keeps handing the app the LAST transaction of a subscription that has
        // ended. Treating that as "still paid" would give away months, so an expired
        // period is refused even though everything else about it checks out.
        let mut ended = plan_tx("com.tokumai.app.plan.20");
        ended.expires_at_ms = now - 1;
        assert!(plan_for(&ended, now).is_err());
        let mut never = plan_tx("com.tokumai.app.plan.20");
        never.expires_at_ms = 0;
        assert!(plan_for(&never, now).is_err());

        // A refund, another app's purchase, a family-shared entitlement, and a consumable
        // are each refused with their own reason.
        let mut refunded = plan_tx("com.tokumai.app.plan.20");
        refunded.revoked = true;
        assert!(plan_for(&refunded, now).is_err());
        let mut other = plan_tx("com.tokumai.app.plan.20");
        other.bundle_id = "com.someone.else".into();
        assert!(plan_for(&other, now).is_err());
        let mut shared = plan_tx("com.tokumai.app.plan.20");
        shared.ownership = "FAMILY_SHARED".into();
        assert!(plan_for(&shared, now).is_err());
        assert!(plan_for(&tx("com.tokumai.app.credit.10"), now).is_err());

        // …and a credit tile is still a credit tile: the two paths do not cross.
        assert!(credit_for(&plan_tx("com.tokumai.app.plan.20")).is_err());
    }

    #[test]
    fn the_fingerprint_is_stable_and_not_the_id() {
        let h = tx_hash("2000000123456789");
        assert_eq!(h.len(), 64);
        assert_eq!(h, tx_hash("2000000123456789"));
        assert!(!h.contains("2000000123456789"));
    }

    #[test]
    fn the_pinned_root_is_apple_root_ca_g3() {
        let (_, root) = X509Certificate::from_der(APPLE_ROOT_CA_G3).unwrap();
        assert!(root.subject().to_string().contains("Apple Root CA - G3"));
        assert!(root.verify_signature(None).is_ok(), "self-signed");
    }

    #[test]
    fn a_jws_that_is_not_apples_is_refused_before_any_payload_is_read() {
        assert!(verify_jws("nope", 0).is_err());
        assert!(verify_jws("a.b", 0).is_err());
        // Right shape, wrong algorithm.
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","x5c":["AA","AA"]}"#);
        assert_eq!(verify_jws(&format!("{hdr}.e30.AA"), 0).unwrap_err(), "unexpected signature algorithm");
        // ES256 but no chain.
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256"}"#);
        assert!(verify_jws(&format!("{hdr}.e30.AA"), 0).unwrap_err().contains("no certificate chain"));
        // A chain that is not certificates at all.
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","x5c":["AAAA","AAAA"]}"#);
        assert!(verify_jws(&format!("{hdr}.e30.AA"), 0).unwrap_err().contains("malformed certificate"));
        // A chain of Apple's root alone, twice: valid certificates, but the "leaf" is the
        // root and the JWS signature cannot verify under it.
        let root_b64 = base64::engine::general_purpose::STANDARD.encode(APPLE_ROOT_CA_G3);
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"alg":"ES256","x5c":["{root_b64}","{root_b64}"]}}"#));
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 64]);
        let err = verify_jws(&format!("{hdr}.e30.{sig}"), 1_757_500_000_000).unwrap_err();
        assert!(err.contains("signature does not verify"), "{err}");
    }
}
