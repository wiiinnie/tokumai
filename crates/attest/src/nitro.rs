//! AWS Nitro Enclaves. The enclave asks the Nitro hypervisor for an attestation document:
//! a COSE_Sign1 (CBOR) signed with ECDSA P-384 by a short-lived certificate that chains to
//! the AWS Nitro root. PCR0 is the hash of the enclave image; `user_data` carries our
//! binding. The app checks all of it offline, against the root pinned below.
//!
//! What is checked, in order (AWS: "Verifying the root of trust"):
//! 1. COSE_Sign1, tagged (18) or not; protected header exactly `{1: -35}` (ES384).
//! 2. The document: `module_id`, `digest` = SHA384, `timestamp`, `pcrs`, `certificate`,
//!    `cabundle`.
//! 3. The chain `cabundle[0]` (must be the pinned root, byte for byte) → … → `certificate`:
//!    every link signed by the one before, every CA marked as one, every certificate valid
//!    at the document's timestamp.
//! 4. The COSE signature under the certificate's P-384 key.
//! 5. PCR0 is a real measurement (a debug enclave reports all zeros and is refused), and
//!    `user_data` is exactly 32 bytes — our binding.
//!
//! Freshness is not the timestamp's job: the binding covers the app's nonce, so an old
//! document cannot answer a new question.

use crate::Evidence;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ciborium::Value;
use x509_parser::prelude::*;

/// AWS_NitroEnclaves_Root-G1, DER. SHA-256
/// 641a0321a3e244efe456463195d606317ed7cdcc3c1756e09893f3c68f79bb5b — the fingerprint AWS
/// publishes (docs.aws.amazon.com/enclaves/latest/user/verify-root.html); checked in the tests.
pub const AWS_NITRO_ROOT_G1: &[u8] = include_bytes!("aws_nitro_root_g1.der");

/// What a verified document says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub module_id: String,
    pub timestamp_ms: u64,
    /// PCR0, hex: the enclave image.
    pub pcr0: String,
    pub user_data: Option<Vec<u8>>,
    pub nonce: Option<Vec<u8>>,
    /// A debug enclave: PCRs all zeros. Genuinely signed by AWS, and proves nothing about
    /// the code — `verify` refuses it.
    pub debug: bool,
}

pub(crate) fn verify(evidence: &Evidence) -> Result<(String, [u8; 32]), String> {
    let raw = B64.decode(evidence.document.trim()).map_err(|_| "the Nitro document is not base64")?;
    let doc = verify_document(&raw, AWS_NITRO_ROOT_G1)?;
    if doc.debug {
        return Err("the enclave runs in debug mode, which proves nothing about its code".into());
    }
    let user_data: [u8; 32] = doc
        .user_data
        .as_deref()
        .and_then(|u| u.try_into().ok())
        .ok_or("the Nitro document carries no binding")?;
    Ok((doc.pcr0, user_data))
}

fn map_get<'a>(m: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    m.iter().find(|(k, _)| k.as_text() == Some(key)).map(|(_, v)| v)
}

fn bytes<'a>(v: Option<&'a Value>, what: &str) -> Result<&'a [u8], String> {
    v.and_then(|v| v.as_bytes()).map(|b| b.as_slice()).ok_or_else(|| format!("the Nitro document has no {what}"))
}

/// Parse and check a raw attestation document against `root` (the pinned AWS root in the
/// app; a throw-away root in tests).
pub fn verify_document(raw: &[u8], root: &[u8]) -> Result<Document, String> {
    let cose: Value = ciborium::de::from_reader(raw).map_err(|_| "the Nitro document is not CBOR")?;
    let cose = match cose {
        Value::Tag(18, inner) => *inner,
        v => v,
    };
    let parts = cose.as_array().filter(|a| a.len() == 4).ok_or("the Nitro document is not a COSE_Sign1")?;
    let protected = parts[0].as_bytes().ok_or("malformed COSE header")?;
    let payload = parts[2].as_bytes().ok_or("malformed COSE payload")?;
    let signature = parts[3].as_bytes().ok_or("malformed COSE signature")?;

    // Exactly {1: -35}: ES384, nothing else a verifier might be talked into honouring.
    let header: Value = ciborium::de::from_reader(protected.as_slice()).map_err(|_| "malformed COSE header")?;
    let alg_ok = header.as_map().is_some_and(|m| m.len() == 1 && m[0].0.as_integer() == Some(1.into()) && m[0].1.as_integer() == Some((-35).into()));
    if !alg_ok {
        return Err("the Nitro document is not signed with ES384".into());
    }

    let body: Value = ciborium::de::from_reader(payload.as_slice()).map_err(|_| "malformed attestation document")?;
    let m = body.as_map().ok_or("malformed attestation document")?;
    let module_id = map_get(m, "module_id").and_then(|v| v.as_text()).filter(|t| !t.is_empty()).ok_or("the Nitro document has no module id")?;
    if map_get(m, "digest").and_then(|v| v.as_text()) != Some("SHA384") {
        return Err("the Nitro document uses an unexpected digest".into());
    }
    let timestamp_ms: u64 = map_get(m, "timestamp")
        .and_then(|v| v.as_integer())
        .and_then(|i| u64::try_from(i).ok())
        .filter(|t| *t > 0)
        .ok_or("the Nitro document has no timestamp")?;
    let pcrs = map_get(m, "pcrs").and_then(|v| v.as_map()).ok_or("the Nitro document has no PCRs")?;
    let pcr0 = pcrs
        .iter()
        .find(|(k, _)| k.as_integer() == Some(0.into()))
        .and_then(|(_, v)| v.as_bytes())
        .filter(|b| b.len() == 48)
        .ok_or("the Nitro document has no PCR0")?;
    let leaf_der = bytes(map_get(m, "certificate"), "certificate")?;
    let bundle: Vec<&[u8]> = map_get(m, "cabundle")
        .and_then(|v| v.as_array())
        .ok_or("the Nitro document has no CA bundle")?
        .iter()
        .map(|c| c.as_bytes().map(|b| b.as_slice()).ok_or("malformed CA bundle"))
        .collect::<Result<_, _>>()?;

    verify_chain(&bundle, leaf_der, root, timestamp_ms)?;

    // The COSE signature: ES384 over Sig_structure = ["Signature1", protected, h'', payload].
    let (_, leaf) = X509Certificate::from_der(leaf_der).map_err(|_| "malformed signing certificate")?;
    let sig_structure = Value::Array(vec![
        Value::Text("Signature1".into()),
        Value::Bytes(protected.clone()),
        Value::Bytes(Vec::new()),
        Value::Bytes(payload.clone()),
    ]);
    let mut to_sign = Vec::new();
    ciborium::ser::into_writer(&sig_structure, &mut to_sign).map_err(|e| e.to_string())?;
    let key = leaf.public_key().subject_public_key.data.as_ref();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P384_SHA384_FIXED, key)
        .verify(&to_sign, signature)
        .map_err(|_| "the Nitro document's signature does not verify")?;

    let opt = |k: &str| map_get(m, k).and_then(|v| v.as_bytes()).cloned();
    Ok(Document {
        module_id: module_id.to_string(),
        timestamp_ms,
        pcr0: hex::encode(pcr0),
        user_data: opt("user_data"),
        nonce: opt("nonce"),
        debug: pcr0.iter().all(|b| *b == 0),
    })
}

fn parse(der: &[u8]) -> Result<X509Certificate<'_>, String> {
    let (rest, c) = X509Certificate::from_der(der).map_err(|_| "malformed certificate in the chain".to_string())?;
    if !rest.is_empty() {
        return Err("trailing bytes after a certificate".into());
    }
    Ok(c)
}

/// `bundle` is [root, intermediate 1, …, intermediate N] as AWS orders it; the leaf is
/// signed by the last. The first must be `root` itself.
fn verify_chain(bundle: &[&[u8]], leaf_der: &[u8], root: &[u8], at_ms: u64) -> Result<(), String> {
    if bundle.first().copied() != Some(root) {
        return Err("the Nitro document's chain does not start at the AWS Nitro root".into());
    }
    if bundle.len() > 8 {
        return Err("the Nitro document's chain is implausibly long".into());
    }
    let at = ASN1Time::from_timestamp((at_ms / 1000) as i64).map_err(|_| "bad timestamp")?;
    let certs: Vec<X509Certificate> = bundle.iter().map(|d| parse(d)).collect::<Result<_, _>>()?;
    let leaf = parse(leaf_der)?;
    let is_ca = |c: &X509Certificate| c.basic_constraints().ok().flatten().map(|b| b.value.ca).unwrap_or(false);
    for c in certs.iter().chain(std::iter::once(&leaf)) {
        if !c.validity().is_valid_at(at) {
            return Err("a certificate in the Nitro chain was not valid when the document was made".into());
        }
    }
    for (i, c) in certs.iter().enumerate() {
        if !is_ca(c) {
            return Err("a certificate in the Nitro CA bundle is not a CA".into());
        }
        let issuer = if i == 0 { c } else { &certs[i - 1] };
        c.verify_signature(Some(issuer.public_key())).map_err(|_| "the Nitro chain does not verify".to_string())?;
    }
    if is_ca(&leaf) {
        return Err("the Nitro signing certificate is a CA".into());
    }
    let last = certs.last().ok_or("empty CA bundle")?;
    leaf.verify_signature(Some(last.public_key())).map_err(|_| "the Nitro signing certificate does not chain to the bundle".to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A genuine attestation document from a Nitro enclave, published as test data by
    /// Evervault (github.com/evervault/attestation-doc-validation, test-data/, Apache-2.0).
    const REAL: &[u8] = include_bytes!("nitro_testdata/evervault-valid-attestation-doc.cbor");

    #[test]
    fn the_pinned_root_is_the_one_aws_publishes() {
        use sha2::{Digest, Sha256};
        assert_eq!(hex::encode(Sha256::digest(AWS_NITRO_ROOT_G1)), "641a0321a3e244efe456463195d606317ed7cdcc3c1756e09893f3c68f79bb5b");
    }

    #[test]
    fn a_real_document_verifies_against_the_aws_root() {
        // Genuinely signed by the AWS chain — and a debug enclave, as published samples are.
        let d = verify_document(REAL, AWS_NITRO_ROOT_G1).unwrap();
        assert_eq!(d.pcr0.len(), 96);
        assert!(d.debug);
        assert!(d.module_id.starts_with("i-"), "{}", d.module_id);
        println!("module {} at {} · PCR0 {}", d.module_id, d.timestamp_ms, d.pcr0);
    }

    #[test]
    fn any_change_to_a_real_document_is_refused() {
        // Flip one bit at a time across the whole document: nothing may still verify — it
        // either stops parsing, or a signature or the chain breaks.
        for i in (0..REAL.len()).step_by(7) {
            let mut bad = REAL.to_vec();
            bad[i] ^= 0x01;
            assert!(verify_document(&bad, AWS_NITRO_ROOT_G1).is_err(), "byte {i} flipped and it still verified");
        }
    }

    #[test]
    fn a_debug_enclave_is_refused_by_the_app() {
        let e = Evidence { platform: crate::Platform::AwsNitro, document: B64.encode(REAL) };
        assert!(verify(&e).unwrap_err().contains("debug mode"));
    }

    #[test]
    fn another_root_is_not_accepted() {
        // The same document against a root it does not start at.
        let other = include_bytes!("nitro_testdata/not-the-aws-root.der");
        assert!(verify_document(REAL, other).unwrap_err().contains("AWS Nitro root"));
    }
}

/// Inside the enclave: ask the Nitro Secure Module for a document over `user_data`.
#[cfg(all(target_os = "linux", feature = "nsm"))]
pub struct NitroAttester {
    fd: i32,
}

#[cfg(all(target_os = "linux", feature = "nsm"))]
impl NitroAttester {
    /// Opens /dev/nsm — present only inside a Nitro enclave.
    pub fn open() -> Result<NitroAttester, String> {
        let fd = aws_nitro_enclaves_nsm_api::driver::nsm_init();
        if fd < 0 {
            return Err("no Nitro Secure Module here (/dev/nsm) — not inside a Nitro enclave".into());
        }
        Ok(NitroAttester { fd })
    }
}

#[cfg(all(target_os = "linux", feature = "nsm"))]
impl Drop for NitroAttester {
    fn drop(&mut self) {
        aws_nitro_enclaves_nsm_api::driver::nsm_exit(self.fd);
    }
}

#[cfg(all(target_os = "linux", feature = "nsm"))]
impl NitroAttester {
    /// A document over `public_key` — for AWS KMS, which encrypts its answer to that key
    /// and refuses unless the document shows an image its key policy allows. Not the
    /// app's binding: KMS wants the key in the document's own `public_key` field.
    /// What the module says this image measures — the same PCR0 a key policy names. Only
    /// for saying so out loud when something is refused; nothing trusts it.
    pub fn pcr0(&self) -> Result<String, String> {
        use aws_nitro_enclaves_nsm_api::api::{Request, Response};
        match aws_nitro_enclaves_nsm_api::driver::nsm_process_request(self.fd, Request::DescribePCR { index: 0 }) {
            Response::DescribePCR { data, .. } => Ok(hex::encode(data)),
            other => Err(format!("the Nitro Secure Module would not say: {other:?}")),
        }
    }

    pub fn attest_for_kms(&self, public_key: &[u8]) -> Result<Vec<u8>, String> {
        use aws_nitro_enclaves_nsm_api::api::{Request, Response};
        let request = Request::Attestation { user_data: None, nonce: None, public_key: Some(serde_bytes::ByteBuf::from(public_key.to_vec())) };
        match aws_nitro_enclaves_nsm_api::driver::nsm_process_request(self.fd, request) {
            Response::Attestation { document } => Ok(document),
            Response::Error(e) => Err(format!("the Nitro Secure Module refused: {e:?}")),
            _ => Err("unexpected answer from the Nitro Secure Module".into()),
        }
    }
}

#[cfg(all(target_os = "linux", feature = "nsm"))]
impl crate::Attester for NitroAttester {
    fn platform(&self) -> crate::Platform {
        crate::Platform::AwsNitro
    }
    fn attest(&self, user_data: &[u8; 32]) -> Result<Evidence, String> {
        use aws_nitro_enclaves_nsm_api::api::{Request, Response};
        // The binding already covers the app's nonce and our keys; nothing else is needed.
        let request = Request::Attestation { user_data: Some(serde_bytes::ByteBuf::from(user_data.to_vec())), nonce: None, public_key: None };
        match aws_nitro_enclaves_nsm_api::driver::nsm_process_request(self.fd, request) {
            Response::Attestation { document } => Ok(Evidence { platform: crate::Platform::AwsNitro, document: B64.encode(document) }),
            Response::Error(e) => Err(format!("the Nitro Secure Module refused: {e:?}")),
            _ => Err("unexpected answer from the Nitro Secure Module".into()),
        }
    }
}
