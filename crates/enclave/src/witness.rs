//! A witness outside the machine, so that a rewound book is noticed.
//!
//! The host keeps the book sealed and cannot read it — but it can hand back an older
//! snapshot and journal, and nothing inside a Nitro enclave survives a restart to notice
//! (`state`). So the enclave tells a third party how far the book has got: one tiny object
//! per mark, `mark/<generation>-<number>`, in an S3 bucket with **Object Lock in compliance
//! mode**. Nothing deletes or overwrites those objects before their retention is over: not
//! the host, not us, not the account's root user. At start the enclave reads the newest
//! mark and refuses a book that stands before it.
//!
//! What the host can still do with its credentials: write a mark of its own (a denial of
//! service, visible in the log, never a rewind) and read the marks (generation numbers,
//! nothing else). What it cannot do: make an older book look current.
//!
//! A restore we mean — the volume died and the nightly snapshot is all there is — is
//! acknowledged by the operator, from the operator's own machine, with an object under
//! `accept/`: the bucket's policy lets only that identity write there, never the host's
//! role. An acknowledgement counts once: it must be newer than the newest mark, and the
//! enclave writes a fresh mark the moment it starts on the accepted book, so the same
//! acknowledgement cannot cover a second rewind to the same place.
//!
//! Marks are written after every fold of the journal into a snapshot and every ten minutes
//! while the book moves. A rewind to within those ten minutes goes unnoticed; that is the
//! window, and it is written down here on purpose.

use crate::kms::Credentials;
use crate::ledger::Mark;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// How a mark is written, from the book's writer thread: blocking, retried by the caller.
pub type Record = std::sync::Arc<dyn Fn(Mark) -> Result<(), String> + Send + Sync>;

/// What the book is given at start: what the witness held when the enclave looked, and
/// the way to write marks from then on.
pub struct Setup {
    pub seen: Witnessed,
    pub record: Record,
}

/// Where the marks go: the bucket, its region, and who the enclave is when it writes.
pub struct Bucket {
    pub name: String,
    pub region: String,
}

/// What the witness holds, as read at start.
#[derive(Debug, Default, PartialEq)]
pub struct Witnessed {
    /// The newest mark, and when it was written (ms since the epoch, from S3's clock).
    pub mark: Option<(Mark, u64)>,
    /// Every acknowledgement: the position it accepts, and when it was written.
    pub accepts: Vec<(Mark, u64)>,
}

/// The rule, pure: may a book standing at `book` start, given what the witness holds?
/// Ok carries whether an acknowledgement was used (then a fresh mark must be written at
/// once, so it is spent).
pub fn may_start(book: Mark, seen: &Witnessed) -> Result<bool, String> {
    let Some((mark, mark_at)) = seen.mark else { return Ok(false) };
    if book >= mark {
        return Ok(false);
    }
    // Behind the witness: only an acknowledgement newer than the mark, for this very
    // position, lets it through.
    if seen.accepts.iter().any(|(at, when)| *at == book && *when > mark_at) {
        return Ok(true);
    }
    Err(format!(
        "the book stands at generation {} record {}, but the witness has seen generation {} record {}: this is an older book than the last one that ran. \
         If this restore is meant, acknowledge it from the operator's machine (deploy/aws/probe.sh accept-rewind {} {}); the enclave will not run on it otherwise",
        book.0, book.1, mark.0, mark.1, book.0, book.1
    ))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256>>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn amz_date(now_ms: u64) -> (String, String) {
    let secs = (now_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = tokumai_core::subscription::civil_from_ms((days as u64) * 86_400_000);
    let date = format!("{y:04}{m:02}{d:02}");
    (format!("{date}T{:02}{:02}{:02}Z", tod / 3600, (tod % 3600) / 60, tod % 60), date)
}

/// One signed S3 request (SigV4, virtual-hosted style), through the egress proxy like
/// everything else. `query` is the already-encoded query string without the `?`. A PUT
/// carries Content-MD5 as well: a bucket with Object Lock asks for it.
async fn call(bucket: &Bucket, creds: &Credentials, method: &str, path: &str, query: &str, body: &[u8], now_ms: u64) -> Result<(u16, String), String> {
    use base64::Engine as _;
    let host = format!("{}.s3.{}.amazonaws.com", bucket.name, bucket.region);
    let (stamp, date) = amz_date(now_ms);
    let payload_hash = hex::encode(Sha256::digest(body));
    // The headers that are signed, in the order SigV4 wants them (by name).
    let mut headers: Vec<(&str, String)> = Vec::new();
    if method == "PUT" {
        headers.push(("content-md5", base64::engine::general_purpose::STANDARD.encode(<md5::Md5 as Digest>::digest(body))));
    }
    headers.push(("host", host.clone()));
    headers.push(("x-amz-content-sha256", payload_hash.clone()));
    headers.push(("x-amz-date", stamp.clone()));
    headers.push(("x-amz-security-token", creds.session_token.clone()));
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_names: String = headers.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_names}\n{payload_hash}");
    let scope = format!("{date}/{}/s3/aws4_request", bucket.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let key = hmac(&hmac(&hmac(&hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes()), bucket.region.as_bytes()), b"s3"), b"aws4_request");
    let signature = hex::encode(hmac(&key, to_sign.as_bytes()));
    let authorization = format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}", creds.access_key_id);
    let url = if query.is_empty() { format!("https://{host}{path}") } else { format!("https://{host}{path}?{query}") };
    let client = crate::http::client();
    let mut req = match method {
        "PUT" => client.put(&url).body(body.to_vec()),
        _ => client.get(&url),
    };
    for (k, v) in &headers {
        if *k != "host" {
            req = req.header(*k, v);
        }
    }
    let res = req.header("authorization", authorization).send().await.map_err(|e| format!("the witness is unreachable: {e}"))?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|e| e.to_string())?;
    Ok((status, text))
}

/// Write the mark. The bucket's default retention locks it; nothing more to say.
pub async fn record(bucket: &Bucket, creds: &Credentials, mark: Mark, now_ms: u64) -> Result<(), String> {
    let path = format!("/mark/{}-{}", mark.0, mark.1);
    let (status, text) = call(bucket, creds, "PUT", &path, "", b"", now_ms).await?;
    if status / 100 != 2 {
        return Err(format!("the witness refused the mark ({status}): {}", text.trim().chars().take(200).collect::<String>()));
    }
    Ok(())
}

/// Read what the witness holds: the newest mark and every acknowledgement.
pub async fn read(bucket: &Bucket, creds: &Credentials, now_ms: u64) -> Result<Witnessed, String> {
    let mut seen = Witnessed::default();
    for prefix in ["mark/", "accept/"] {
        let mut token: Option<String> = None;
        loop {
            let mut query = format!("list-type=2&prefix={}", prefix.replace('/', "%2F"));
            if let Some(t) = &token {
                query.push_str("&continuation-token=");
                query.push_str(&url_encode(t));
            }
            let (status, text) = call(bucket, creds, "GET", "/", &query, b"", now_ms).await?;
            if status / 100 != 2 {
                return Err(format!("the witness could not be read ({status}): {}", text.trim().chars().take(200).collect::<String>()));
            }
            for (key, modified) in objects(&text) {
                let Some(rest) = key.strip_prefix(prefix) else { continue };
                let Some((g, n)) = rest.split_once('-') else { continue };
                let (Ok(g), Ok(n)) = (g.parse::<u64>(), n.parse::<u64>()) else { continue };
                let when = parse_time_ms(&modified).unwrap_or(0);
                if prefix == "mark/" {
                    if seen.mark.map(|(m, _)| (g, n) > m).unwrap_or(true) {
                        seen.mark = Some(((g, n), when));
                    }
                } else {
                    seen.accepts.push(((g, n), when));
                }
            }
            match next_token(&text) {
                Some(t) => token = Some(t),
                None => break,
            }
        }
    }
    Ok(seen)
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `(Key, LastModified)` of every object in a ListObjectsV2 answer. The answer is XML,
/// and these two fields are all we read, so no XML library: the tags are found by name.
fn objects(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let Some(end) = rest[start..].find("</Contents>") else { break };
        let item = &rest[start..start + end];
        let key = between(item, "<Key>", "</Key>").unwrap_or_default();
        let modified = between(item, "<LastModified>", "</LastModified>").unwrap_or_default();
        out.push((key, modified));
        rest = &rest[start + end..];
    }
    out
}

fn next_token(xml: &str) -> Option<String> {
    between(xml, "<NextContinuationToken>", "</NextContinuationToken>")
}

fn between(text: &str, open: &str, close: &str) -> Option<String> {
    let s = text.find(open)? + open.len();
    let e = text[s..].find(close)? + s;
    Some(text[s..e].replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\""))
}

/// `2026-10-02T14:03:21.000Z` → ms since the epoch. Only the shape S3 writes.
fn parse_time_ms(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day) = (d.next()?.parse::<i32>().ok()?, d.next()?.parse::<u32>().ok()?, d.next()?.parse::<u32>().ok()?);
    let time = time.trim_end_matches('Z');
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':');
    let (h, mi, sec) = (t.next()?.parse::<u64>().ok()?, t.next()?.parse::<u64>().ok()?, t.next()?.parse::<u64>().ok()?);
    let ms: u64 = format!("{:0<3}", frac).chars().take(3).collect::<String>().parse().ok()?;
    let day_ms = tokumai_core::subscription::ms_from_civil(y, m, day);
    Some(day_ms + (h * 3600 + mi * 60 + sec) * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_book_behind_the_witness_is_refused_unless_acknowledged_once() {
        let fresh = Witnessed::default();
        assert_eq!(may_start((3, 10), &fresh), Ok(false), "no witness yet: nothing to compare with");
        let seen = Witnessed { mark: Some(((5, 200), 1_000)), accepts: vec![] };
        assert_eq!(may_start((5, 200), &seen), Ok(false));
        assert_eq!(may_start((6, 0), &seen), Ok(false), "ahead of the witness is fine: the mark lags the book");
        assert!(may_start((5, 199), &seen).is_err(), "one record behind is a rewind");
        assert!(may_start((4, 900), &seen).is_err());
        // Acknowledged for exactly this position, after the mark: once.
        let acked = Witnessed { mark: Some(((5, 200), 1_000)), accepts: vec![((4, 900), 2_000)] };
        assert_eq!(may_start((4, 900), &acked), Ok(true));
        assert!(may_start((4, 899), &acked).is_err(), "an acknowledgement names one position");
        let stale = Witnessed { mark: Some(((5, 200), 3_000)), accepts: vec![((4, 900), 2_000)] };
        assert!(may_start((4, 900), &stale).is_err(), "an acknowledgement older than the newest mark is spent");
    }

    #[test]
    fn the_listing_is_read_for_keys_and_times() {
        let xml = r#"<?xml version="1.0"?><ListBucketResult><Name>b</Name><IsTruncated>false</IsTruncated>
            <Contents><Key>mark/5-200</Key><LastModified>2026-10-02T14:03:21.000Z</LastModified><Size>0</Size></Contents>
            <Contents><Key>mark/5-30</Key><LastModified>2026-10-02T13:03:21.000Z</LastModified></Contents>
            <Contents><Key>mark/junk</Key><LastModified>2026-10-02T13:03:21.000Z</LastModified></Contents></ListBucketResult>"#;
        let items = objects(xml);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].0, "mark/5-200");
        assert_eq!(parse_time_ms("2026-10-02T14:03:21.000Z"), Some(tokumai_core::subscription::ms_from_civil(2026, 10, 2) + (14 * 3600 + 3 * 60 + 21) * 1000));
        assert!(next_token(xml).is_none());
        assert_eq!(url_encode("a b/c"), "a%20b%2Fc");
    }
}
