//! tokumai-admin — the operator's console, on the operator's own machine.
//!
//! A page on loopback, as the first console was: nothing of it is reachable from outside
//! this machine, and there is no login of our own because the machine's own login is the
//! one that matters. What it shows comes from three places:
//!
//! - the enclave, over the mixnet like the app, through the `admin.*` operations signed by
//!   the operator's account (`tokumai_enclave::admin`) — counts, never a name;
//! - the host, over SSH with the probe's key: the units, the pulse, the disk, the book;
//! - AWS, with the `tokumai` profile: the alarm's state.
//!
//!     cargo run -p tokumai-server --bin tokumai-admin             # http://127.0.0.1:8791
//!     cargo run -p tokumai-server --bin tokumai-admin -- whoami   # the operator's account id
//!
//! The operator's account is a phrase in ~/.tokumai-admin/phrase, made on first use; its id
//! goes into the enclave image (TOKUMAI_ADMIN_ACCOUNT in deploy/enclave/Dockerfile), which is
//! what makes it the operator's. The enclave it talks to is the one dev-data/probe.json
//! names, as for every dev tool.

use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokumai_attest::Policy;
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokumai_client::app::{BoxFuture, Connection, Connector, MixConnector};
use tokumai_client::Transport;
use tokumai_client::gateways::EntryChoice;
use tokumai_core::account::Account;

const PAGE: &str = include_str!("../../admin/admin.html");
const SCRIPT: &str = include_str!("../../admin/admin.js");
const DEFAULT_PORT: u16 = 8791;
const MAX_BODY: usize = 8 * 1024;

fn home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".tokumai-admin")
}

/// The operator's account: a phrase of its own, kept like the app keeps the user's.
fn operator() -> Account {
    let dir = home();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("phrase");
    let phrase = match std::fs::read_to_string(&path) {
        Ok(p) => p,
        Err(_) => {
            let a = tokumai_core::account::create_account();
            std::fs::write(&path, &a.mnemonic).expect("write the operator's phrase");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            eprintln!("tokumai-admin: made the operator's account at {}", path.display());
            a.mnemonic
        }
    };
    tokumai_core::account::from_mnemonic(&phrase).expect("the operator's phrase")
}

/// JSON lines over TCP to the simulated enclave on this machine (TOKUMAI_ENCLAVE_TCP), for
/// trying the console without a mixnet: the same transport the dev client uses.
struct Tcp {
    lines: Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

struct TcpConnector(String);

impl Connector for TcpConnector {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Transport>, String>> {
        Box::pin(async move {
            let sock = tokio::net::TcpStream::connect(&self.0).await.map_err(|e| format!("connect to {}: {e}", self.0))?;
            let (r, w) = sock.into_split();
            Ok(Box::new(Tcp { lines: BufReader::new(r).lines(), w }) as Box<dyn Transport>)
        })
    }
}

impl Transport for Tcp {
    fn roundtrip<'a>(&'a mut self, msg: &'a [u8]) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>> {
        Box::pin(async move {
            self.w.write_all(msg).await.map_err(|e| e.to_string())?;
            self.w.write_all(b"\n").await.map_err(|e| e.to_string())?;
            let line = self.lines.next_line().await.map_err(|e| e.to_string())?.ok_or("the enclave hung up")?;
            Ok(line.into_bytes())
        })
    }
    fn reached_at(&self) -> Option<String> {
        None
    }
}

struct State {
    account: Account,
    conn: tokio::sync::Mutex<Connection>,
    target: (String, String),
    host: tokio::sync::Mutex<Option<(String, std::time::Instant)>>,
    /// The port this process listens on: the only Host it answers to.
    port: u16,
    /// A token for this run, served in the page and required on every /api call.
    token: String,
}

/// Equal, with every byte looked at whatever the first said.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let account = operator();
    if args.first().map(String::as_str) == Some("whoami") {
        println!("{}", account.account_id);
        return;
    }
    let (address, pcr0, connector): (String, String, Box<dyn Connector>) = match std::env::var("TOKUMAI_ENCLAVE_TCP").ok().filter(|a| !a.is_empty()) {
        Some(tcp) => (tcp.clone(), "simulated".into(), Box::new(TcpConnector(tcp))),
        None => {
            let (address, pcr0) = tokumai_server::probe_target();
            let address = std::env::var("TOKUMAI_ENCLAVE").ok().or(address).expect("no enclave: dev-data/probe.json (deploy/aws/probe.sh deploy) or TOKUMAI_ENCLAVE");
            let pcr0 = std::env::var("TOKUMAI_PCR0").ok().or(pcr0).expect("no PCR0: dev-data/probe.json or TOKUMAI_PCR0");
            (address.clone(), pcr0, Box::new(MixConnector::new(&address, EntryChoice::Random)))
        }
    };
    let policy = if pcr0 == "simulated" {
        let root: [u8; 32] = std::fs::read(tokumai_server::dev_data().join("sim-root.key")).expect("start tokumai-enclave-dev first").try_into().expect("32 bytes");
        Policy { measurements: vec![], simulated_root: Some(tokumai_attest::sim::root_public(&root)), simulated_any_measurement: true }
    } else {
        Policy { measurements: vec![pcr0.trim().to_lowercase()], simulated_root: None, simulated_any_measurement: false }
    };
    let port: u16 = std::env::var("ADMIN_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    let state = Arc::new(State {
        account,
        conn: tokio::sync::Mutex::new(Connection::new(connector, policy)),
        target: (address.clone(), pcr0.clone()),
        host: tokio::sync::Mutex::new(None),
        port,
        token: hex::encode(rand::random::<[u8; 16]>()),
    });
    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind loopback");
    println!("tokumai-admin: http://127.0.0.1:{port}  (operator {}…, enclave {}…)", state.account.account_id.chars().take(12).collect::<String>(), address.chars().take(16).collect::<String>());
    println!("tokumai-admin: loopback only — this page is for the machine it runs on");
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { continue };
        let state = state.clone();
        tokio::spawn(async move {
            if let Some(req) = read_req(&mut sock).await {
                route(&mut sock, &req, &state).await;
            }
            let _ = sock.shutdown().await;
        });
    }
}

struct Req {
    method: String,
    path: String,
    body: Vec<u8>,
    /// The Host header, lower-cased: this process answers only its own name.
    host: String,
    /// The page's token, when the request carries it (`X-Admin-Token`).
    token: String,
}

async fn read_req(sock: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let mut lines = head.lines();
            let mut first = lines.next()?.split_whitespace();
            let method = first.next()?.to_string();
            let path = first.next()?.to_string();
            let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
            let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default();
            let len: usize = header("content-length").parse().unwrap_or(0);
            let host = header("host").to_ascii_lowercase();
            let token = header("x-admin-token");
            if len > MAX_BODY {
                return None;
            }
            let mut body = buf[end + 4..].to_vec();
            while body.len() < len {
                let n = sock.read(&mut chunk).await.ok()?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            body.truncate(len);
            return Some(Req { method, path, body, host, token });
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn send(sock: &mut tokio::net::TcpStream, status: u16, ctype: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.flush().await;
}

async fn reply(sock: &mut tokio::net::TcpStream, v: &Value) {
    send(sock, 200, "application/json", serde_json::to_string(v).unwrap_or_default().as_bytes()).await
}

async fn route(sock: &mut tokio::net::TcpStream, req: &Req, state: &Arc<State>) {
    // Loopback is not enough against a browser on this machine: a page from anywhere can
    // be pointed at 127.0.0.1 by its own name (DNS rebinding) or POST here cross-site. So:
    // only requests addressed to this process's own name, and nothing from /api without
    // the token the page was served with (audit M10, 2026-10-06).
    let own = [format!("127.0.0.1:{}", state.port), format!("localhost:{}", state.port)];
    if !own.contains(&req.host) {
        return send(sock, 404, "text/plain", b"not here").await;
    }
    if req.path.starts_with("/api/") && !same(req.token.as_bytes(), state.token.as_bytes()) {
        return send(sock, 404, "text/plain", b"not here").await;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => send(sock, 200, "text/html; charset=utf-8", PAGE.replace("{{TOKEN}}", &state.token).as_bytes()).await,
        ("GET", "/admin.js") => send(sock, 200, "text/javascript; charset=utf-8", SCRIPT.as_bytes()).await,
        ("GET", "/api/state") => reply(sock, &state_json(state).await).await,
        ("POST", "/api/account") => {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            let account = body["account"].as_str().unwrap_or("").trim().to_string();
            if account.is_empty() {
                return reply(sock, &json!({ "error": "which account?" })).await;
            }
            reply(sock, &ask(state, "admin.account", json!({ "account": account })).await.unwrap_or_else(|e| json!({ "error": e }))).await
        }
        _ => send(sock, 404, "text/plain", b"not here").await,
    }
}

/// One operator operation, over the mixnet.
async fn ask(state: &State, op: &str, body: Value) -> Result<Value, String> {
    let mut conn = state.conn.lock().await;
    let v = conn.call(&state.account, op, &body).await?;
    if v["kind"] == "error" {
        return Err(v["error"].as_str().unwrap_or("error").to_string());
    }
    Ok(v)
}

fn or_error(r: Result<Value, String>) -> Value {
    r.unwrap_or_else(|e| json!({ "error": e }))
}

async fn state_json(state: &Arc<State>) -> Value {
    // The three enclave questions one after the other on the one connection; the host and
    // AWS beside them.
    let enclave = async {
        json!({
            "health": or_error(ask(state, "admin.health", json!({})).await),
            "usage": or_error(ask(state, "admin.usage", json!({})).await),
            "plans": or_error(ask(state, "admin.plans", json!({})).await),
        })
    };
    let (enclave, host, alarm) = tokio::join!(enclave, host_json(state), alarm_json());
    json!({
        "clock": tokumai_proto::now_ms(),
        "operator": state.account.account_id,
        "target": { "address": state.target.0, "pcr0": state.target.1 },
        "enclave": enclave,
        "host": host,
        "alarm": alarm,
    })
}

/// A shell command on this machine, with a deadline.
async fn run(cmd: &str, args: &[&str], secs: u64) -> Result<String, String> {
    let (cmd_s, args_s) = (cmd.to_string(), args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    let out = tokio::time::timeout(std::time::Duration::from_secs(secs), tokio::task::spawn_blocking(move || std::process::Command::new(&cmd_s).args(&args_s).output()))
        .await
        .map_err(|_| format!("{cmd} took longer than {secs}s"))?
        .map_err(|e| format!("{cmd}: {e}"))?
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

const AWS: &[&str] = &["--profile", "tokumai", "--region", "eu-central-1", "--output", "text"];

async fn host_ip(state: &State) -> Result<String, String> {
    let mut cache = state.host.lock().await;
    if let Some((ip, at)) = &*cache {
        if at.elapsed() < std::time::Duration::from_secs(600) {
            return Ok(ip.clone());
        }
    }
    let mut args = vec!["ec2", "describe-instances", "--filters", "Name=tag:Name,Values=tokumai-probe", "Name=instance-state-name,Values=running", "--query", "Reservations[].Instances[].PublicIpAddress"];
    args.extend_from_slice(AWS);
    let ip = run("aws", &args, 20).await?.trim().to_string();
    if ip.is_empty() {
        return Err("no running instance".into());
    }
    *cache = Some((ip.clone(), std::time::Instant::now()));
    Ok(ip)
}

/// What the host says about itself, as key=value lines from one SSH session.
async fn host_json(state: &State) -> Value {
    let ip = match host_ip(state).await {
        Ok(ip) => ip,
        Err(e) => return json!({ "error": e }),
    };
    let key = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".ssh/tokumai-probe.pem");
    let script = r#"
echo "egress=$(systemctl is-active tokumai-egress)"
echo "enclave=$(systemctl is-active tokumai-enclave)"
echo "pulsewatch=$(systemctl is-active tokumai-pulse-watch)"
echo "pulsetimer=$(systemctl is-active tokumai-pulse.timer)"
echo "running=$(nitro-cli describe-enclaves 2>/dev/null | grep -c RUNNING)"
echo "pulseage=$(( $(date +%s) - $(stat -c %Y pulse.stamp 2>/dev/null || echo 0) ))"
echo "uptime=$(cut -d' ' -f1 /proc/uptime)"
echo "disk=$(df -h / | awk 'NR==2{print $3"/"$2" ("$5")"}')"
echo "bookvol=$(findmnt -n -o SOURCE /home/ec2-user/book 2>/dev/null || echo root)"
echo "booksize=$(du -sh book 2>/dev/null | cut -f1)"
echo "journal=$(stat -c %s book/book.journal 2>/dev/null || echo 0)"
echo "snapshot=$(stat -c %Y book/book.snapshot 2>/dev/null || echo 0)"
echo "image=$(grep -o 'tokumai-[a-z0-9-]*\.eif' enclave.conf 2>/dev/null)"
echo "mem=$(free -m | awk 'NR==2{print $3"/"$2" MiB"}')"
echo "---"
grep -E "^egress [a-z0-9.-]+: |^egress: nothing|pulse thread|witness|book:|serving on|enclave-run" egress.log | tail -n 14
"#;
    let ssh = run(
        "ssh",
        &["-i", key.to_str().unwrap_or(""), "-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-o", "LogLevel=ERROR", &format!("ec2-user@{ip}"), script],
        30,
    )
    .await;
    match ssh {
        Ok(text) => {
            let (kv, log) = text.split_once("\n---\n").unwrap_or((text.as_str(), ""));
            let mut m = serde_json::Map::new();
            for line in kv.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    m.insert(k.to_string(), Value::String(v.trim().to_string()));
                }
            }
            m.insert("ip".into(), Value::String(ip));
            m.insert("log".into(), Value::Array(log.lines().map(|l| Value::String(l.to_string())).collect()));
            Value::Object(m)
        }
        Err(e) => json!({ "ip": ip, "error": e }),
    }
}

async fn alarm_json() -> Value {
    let mut args = vec!["cloudwatch", "describe-alarms", "--alarm-names", "tokumai-enclave-silent", "--query", "MetricAlarms[0].[StateValue,StateUpdatedTimestamp,StateReason]"];
    args.extend_from_slice(AWS);
    match run("aws", &args, 20).await {
        Ok(text) => {
            let mut parts = text.trim().splitn(3, '\t');
            json!({ "state": parts.next().unwrap_or(""), "since": parts.next().unwrap_or(""), "reason": parts.next().unwrap_or("") })
        }
        Err(e) => json!({ "error": e }),
    }
}
