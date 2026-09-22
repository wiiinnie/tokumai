//! App Store purchases (StoreKit 2): plans (auto-renewable subscriptions) and prepaid credit
//! (consumables), verified inside the enclave. Carried over from the first server, with the
//! chain check of the audit of 2026-09-21 (H6) and one change of principle: every switch that
//! decides what counts as a purchase — the bundle id, the product ids, whether sandbox
//! purchases are accepted — is compiled in (`policy`), because anything the operator's
//! machine could set would let the operator mint plans out of test receipts.
//!
//! The app hands over the transaction as Apple signed it (a JWS with Apple's chain in
//! `x5c`). Nothing needs Apple's servers to check it: the chain ends at the pinned Apple Root
//! CA G3. Apple learns nothing it did not know; the enclave never learns the Apple ID.

use base64::Engine;
use serde_json::Value;
use x509_parser::prelude::*;

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
    crate::policy::APPLE_CREDIT_PREFIX.to_string()
}

/// The bundle id a transaction must name (`IAP_BUNDLE_ID`, default com.tokumai.app).
pub fn bundle_id() -> String {
    crate::policy::APPLE_BUNDLE_ID.to_string()
}

/// Prepaid tiles sold through the App Store (`policy::APPLE_CREDIT_TILES`). An App Store
/// buyer gets the same amounts as on every other rail, so the bucket size gives nobody away.
pub fn tiers() -> Vec<u32> {
    crate::policy::APPLE_CREDIT_TILES.to_vec()
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
    crate::policy::APPLE_PLAN_PREFIX.to_string()
}

/// The subscription product ids, in tier order: monthly then yearly.
pub fn plan_ids() -> Vec<String> {
    let p = plan_prefix();
    let monthly = tokumai_core::subscription::TIERS.iter().map(|(_, cents)| format!("{p}{}", cents / 100));
    let yearly: Vec<String> =
        tokumai_core::subscription::TIERS.iter().map(|(_, cents)| format!("{p}{}.year", cents / 100)).collect();
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
    let tier = tokumai_core::subscription::TIERS.iter().position(|(_, c)| *c == cents)?;
    Some((tier, yearly))
}

/// Does a verified transaction buy a SUBSCRIPTION here, and which one? The checks mirror
/// `credit_for`; what differs is the product type and that a lapsed period is refused —
/// Apple keeps handing the app the last transaction of a subscription that has ended, and
/// treating that as "still paid" would give away months.
/// Is this Apple's word that one of OUR plans was refunded? Apple marks a refunded (or
/// otherwise revoked) transaction with `revocationDate`; the plan it belongs to must end at
/// once (audit 2026-09-21, M1). Same checks as `plan_for` except the ones a refund makes
/// moot (expiry, and the revocation itself).
pub fn revoked_plan(tx: &AppleTx) -> bool {
    tx.revoked
        && tx.bundle_id == bundle_id()
        && tx.kind == "Auto-Renewable Subscription"
        && !tx.original_transaction_id.is_empty()
        && product_plan(&tx.product_id).is_some()
        && (tx.environment == "Production" || (tx.environment == "Sandbox" && allow_sandbox()))
}

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
// ---------------------------------------------------------------------------------------
// App Store Server API (audit 2026-09-21, M1). The app reports its subscription only when
// it is opened, and Apple stops reporting one it has refunded — so without asking Apple
// ourselves, a refunded plan ran on and a renewal waited for the next launch. Outbound
// only, like the Stripe check: no port, no webhook. Off until the operator adds an
// In-App Purchase key from App Store Connect (Users and Access → Integrations):
//   IAP_API_KEY_ID, IAP_API_ISSUER_ID, and IAP_API_KEY (the .p8 file's text) or
//   IAP_API_KEY_PATH (where it lies).
// ---------------------------------------------------------------------------------------

struct ApiKey {
    key_id: String,
    issuer: String,
    pkcs8: Vec<u8>,
}

/// The App Store Server API client, when its key is among the sealed secrets
/// (`IAP_API_KEY_ID`, `IAP_API_ISSUER_ID`, `IAP_API_KEY` — the .p8 file's text).
pub struct AppleApi {
    key: ApiKey,
}

impl AppleApi {
    pub fn from_secrets(s: &dyn crate::secrets::SecretSource) -> Option<AppleApi> {
        let key_id = s.get("IAP_API_KEY_ID")?;
        let issuer = s.get("IAP_API_ISSUER_ID")?;
        let pem = s.get("IAP_API_KEY")?;
        let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
        let pkcs8 = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
        Some(AppleApi { key: ApiKey { key_id, issuer, pkcs8 } })
    }
}

/// The bearer token Apple's API wants: an ES256 JWT for our issuer and bundle, 20 minutes.
fn api_token(k: &ApiKey, now_s: u64) -> Result<String, String> {
    use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    let enc = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let header = enc(&serde_json::json!({ "alg": "ES256", "kid": k.key_id, "typ": "JWT" }));
    let claims = enc(&serde_json::json!({
        "iss": k.issuer, "iat": now_s, "exp": now_s + 20 * 60,
        "aud": "appstoreconnect-v1", "bid": bundle_id(),
    }));
    let rng = ring::rand::SystemRandom::new();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &k.pkcs8, &rng)
        .map_err(|_| "the App Store API key is not a P-256 PKCS#8 key".to_string())?;
    let signing = format!("{header}.{claims}");
    let sig = pair.sign(&rng, signing.as_bytes()).map_err(|_| "could not sign the App Store API token".to_string())?;
    Ok(format!("{signing}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.as_ref())))
}

/// Apple's current word on one subscription: its status (1 active, 2 expired, 3 billing
/// retry, 4 grace period, 5 revoked) and the latest transaction, signed by Apple — which the
/// caller verifies with `verify_jws` like any other. `Ok(None)`: Apple does not know it.
pub async fn subscription_status(api: &AppleApi, original_transaction_id: &str) -> Result<Option<(u64, String)>, String> {
    let k = &api.key;
    if original_transaction_id.is_empty() || !original_transaction_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("not an App Store transaction number".into());
    }
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let token = api_token(k, now_s)?;
    // Production first; Apple's own advice is to try the sandbox when production does not
    // know the transaction — and only a server that accepts sandbox purchases does.
    let mut hosts = vec!["https://api.storekit.itunes.apple.com"];
    if allow_sandbox() {
        hosts.push("https://api.storekit-sandbox.itunes.apple.com");
    }
    for host in hosts {
        let res = crate::http::client()
            .get(format!("{host}/inApps/v1/subscriptions/{original_transaction_id}"))
            .bearer_auth(&token)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("App Store API unreachable: {e}"))?;
        let status = res.status();
        if status.as_u16() == 404 {
            continue;
        }
        if !status.is_success() {
            return Err(format!("App Store API answered {status}"));
        }
        let v: Value = res.json().await.map_err(|_| "App Store API sent something unreadable".to_string())?;
        return Ok(status_of(&v, original_transaction_id));
    }
    Ok(None)
}

/// The entry for `original_transaction_id` in a "Get All Subscription Statuses" answer.
fn status_of(v: &Value, original_transaction_id: &str) -> Option<(u64, String)> {
    v.get("data")?.as_array()?.iter().find_map(|group| {
        group.get("lastTransactions")?.as_array()?.iter().find_map(|t| {
            (t.get("originalTransactionId")?.as_str()? == original_transaction_id).then(|| {
                (
                    t.get("status").and_then(|s| s.as_u64()).unwrap_or(0),
                    t.get("signedTransactionInfo").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                )
            })
        })
    })
}

pub fn allow_sandbox() -> bool {
    crate::policy::APPLE_SANDBOX
}

/// The fingerprint the table is keyed by. Apple's transaction ids are sequential-looking
/// numbers that mean nothing without Apple; the hash keeps them out of a dump anyway.

/// Apple's marker on the certificate that signs App Store transactions (the leaf).
const OID_APPSTORE_RECEIPT_SIGNING: &str = "1.2.840.113635.100.6.11.1";
/// Apple's marker on the WWDR intermediate that issues it.
const OID_APPLE_WWDR_INTERMEDIATE: &str = "1.2.840.113635.100.6.2.1";

/// The `x5c` chain of an App Store JWS, checked the way Apple's own server library checks it:
/// exactly leaf → intermediate [→ root], where
///
/// * the root, if sent, is byte-for-byte the pinned one, and the intermediate is signed by
///   the pinned root — so there is no room for a longer chain under it;
/// * the intermediate is a CA and carries Apple's WWDR marker;
/// * the leaf is NOT a CA, carries the App Store receipt-signing marker, and is signed by the
///   intermediate;
/// * both are valid at `at`.
///
/// Before 2026-09-21 only the signatures, the dates and the pin were checked (audit H6):
/// any certificate chaining to Apple's root would do — an Apple Pay or other developer
/// certificate made from one's own key among them, even a leaf standing in as the
/// intermediate — and its holder could sign any plan for themselves.
fn verify_apple_chain(chain: &[Vec<u8>], root_der: &[u8], at: ASN1Time) -> Result<(), String> {
    if chain.len() != 2 && chain.len() != 3 {
        return Err("unexpected certificate chain length".into());
    }
    if chain.len() == 3 && chain[2] != root_der {
        return Err("certificate chain does not end at Apple's root".into());
    }
    fn parse(der: &[u8]) -> Result<X509Certificate<'_>, String> {
        X509Certificate::from_der(der).map(|(_, c)| c).map_err(|_| "malformed certificate".to_string())
    }
    let (leaf, inter, root) = (parse(&chain[0])?, parse(&chain[1])?, parse(root_der)?);
    for c in [&leaf, &inter] {
        if !c.validity().is_valid_at(at) {
            return Err("a certificate in the chain is not valid now".into());
        }
    }
    let is_ca = |c: &X509Certificate| c.basic_constraints().ok().flatten().map(|b| b.value.ca).unwrap_or(false);
    let marked = |c: &X509Certificate, oid: &str| c.extensions().iter().any(|e| e.oid.to_id_string() == oid);
    if !is_ca(&inter) || !marked(&inter, OID_APPLE_WWDR_INTERMEDIATE) {
        return Err("the intermediate certificate is not Apple's WWDR authority".into());
    }
    if is_ca(&leaf) || !marked(&leaf, OID_APPSTORE_RECEIPT_SIGNING) {
        return Err("the signing certificate is not an App Store receipt-signing certificate".into());
    }
    inter
        .verify_signature(Some(root.public_key()))
        .map_err(|_| "certificate chain does not end at Apple's root".to_string())?;
    leaf.verify_signature(Some(inter.public_key()))
        .map_err(|_| "certificate chain does not verify".to_string())?;
    Ok(())
}

fn b64url(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .map_err(|_| "malformed transaction (base64)".to_string())
}

/// Verify an App Store JWS and read the transaction it carries. `now_ms` is the clock the
/// certificate validity is checked against.
///
/// Checks, in order: three-part compact JWS; `alg` ES256; the `x5c` chain as Apple builds it
/// (`verify_apple_chain`); the JWS signature under the leaf key. Then the payload is parsed
/// — the caller decides what to do with it.
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
        .filter(|x| x.len() == 2 || x.len() == 3)
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
    let at = ASN1Time::from_timestamp((now_ms / 1000) as i64).map_err(|_| "clock".to_string())?;
    verify_apple_chain(&chain, APPLE_ROOT_CA_G3, at)?;
    let (_, leaf) = X509Certificate::from_der(&chain[0]).map_err(|_| "malformed certificate".to_string())?;
    // The leaf signs the JWS: ES256 = ECDSA P-256 over SHA-256, signature as r||s (64 bytes).
    let leaf_key = leaf.public_key().subject_public_key.data.as_ref();
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
    Ok(usd as u64 * tokumai_core::billing::TOKU_PER_USD * q)
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
        assert_eq!(credit_for(&tx("com.tokumai.app.credit.10")), Ok(10 * tokumai_core::billing::TOKU_PER_USD));
        let mut t = tx("com.tokumai.app.credit.10");
        t.bundle_id = "com.example.other".into();
        assert!(credit_for(&t).is_err());
        let mut t = tx("com.tokumai.app.credit.10");
        t.revoked = true;
        assert!(credit_for(&t).unwrap_err().contains("refunded"));
        let mut t = tx("com.tokumai.app.credit.10");
        t.environment = "Sandbox".into();
        // Sandbox purchases count only where the build says so (debug builds, or the
        // `apple-sandbox` feature) — never because of anything the operator can set.
        if crate::policy::APPLE_SANDBOX {
            assert!(credit_for(&t).is_ok());
        } else {
            assert!(credit_for(&t).unwrap_err().contains("sandbox"));
        }
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
        assert_eq!(ids.len(), tokumai_core::subscription::TIERS.len() * 2);
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
    fn the_pinned_root_is_apple_root_ca_g3() {
        let (_, root) = X509Certificate::from_der(APPLE_ROOT_CA_G3).unwrap();
        assert!(root.subject().to_string().contains("Apple Root CA - G3"));
        assert!(root.verify_signature(None).is_ok(), "self-signed");
    }

    #[test]
    fn apples_status_answer_is_read_for_the_one_subscription_asked_about() {
        let v = serde_json::json!({ "data": [{ "subscriptionGroupIdentifier": "g", "lastTransactions": [
            { "originalTransactionId": "111", "status": 1, "signedTransactionInfo": "a.b.c" },
            { "originalTransactionId": "222", "status": 5, "signedTransactionInfo": "d.e.f" } ] }] });
        assert_eq!(status_of(&v, "222"), Some((5, "d.e.f".to_string())));
        assert_eq!(status_of(&v, "333"), None);
        assert_eq!(status_of(&serde_json::json!({}), "111"), None);
    }

    #[test]
    fn the_api_token_is_an_es256_jwt_for_our_bundle() {
        // A throw-away P-256 key, made here: only the token's shape is under test.
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let k = ApiKey { key_id: "KID123".into(), issuer: "iss-uuid".into(), pkcs8: pkcs8.as_ref().to_vec() };
        let t = api_token(&k, 1_789_000_000).unwrap();
        let parts: Vec<&str> = t.split('.').collect();
        assert_eq!(parts.len(), 3);
        let dec = |p: &str| -> Value { serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap() };
        assert_eq!(dec(parts[0])["alg"], "ES256");
        assert_eq!(dec(parts[0])["kid"], "KID123");
        let c = dec(parts[1]);
        assert_eq!(c["aud"], "appstoreconnect-v1");
        assert_eq!(c["bid"], bundle_id());
        assert_eq!(c["exp"].as_u64().unwrap() - c["iat"].as_u64().unwrap(), 1200);
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(), 64, "r||s");
    }

    /// Test chains under a throw-away root (keys never kept; see iap_testdata), each
    /// breaking one rule. The real WWDR G6 intermediate is the positive control against the
    /// pinned root.
    mod chain {
        use super::super::*;
        const ROOT: &[u8] = include_bytes!("iap_testdata/test_root.der");
        const INTER_OK: &[u8] = include_bytes!("iap_testdata/test_inter_ok.der");
        const INTER_NOCA: &[u8] = include_bytes!("iap_testdata/test_inter_noca.der");
        const INTER_NOOID: &[u8] = include_bytes!("iap_testdata/test_inter_nooid.der");
        const LEAF_OK: &[u8] = include_bytes!("iap_testdata/test_leaf_ok.der");
        const LEAF_NOOID: &[u8] = include_bytes!("iap_testdata/test_leaf_nooid.der");
        const LEAF_UNDER_LEAF: &[u8] = include_bytes!("iap_testdata/test_leaf2.der");
        const LEAF_UNDER_NOCA: &[u8] = include_bytes!("iap_testdata/test_leafc.der");
        const LEAF_UNDER_NOOID: &[u8] = include_bytes!("iap_testdata/test_leafn.der");
        const APPLE_G6: &[u8] = include_bytes!("iap_testdata/apple_wwdr_g6.der");

        fn at(y: i64) -> ASN1Time {
            ASN1Time::from_timestamp((y - 1970) * 31_556_952).unwrap()
        }
        fn check(chain: &[&[u8]], root: &[u8]) -> Result<(), String> {
            let v: Vec<Vec<u8>> = chain.iter().map(|c| c.to_vec()).collect();
            verify_apple_chain(&v, root, at(2030))
        }

        #[test]
        fn a_chain_built_like_apples_passes() {
            assert_eq!(check(&[LEAF_OK, INTER_OK], ROOT), Ok(()));
            assert_eq!(check(&[LEAF_OK, INTER_OK, ROOT], ROOT), Ok(()));
        }

        #[test]
        fn a_leaf_without_the_receipt_signing_marker_is_refused() {
            assert!(check(&[LEAF_NOOID, INTER_OK], ROOT).unwrap_err().contains("receipt-signing"));
        }

        #[test]
        fn a_leaf_standing_in_as_the_intermediate_is_refused() {
            // The attack the old loop allowed: a non-CA certificate signing the "leaf".
            assert!(check(&[LEAF_UNDER_LEAF, LEAF_OK], ROOT).unwrap_err().contains("WWDR"));
        }

        #[test]
        fn an_intermediate_that_is_no_ca_or_not_apples_is_refused() {
            assert!(check(&[LEAF_UNDER_NOCA, INTER_NOCA], ROOT).unwrap_err().contains("WWDR"));
            assert!(check(&[LEAF_UNDER_NOOID, INTER_NOOID], ROOT).unwrap_err().contains("WWDR"));
        }

        #[test]
        fn the_chain_must_end_at_the_pinned_root_and_nowhere_else() {
            // Under Apple's pinned root, a chain signed by our test root fails.
            assert!(check(&[LEAF_OK, INTER_OK], APPLE_ROOT_CA_G3).is_err());
            // A third certificate that is not the pin, and a fourth of any kind, are refused.
            assert!(check(&[LEAF_OK, INTER_OK, INTER_OK], ROOT).is_err());
            assert!(check(&[LEAF_OK, INTER_OK, ROOT, ROOT], ROOT).is_err());
            assert!(check(&[LEAF_OK], ROOT).is_err());
        }

        #[test]
        fn a_chain_outside_its_dates_is_refused() {
            let v = vec![LEAF_OK.to_vec(), INTER_OK.to_vec()];
            assert!(verify_apple_chain(&v, ROOT, at(2060)).is_err());
        }

        #[test]
        fn apples_real_wwdr_g6_meets_the_intermediate_rules() {
            // The positive control: the certificate Apple actually uses carries the marker,
            // is a CA, and is signed by the pinned root. If Apple's intermediate ever failed
            // these rules, every purchase would be refused — this is where it would show.
            let (_, g6) = X509Certificate::from_der(APPLE_G6).unwrap();
            let (_, root) = X509Certificate::from_der(APPLE_ROOT_CA_G3).unwrap();
            assert!(g6.basic_constraints().unwrap().unwrap().value.ca);
            assert!(g6.extensions().iter().any(|e| e.oid.to_id_string() == OID_APPLE_WWDR_INTERMEDIATE));
            assert!(g6.verify_signature(Some(root.public_key())).is_ok());
            assert!(g6.validity().is_valid_at(at(2030)));
        }
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
        // A chain of Apple's root alone, twice: valid certificates signed by the pin — and
        // refused all the same, because neither is the WWDR intermediate nor a receipt-
        // signing leaf. Before audit H6 this got as far as the JWS signature.
        let root_b64 = base64::engine::general_purpose::STANDARD.encode(APPLE_ROOT_CA_G3);
        let hdr = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"alg":"ES256","x5c":["{root_b64}","{root_b64}"]}}"#));
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 64]);
        let err = verify_jws(&format!("{hdr}.e30.{sig}"), 1_757_500_000_000).unwrap_err();
        assert!(err.contains("WWDR"), "{err}");
    }
}
