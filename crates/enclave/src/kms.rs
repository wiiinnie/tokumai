//! Getting a secret out of AWS KMS, into the enclave and nowhere else.
//!
//! The key's policy allows `kms:Decrypt` and `kms:GenerateDataKey` only for a request that
//! carries an attestation document of the published image (`kms:RecipientAttestation:PCR0`).
//! AWS checks that document itself, and answers not with the plaintext but with a copy
//! encrypted to a public key that appears IN that document — a key whose private half
//! exists only inside this enclave, for this request. So the host it runs on relays
//! ciphertext both ways.
//!
//! Two things come this way. The operator's secrets (provider keys, Stripe), which the
//! operator sealed and therefore knows. And the enclave's own data key, which the operator
//! must NOT know: it is born here, by `GenerateDataKey` — KMS makes it, hands this enclave
//! the plaintext under the request key and the host a KMS-wrapped copy to keep. No one
//! outside an attested enclave ever holds it in the clear (until 2026-10-05 it was typed
//! into the sealed file by the operator, which made the KMS gate a formality against the
//! one party it exists for).
//!
//! The request is one signed POST (SigV4, signed here rather than by the AWS SDK: the SDK
//! brings its own HTTP stack, and everything the enclave sends has to go through the
//! egress proxy). The answer is CMS: the content key encrypted to our RSA key
//! (RSAES-OAEP-SHA256), the content under AES-256-CBC.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The request key's type, for the binary that holds one across its KMS calls.
pub use rsa::RsaPrivateKey;

/// Temporary credentials of the instance role, as the host hands them over.
#[derive(Clone, serde::Deserialize)]
pub struct Credentials {
    #[serde(rename = "AccessKeyId")]
    pub access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    pub secret_access_key: String,
    #[serde(rename = "Token", alias = "SessionToken")]
    pub session_token: String,
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256>>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn hex_sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `YYYYMMDDTHHMMSSZ` and `YYYYMMDD` from milliseconds since the epoch — the two forms
/// SigV4 wants. (No date library: this is the only place the enclave formats a time.)
fn amz_date(now_ms: u64) -> (String, String) {
    let secs = (now_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = tokumai_core::subscription::civil_from_ms((days as u64) * 86_400_000);
    let date = format!("{y:04}{m:02}{d:02}");
    (format!("{date}T{:02}{:02}{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60), date)
}

/// One signed POST to KMS. `target` is the API action (`TrentService.Decrypt`).
async fn call(creds: &Credentials, region: &str, target: &str, body: &Value, now_ms: u64) -> Result<Value, String> {
    let host = format!("kms.{region}.amazonaws.com");
    let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let (stamp, date) = amz_date(now_ms);
    let payload_hash = hex_sha256(&body);

    // SigV4: canonical request → string to sign → signature.
    let canonical = format!(
        "POST\n/\n\ncontent-type:application/x-amz-json-1.1\nhost:{host}\nx-amz-date:{stamp}\nx-amz-security-token:{}\nx-amz-target:{target}\n\ncontent-type;host;x-amz-date;x-amz-security-token;x-amz-target\n{payload_hash}",
        creds.session_token
    );
    let scope = format!("{date}/{region}/kms/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", hex_sha256(canonical.as_bytes()));
    let key = hmac(&hmac(&hmac(&hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes()), region.as_bytes()), b"kms"), b"aws4_request");
    let signature = hex::encode(hmac(&key, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target, Signature={signature}",
        creds.access_key_id
    );

    let res = crate::http::client()
        .post(format!("https://{host}/"))
        .header("content-type", "application/x-amz-json-1.1")
        .header("x-amz-date", &stamp)
        .header("x-amz-security-token", &creds.session_token)
        .header("x-amz-target", target)
        .header("authorization", authorization)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("KMS is unreachable: {e}"))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let said: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let msg = said["message"].as_str().or(said["Message"].as_str()).unwrap_or(text.trim());
        return Err(format!("KMS refused ({status}): {msg}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("KMS answered unreadably: {e}"))
}

/// The encryption context the data key is wrapped under: a KMS blob made for one purpose
/// does not open as another (a sealed-secrets key handed over as "the data key", say).
pub const DATA_KEY_CONTEXT: (&str, &str) = ("tokumai", "data-key");

/// Decrypt `ciphertext` (a KMS blob) so that only this enclave can read the result.
///
/// `attestation` is a document over `recipient_public_key` (the NSM's `public_key` field),
/// `private` its private half, which never leaves here. KMS checks the document against
/// the key's policy and encrypts its answer to that key.
pub async fn decrypt_to_enclave(
    creds: &Credentials,
    region: &str,
    ciphertext: &[u8],
    attestation: &[u8],
    private: &rsa::RsaPrivateKey,
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    decrypt_to_enclave_in(creds, region, ciphertext, None, attestation, private, now_ms).await
}

/// The same, for a blob that was made under an encryption context (`DATA_KEY_CONTEXT`):
/// KMS refuses to open it under any other.
pub async fn decrypt_to_enclave_in(
    creds: &Credentials,
    region: &str,
    ciphertext: &[u8],
    context: Option<(&str, &str)>,
    attestation: &[u8],
    private: &rsa::RsaPrivateKey,
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    let mut body = json!({
        "CiphertextBlob": B64.encode(ciphertext),
        "Recipient": { "AttestationDocument": B64.encode(attestation), "KeyEncryptionAlgorithm": "RSAES_OAEP_SHA_256" },
    });
    if let Some((k, v)) = context {
        body["EncryptionContext"] = json!({ k: v });
    }
    let answer = call(creds, region, "TrentService.Decrypt", &body, now_ms).await?;
    for_this_enclave(&answer, private)
}

/// A fresh 256-bit data key, made by KMS under `key_id` (`alias/…` or an ARN, part of the
/// image), that the enclave alone sees in the clear. Returns the KMS-wrapped copy for the
/// host to keep — it opens only by `decrypt_to_enclave_in` with `DATA_KEY_CONTEXT`, and
/// only for an attested image on the key's allow list — and the key itself.
pub async fn generate_data_key_to_enclave(
    creds: &Credentials,
    region: &str,
    key_id: &str,
    attestation: &[u8],
    private: &rsa::RsaPrivateKey,
    now_ms: u64,
) -> Result<(Vec<u8>, [u8; 32]), String> {
    let body = json!({
        "KeyId": key_id,
        "KeySpec": "AES_256",
        "EncryptionContext": { DATA_KEY_CONTEXT.0: DATA_KEY_CONTEXT.1 },
        "Recipient": { "AttestationDocument": B64.encode(attestation), "KeyEncryptionAlgorithm": "RSAES_OAEP_SHA_256" },
    });
    let answer = call(creds, region, "TrentService.GenerateDataKey", &body, now_ms).await?;
    // With a Recipient, KMS leaves `Plaintext` out of its answer. Were it present, the key
    // would have crossed the host's proxy in the clear (TLS ends in here, but the point of
    // the recipient copy is that KMS itself never says the key to anyone but this enclave).
    if answer.get("Plaintext").is_some_and(|p| !p.is_null()) {
        return Err("KMS answered with the data key in the clear — refused".into());
    }
    let wrapped = answer["CiphertextBlob"].as_str().ok_or("KMS answered without a wrapped copy of the data key")?;
    let wrapped = B64.decode(wrapped).map_err(|e| format!("KMS answer is not base64: {e}"))?;
    let key = for_this_enclave(&answer, private)?;
    let key: [u8; 32] = key.try_into().map_err(|_| "KMS made a data key that is not 32 bytes".to_string())?;
    Ok((wrapped, key))
}

/// The part of a KMS answer that only this enclave can read.
fn for_this_enclave(answer: &Value, private: &rsa::RsaPrivateKey) -> Result<Vec<u8>, String> {
    let sealed = answer["CiphertextForRecipient"]
        .as_str()
        .ok_or("KMS answered without a copy for this enclave — the key policy may allow plaintext instead")?;
    let sealed = B64.decode(sealed).map_err(|e| format!("KMS answer is not base64: {e}"))?;
    open_cms(&sealed, private)
}

/// The CMS envelope KMS sends back: the content key encrypted to our RSA key, the content
/// under AES-256-CBC.
///
/// Read by hand, because KMS answers in BER with lengths left open (a stream it did not
/// size up front), and a DER reader refuses those.
fn open_cms(ber: &[u8], private: &rsa::RsaPrivateKey) -> Result<Vec<u8>, String> {
    let (enc_key, iv, content) = cms_parts(ber)?;
    let content_key = private
        .decrypt(rsa::Oaep::new::<Sha256>(), &enc_key)
        .map_err(|_| "the content key is not for this enclave's key".to_string())?;
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    <cbc::Decryptor<aes::Aes256>>::new_from_slices(&content_key, &iv)
        .map_err(|_| "unexpected key or IV length".to_string())?
        .decrypt_padded_vec_mut::<Pkcs7>(&content)
        .map_err(|_| "the CMS content does not open".to_string())
}

/// One BER value: its tag, its contents, and where the next one starts. Lengths may be
/// definite or left open (`80`, ended by two zero bytes) — KMS uses both.
fn ber(bytes: &[u8], at: usize) -> Result<(u8, Vec<u8>, usize), String> {
    let tag = *bytes.get(at).ok_or("the CMS message ends early")?;
    let first = *bytes.get(at + 1).ok_or("the CMS message ends early")?;
    let (len, body) = if first < 0x80 {
        (first as usize, at + 2)
    } else if first == 0x80 {
        // Open-ended: the contents run until two zero bytes, at this level.
        let mut end = at + 2;
        loop {
            if bytes.get(end..end + 2) == Some(&[0, 0][..]) {
                return Ok((tag, bytes[at + 2..end].to_vec(), end + 2));
            }
            let (_, _, next) = ber(bytes, end)?;
            if next <= end {
                return Err("the CMS message does not advance".into());
            }
            end = next;
        }
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return Err("an unreasonable length in the CMS message".into());
        }
        let slice = bytes.get(at + 2..at + 2 + n).ok_or("the CMS message ends early")?;
        (slice.iter().fold(0usize, |a, b| (a << 8) | *b as usize), at + 2 + n)
    };
    let end = body.checked_add(len).ok_or("an unreasonable length in the CMS message")?;
    Ok((tag, bytes.get(body..end).ok_or("the CMS message ends early")?.to_vec(), end))
}

/// Every value inside one constructed value.
fn children(body: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        if body[at] == 0 {
            break; // the end-of-contents marker of an open-ended value
        }
        let (tag, content, next) = ber(body, at)?;
        out.push((tag, content));
        at = next;
    }
    Ok(out)
}

/// The three pieces we need out of the envelope: the content key as it was encrypted to
/// our key, the IV, and the encrypted content.
fn cms_parts(ber_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let (_, content_info, _) = ber(ber_bytes, 0)?;
    let explicit = children(&content_info)?
        .into_iter()
        .find(|(tag, _)| *tag == 0xa0)
        .ok_or("no enveloped data in the CMS message")?
        .1;
    let enveloped = children(&explicit)?.into_iter().next().ok_or("empty enveloped data")?.1;
    let parts = children(&enveloped)?;
    let recipients = parts.iter().find(|(tag, _)| *tag == 0x31).ok_or("the CMS message names no recipient")?.1.clone();
    let recipient = children(&recipients)?.into_iter().next().ok_or("the CMS message names no recipient")?.1;
    // KeyTransRecipientInfo: version, who, algorithm, and last the encrypted key.
    let enc_key = children(&recipient)?
        .into_iter()
        .filter(|(tag, _)| *tag == 0x04)
        .next_back()
        .ok_or("the recipient carries no key")?
        .1;

    // EncryptedContentInfo: content type, the algorithm with its IV, the content.
    let info = parts
        .iter()
        .filter(|(tag, _)| *tag == 0x30)
        .next_back()
        .ok_or("the CMS message carries no content")?
        .1
        .clone();
    let info = children(&info)?;
    let algorithm = info.iter().find(|(tag, _)| *tag == 0x30).ok_or("no content algorithm")?.1.clone();
    let iv = children(&algorithm)?
        .into_iter()
        .find(|(tag, _)| *tag == 0x04)
        .ok_or("no IV in the CMS message")?
        .1;
    let content = info.iter().find(|(tag, _)| *tag & 0xdf == 0x80).ok_or("no content in the CMS message")?;
    // Open-ended content comes in pieces, each its own octet string.
    let content = if content.0 == 0xa0 { children(&content.1)?.into_iter().flat_map(|(_, c)| c).collect() } else { content.1.clone() };
    Ok((enc_key, iv, content))
}

/// A fresh RSA key for one request: its public half goes into the attestation document, its
/// private half stays here and dies with the request.
pub fn request_key() -> Result<(rsa::RsaPrivateKey, Vec<u8>), String> {
    use rsa::pkcs8::EncodePublicKey;
    let private = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 2048).map_err(|e| format!("could not make a request key: {e}"))?;
    let public = private.to_public_key().to_public_key_der().map_err(|e| e.to_string())?.as_bytes().to_vec();
    Ok((private, public))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_amazon_date_is_the_one_sigv4_expects() {
        // 2026-09-22T17:25:18Z
        assert_eq!(amz_date(1_790_097_918_000), ("20260922T172518Z".to_string(), "20260922".to_string()));
        assert_eq!(amz_date(0), ("19700101T000000Z".to_string(), "19700101".to_string()));
    }

    /// A value with its tag and length, definite.
    fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if body.len() < 0x80 {
            out.push(body.len() as u8);
        } else {
            let n = body.len();
            out.push(0x82);
            out.extend_from_slice(&[(n >> 8) as u8, n as u8]);
        }
        out.extend_from_slice(body);
        out
    }

    /// A value whose length is left open, ended by two zero bytes — how KMS writes them.
    fn open(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag, 0x80];
        out.extend_from_slice(body);
        out.extend_from_slice(&[0, 0]);
        out
    }

    /// An envelope shaped like the one KMS returns; `wrap` decides definite or open.
    fn envelope(wrap: fn(u8, &[u8]) -> Vec<u8>, chunked: bool) -> Vec<u8> {
        let ktri = tlv(0x30, &[tlv(0x02, &[0]), tlv(0x30, b"who"), tlv(0x30, b"alg"), tlv(0x04, b"the wrapped key")].concat());
        let algorithm = tlv(0x30, &[tlv(0x06, b"oid"), tlv(0x04, b"0123456789abcdef")].concat());
        let content = if chunked {
            wrap(0xa0, &[tlv(0x04, b"first half "), tlv(0x04, b"second half")].concat())
        } else {
            tlv(0x80, b"first half second half")
        };
        let info = tlv(0x30, &[tlv(0x06, b"data"), algorithm, content].concat());
        let enveloped = wrap(0x30, &[tlv(0x02, &[2]), tlv(0x31, &ktri), info].concat());
        tlv(0x30, &[tlv(0x06, b"envelopedData"), wrap(0xa0, &enveloped)].concat())
    }

    /// KMS answers in BER with lengths left open, and in pieces — both have to read the
    /// same as the plain form, or the enclave starts without its secrets.
    #[test]
    fn the_envelope_reads_whichever_way_it_is_written() {
        for (name, bytes) in [
            ("definite", envelope(tlv, false)),
            ("open-ended", envelope(open, false)),
            ("open-ended, in pieces", envelope(open, true)),
        ] {
            let (key, iv, content) = cms_parts(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(key, b"the wrapped key", "{name}");
            assert_eq!(iv, b"0123456789abcdef", "{name}");
            assert_eq!(content, b"first half second half", "{name}");
        }
        assert!(cms_parts(b"\x30\x03\x02\x01\x01").is_err());
    }

    /// The example from AWS's own SigV4 documentation, so the signing is checked against
    /// something other than our own understanding of it.
    #[test]
    fn the_signing_key_matches_aws_own_example() {
        let key = hmac(&hmac(&hmac(&hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20150830"), b"us-east-1"), b"iam"), b"aws4_request");
        assert_eq!(hex::encode(key), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
    }
}
