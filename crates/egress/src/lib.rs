//! The way out of a Nitro enclave.
//!
//! An enclave has no network, only a vsock channel to its host. Everything the enclave
//! connects to — its Nym gateway, the Nym API, OpenAI, Gemini, Stripe, Apple — goes as an
//! HTTP CONNECT tunnel:
//!
//! ```text
//!  enclave: app → 127.0.0.1:1080 ──[forward]── vsock:3:8080 → host: [proxy] → host:port
//! ```
//!
//! - [`forward`] runs inside the enclave: a loopback listener whose every connection is
//!   piped, byte for byte, to the host. It understands nothing of what it carries.
//! - [`serve`] runs on the host: it reads the CONNECT line, checks the destination against
//!   the [`Allowlist`], connects, answers 200, and pipes. Names are resolved here, outside.
//!
//! TLS starts inside the enclave and ends at the destination, so the host sees where a
//! connection goes and how much crosses it — which it would see anyway — and nothing else.
//! Both ends also speak plain TCP ([`Endpoint::Tcp`]), to run the whole path on a laptop.

use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Where a side listens or connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    /// (cid, port). The host is CID 3 as seen from an enclave; a listener uses VMADDR_CID_ANY.
    Vsock(u32, u32),
}

impl Endpoint {
    /// `tcp:127.0.0.1:8080` or `vsock:3:8080`.
    pub fn parse(s: &str) -> Result<Endpoint, String> {
        if let Some(rest) = s.strip_prefix("tcp:") {
            return Ok(Endpoint::Tcp(rest.to_string()));
        }
        if let Some(rest) = s.strip_prefix("vsock:") {
            let (cid, port) = rest.split_once(':').ok_or("vsock:<cid>:<port>")?;
            return Ok(Endpoint::Vsock(cid.parse().map_err(|_| "bad cid")?, port.parse().map_err(|_| "bad port")?));
        }
        Err(format!("not an endpoint: {s} (tcp:host:port or vsock:cid:port)"))
    }
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn connect(to: &Endpoint) -> io::Result<Box<dyn Stream>> {
    match to {
        Endpoint::Tcp(addr) => Ok(Box::new(tokio::net::TcpStream::connect(addr).await?)),
        #[cfg(target_os = "linux")]
        Endpoint::Vsock(cid, port) => Ok(Box::new(tokio_vsock::VsockStream::connect(tokio_vsock::VsockAddr::new(*cid, *port)).await?)),
        #[cfg(not(target_os = "linux"))]
        Endpoint::Vsock(..) => Err(io::Error::new(io::ErrorKind::Unsupported, "vsock exists only on Linux")),
    }
}

/// Accept connections on `on`, calling `handle` for each in its own task.
async fn accept_loop<F, Fut>(on: &Endpoint, handle: F) -> io::Result<()>
where
    F: Fn(Box<dyn Stream>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = Arc::new(handle);
    match on {
        Endpoint::Tcp(addr) => {
            let l = tokio::net::TcpListener::bind(addr).await?;
            loop {
                let (s, _) = l.accept().await?;
                let h = handle.clone();
                tokio::spawn(async move { h(Box::new(s)).await });
            }
        }
        #[cfg(target_os = "linux")]
        Endpoint::Vsock(cid, port) => {
            let l = tokio_vsock::VsockListener::bind(tokio_vsock::VsockAddr::new(*cid, *port))?;
            loop {
                let (s, _) = l.accept().await?;
                let h = handle.clone();
                tokio::spawn(async move { h(Box::new(s)).await });
            }
        }
        #[cfg(not(target_os = "linux"))]
        Endpoint::Vsock(..) => Err(io::Error::new(io::ErrorKind::Unsupported, "vsock exists only on Linux")),
    }
}

/// Inside the enclave: pipe every connection on `listen` (the loopback) to `host`.
pub async fn forward(listen: Endpoint, host: Endpoint) -> io::Result<()> {
    let host = Arc::new(host);
    accept_loop(&listen, move |mut inner| {
        let host = host.clone();
        async move {
            if let Ok(mut outer) = connect(&host).await {
                let _ = tokio::io::copy_bidirectional(&mut inner, &mut outer).await;
            }
        }
    })
    .await
}

/// Inside the enclave: one line to the host (its Nym address, at start). A Nitro enclave
/// has no console in production, so this is how the operator learns where it listens —
/// something the address itself tells anyone who is sent it, and the attestation proves.
pub async fn announce(host: &Endpoint, line: &str) -> io::Result<()> {
    let mut s = connect(host).await?;
    s.write_all(line.as_bytes()).await?;
    s.write_all(b"\n").await?;
    s.shutdown().await
}

/// On the host: print what the enclave announces, one line per connection (at most 4 KiB).
pub async fn hear(listen: Endpoint) -> io::Result<()> {
    accept_loop(&listen, |mut s| async move {
        let mut buf = Vec::new();
        let _ = (&mut s).take(4096).read_to_end(&mut buf).await;
        println!("enclave: {}", String::from_utf8_lossy(&buf).trim());
    })
    .await
}

/// What the enclave may ask its host for, on a channel of its own: the instance's
/// temporary AWS credentials (so it can call KMS itself — they open nothing on their own,
/// the key's policy demands an attestation as well), the sealed secrets, which only an
/// attested enclave can open, and the book the host keeps for it (sealed too: `snapshot`
/// and `journal`).
pub async fn ask_host(host: &Endpoint, what: &str) -> Result<Vec<u8>, String> {
    hand_over(host, what, &[]).await
}

/// The same channel the other way: what the enclave gives the host to keep (`put-snapshot`,
/// `add-record`). The answer says it is on the host's disk.
pub async fn tell_host(host: &Endpoint, what: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    hand_over(host, what, body).await
}

async fn hand_over(host: &Endpoint, what: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    let mut s = connect(host).await.map_err(|e| format!("the host does not answer: {e}"))?;
    s.write_all(format!("{what} {}\n", body.len()).as_bytes()).await.map_err(|e| e.to_string())?;
    if !body.is_empty() {
        s.write_all(body).await.map_err(|e| e.to_string())?;
    }
    s.flush().await.map_err(|e| e.to_string())?;
    // The answer comes with its length first (`<bytes>\n`), so a hundred kilobytes of
    // sealed secrets that end with a reset instead of a clean close is not mistaken for
    // the whole of them.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\n") && head.len() < 24 {
        match s.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&head).trim().to_string();
    if head.is_empty() {
        return Err(format!("the host had no answer for {what:?}"));
    }
    let len: usize = head.parse().map_err(|_| format!("the host answered {what:?} with {head:?} where a length was expected"))?;
    if len > (1 << 26) {
        return Err(format!("the host answered {what:?} with an impossible length ({len})"));
    }
    if len == 0 {
        return Ok(Vec::new()); // nothing kept yet — a book that has not been written to
    }
    let mut answer = vec![0u8; len];
    s.read_exact(&mut answer).await.map_err(|e| format!("the host's answer to {what:?} broke off: {e}"))?;
    Ok(answer)
}

/// On the host: answer the enclave's few questions, and nothing else. `sealed` is the file
/// with the secrets as KMS sealed them, `book` the directory where the enclave's sealed
/// book is kept (a snapshot and a journal — the host can read neither). The credentials
/// come from the instance's own role.
pub async fn serve_host<F, Fut>(listen: Endpoint, sealed: std::path::PathBuf, book: std::path::PathBuf, credentials: F) -> io::Result<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let credentials = Arc::new(credentials);
    let sealed = Arc::new(sealed);
    let book = Arc::new(book);
    accept_loop(&listen, move |mut s| {
        let (credentials, sealed, book) = (credentials.clone(), sealed.clone(), book.clone());
        async move {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while !line.ends_with(b"\n") && line.len() < 64 {
                match s.read(&mut byte).await {
                    Ok(1) => line.push(byte[0]),
                    _ => break,
                }
            }
            let line = String::from_utf8_lossy(&line).trim().to_string();
            let (what, len) = line.split_once(' ').unwrap_or((line.as_str(), "0"));
            let len: usize = len.trim().parse().unwrap_or(0);
            let mut given = vec![0u8; len.min(1 << 26)];
            let read = if given.is_empty() { Ok(()) } else { s.read_exact(&mut given).await.map(|_| ()) };
            let answer = match (read, what) {
                (Err(e), _) => Err(format!("what the enclave handed over broke off: {e}")),
                (_, "credentials") => credentials().await.map(String::into_bytes),
                (_, "sealed") => std::fs::read(sealed.as_path()).map_err(|e| e.to_string()),
                (_, "snapshot") => Ok(read_or_empty(&book.join(SNAPSHOT))),
                (_, "journal") => Ok(read_or_empty(&book.join(JOURNAL))),
                (_, "put-snapshot") => put_snapshot(&book, &given).map(|()| b"kept".to_vec()),
                (_, "add-record") => add_record(&book, &given).map(|()| b"kept".to_vec()),
                (_, other) => Err(format!("the enclave asked for {other:?}, which is not answered here")),
            };
            match answer {
                Ok(bytes) => {
                    let _ = s.write_all(format!("{}\n", bytes.len()).as_bytes()).await;
                    let _ = s.write_all(&bytes).await;
                    let _ = s.flush().await;
                    // The book is written to all day; saying so every time would drown the log.
                    if !matches!(what, "add-record" | "put-snapshot" | "snapshot" | "journal") {
                        println!("host: answered {what}");
                    }
                }
                Err(e) => println!("host: could not answer {what}: {e}"),
            }
            let _ = s.shutdown().await;
        }
    })
    .await
}

/// The two files the host keeps of the enclave's book. Sealed: this side never reads them.
pub const SNAPSHOT: &str = "book.snapshot";
pub const JOURNAL: &str = "book.journal";

fn read_or_empty(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

/// A new snapshot takes the place of the old one and the journal goes with it — in that
/// order, so a stop in between leaves a snapshot with a journal that still belongs to it.
fn put_snapshot(dir: &std::path::Path, sealed: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join("book.snapshot.new");
    std::fs::write(&tmp, sealed).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(SNAPSHOT)).map_err(|e| e.to_string())?;
    match std::fs::remove_file(dir.join(JOURNAL)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// One record onto the journal, its length first — and not answered before it is safe on
/// the disk, because the enclave answers its user once this returns.
fn add_record(dir: &std::path::Path, record: &[u8]) -> Result<(), String> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(JOURNAL)).map_err(|e| e.to_string())?;
    f.write_all(&(record.len() as u32).to_be_bytes()).map_err(|e| e.to_string())?;
    f.write_all(record).map_err(|e| e.to_string())?;
    f.sync_data().map_err(|e| e.to_string())
}

/// Which destinations the host connects to: host names by exact name or by suffix
/// (`.nymtech.net` covers every subdomain), each with the ports allowed.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    rules: Vec<(String, Vec<u16>)>,
}

impl Allowlist {
    /// One rule per line: `host-or-.suffix port[,port…]`; `#` starts a comment.
    pub fn parse(text: &str) -> Result<Allowlist, String> {
        let mut rules = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (host, ports) = line.split_once(char::is_whitespace).ok_or(format!("line {}: host and ports", n + 1))?;
            let ports: Vec<u16> = ports.split(',').map(|p| p.trim().parse().map_err(|_| format!("line {}: bad port {p}", n + 1))).collect::<Result<_, _>>()?;
            rules.push((host.to_ascii_lowercase(), ports));
        }
        Ok(Allowlist { rules })
    }

    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.rules.iter().any(|(rule, ports)| {
            let name_ok = match rule.strip_prefix('.') {
                Some(suffix) => host.ends_with(&format!(".{suffix}")) || host == suffix,
                None => host == *rule,
            };
            name_ok && ports.contains(&port)
        })
    }
}

/// Read one CONNECT request (up to the blank line), return (host, port).
async fn read_connect(s: &mut Box<dyn Stream>) -> Result<(String, u16), &'static str> {
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err("request too long");
        }
        match s.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return Err("connection closed"),
        }
    }
    let text = String::from_utf8_lossy(&head);
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    if parts.next() != Some("CONNECT") {
        return Err("only CONNECT is spoken here");
    }
    let target = parts.next().ok_or("no target")?;
    let (host, port) = target.rsplit_once(':').ok_or("no port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = port.parse().map_err(|_| "bad port")?;
    Ok((host, port))
}

/// What the host proxy reports about each tunnel (destination, bytes up, bytes down), for
/// the operator's log: nothing it would not see on the wire anyway. Called when a tunnel
/// opens (with 0, 0) and again when it closes.
pub type Report = Arc<dyn Fn(&str, u16, Result<(u64, u64), String>) + Send + Sync>;

/// On the host: the CONNECT proxy, for destinations on the `allow` list only.
pub async fn serve(listen: Endpoint, allow: Allowlist, report: Report) -> io::Result<()> {
    let allow = Arc::new(allow);
    accept_loop(&listen, move |mut s| {
        let (allow, report) = (allow.clone(), report.clone());
        async move {
            let (host, port) = match read_connect(&mut s).await {
                Ok(t) => t,
                Err(e) => {
                    let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
                    report("?", 0, Err(e.into()));
                    return;
                }
            };
            if !allow.allows(&host, port) {
                let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                report(&host, port, Err("not on the allowlist".into()));
                return;
            }
            match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(mut out) => {
                    if s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
                        return;
                    }
                    // Said at once too: a kept-alive connection may stay open for hours.
                    report(&host, port, Ok((0, 0)));
                    let r = tokio::io::copy_bidirectional(&mut s, &mut out).await.map_err(|e| e.to_string());
                    report(&host, port, r);
                }
                Err(e) => {
                    let _ = s.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    report(&host, port, Err(e.to_string()));
                }
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_takes_names_and_suffixes_with_their_ports() {
        let a = Allowlist::parse("api.openai.com 443\n.nymtech.net 443,9000 # the Nym API and gateways\n\n1.2.3.4 9001").unwrap();
        assert!(a.allows("api.openai.com", 443));
        assert!(!a.allows("api.openai.com", 80));
        assert!(!a.allows("evil-api.openai.com", 443));
        assert!(a.allows("validator.nymtech.net", 443));
        assert!(a.allows("VALIDATOR.nymtech.net.", 9000));
        assert!(!a.allows("nymtech.net.evil.example", 443));
        assert!(a.allows("1.2.3.4", 9001));
        assert!(Allowlist::parse("no-ports-here").is_err());
    }

    #[tokio::test]
    async fn a_tunnel_goes_through_both_ends_and_a_forbidden_one_is_refused() {
        // A destination that echoes, the host proxy, and the enclave-side forwarder — all TCP.
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (host_port, fwd_port) = (free(), free());
        let allow = Allowlist::parse(&format!("127.0.0.1 {echo_port}")).unwrap();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        let report: Report = Arc::new(move |h, p, r| log.lock().unwrap().push(format!("{h}:{p} {}", r.is_ok())));
        tokio::spawn(serve(Endpoint::Tcp(format!("127.0.0.1:{host_port}")), allow, report));
        tokio::spawn(forward(Endpoint::Tcp(format!("127.0.0.1:{fwd_port}")), Endpoint::Tcp(format!("127.0.0.1:{host_port}"))));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", fwd_port)).await.unwrap();
        s.write_all(format!("CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut head = [0u8; 39];
        s.read_exact(&mut head).await.unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200"));
        s.write_all(b"through the tunnel").await.unwrap();
        let mut back = [0u8; 18];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"through the tunnel");
        drop(s);

        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", fwd_port)).await.unwrap();
        s.write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut head = [0u8; 12];
        s.read_exact(&mut head).await.unwrap();
        assert_eq!(&head, b"HTTP/1.1 403");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(seen.lock().unwrap().iter().any(|l| l == "example.com:443 false"));
    }

    /// What the enclave asks its host for comes back whole, however large — the sealed
    /// secrets are a hundred kilobytes, and an answer cut short would look like secrets
    /// that do not open.
    #[tokio::test]
    async fn the_host_answers_in_full() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let at = Endpoint::Tcp(format!("127.0.0.1:{port}"));
        let sealed = std::env::temp_dir().join(format!("tokumai-sealed-{}.json", std::process::id()));
        let long = format!("{{\"ct\":\"{}\"}}", "x".repeat(200_000));
        std::fs::write(&sealed, &long).unwrap();
        let book = std::env::temp_dir().join(format!("tokumai-book-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&book);
        tokio::spawn(serve_host(at.clone(), sealed.clone(), book.clone(), || async { Ok("{\"AccessKeyId\":\"AK\"}".to_string()) }));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(ask_host(&at, "sealed").await.unwrap(), long.as_bytes());
        assert_eq!(ask_host(&at, "credentials").await.unwrap(), b"{\"AccessKeyId\":\"AK\"}");
        assert!(ask_host(&at, "the data key").await.is_err());

        // The book: nothing kept yet, then records, then a snapshot that replaces them.
        assert!(ask_host(&at, "snapshot").await.unwrap().is_empty());
        assert!(ask_host(&at, "journal").await.unwrap().is_empty());
        assert_eq!(tell_host(&at, "add-record", b"a held request").await.unwrap(), b"kept");
        tell_host(&at, "add-record", b"settled").await.unwrap();
        assert_eq!(ask_host(&at, "journal").await.unwrap().len(), 4 + 14 + 4 + 7);
        tell_host(&at, "put-snapshot", b"the whole book").await.unwrap();
        assert_eq!(ask_host(&at, "snapshot").await.unwrap(), b"the whole book");
        assert!(ask_host(&at, "journal").await.unwrap().is_empty());
        let _ = std::fs::remove_file(&sealed);
        let _ = std::fs::remove_dir_all(&book);
    }

}
