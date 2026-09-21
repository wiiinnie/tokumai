// ---------------------------------------------------------------------------
// support.rs — the tickets both routes end in.
//
// Two processes write here: the faucet (the web form, clearnet) and the server (the app,
// over the mixnet). They already share state.db, so the tickets are a table rather than a
// port — same argument as `web_orders` in store.rs. Everything below therefore takes a
// bare `&Connection`: the faucet opens its own, the server hands over the Store's.
//
// The design is written up in docs/support.md. The two parts of it that constrain this
// file:
//
//   · An app report carries NO identity — no account key, no session. What comes back to
//     it is fetched later against a secret only the device holds, so the thread needs no
//     address and no account. We store sha256(secret); a fetch sends the secret itself.
//     There is deliberately no challenge-response: the secret is a bearer token for
//     reading one ticket, and anyone who could replay it could already read the database.
//
//   · The mail we send carries no report, only the news that one arrived (mail.rs). So a
//     ticket must be complete in the database BEFORE any notification is attempted — a
//     mail that fails is then a delay, never a lost report. `notified_at IS NULL` is the
//     work list the drain loop retries.
// ---------------------------------------------------------------------------
use rusqlite::{params, Connection, OptionalExtension};

/// Longest body we accept. The app caps at the same number; the web form is checked here
/// because a browser cannot be trusted to have.
pub const MAX_BODY: usize = 4000;
pub const MAX_SUBJECT: usize = 200;
/// One screenshot, already stripped of metadata and downscaled by the client. The limit is
/// generous for that — an over-large image is a client that did not do its half.
pub const MAX_IMAGE: usize = 2 * 1024 * 1024;
/// How many tickets one `support.fetch` may ask about.
pub const MAX_FETCH: usize = 20;

/// Where a ticket came from. It decides how it is answered, so it is stored, not guessed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Via {
    App,
    Web,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::App => "app",
            Via::Web => "web",
        }
    }
}

/// The five categories from docs/support.md. Anything else is rejected rather than
/// silently filed under "other": a category we did not send is a client we did not write.
pub const CATEGORIES: [&str; 5] = ["technical", "bug", "payment", "idea", "other"];

pub fn valid_category(c: &str) -> bool {
    CATEGORIES.contains(&c)
}

/// A new ticket, as either route hands it over.
pub struct NewTicket {
    pub via: Via,
    pub category: String,
    pub subject: String,
    pub body: String,
    /// App only, and only when the user left the toggle on.
    pub diag: Option<String>,
    /// Web only, and only if the user chose to give one.
    pub reply_to: Option<String>,
    /// App only: sha256 of the secret the device keeps.
    pub secret_hash: Option<String>,
    pub image: Option<(Vec<u8>, String)>,
}

/// A ticket as the admin console lists it (no image, no body — that is `get`).
pub struct TicketRow {
    pub id: String,
    pub created_at: i64,
    pub via: String,
    pub category: String,
    pub subject: String,
    pub status: String,
    pub has_image: bool,
    pub waiting: bool,
}

pub fn init(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS support_tickets (\
           id TEXT PRIMARY KEY,\
           created_at INTEGER NOT NULL,\
           updated_at INTEGER NOT NULL,\
           via TEXT NOT NULL,\
           category TEXT NOT NULL,\
           subject TEXT NOT NULL,\
           body TEXT NOT NULL,\
           diag TEXT,\
           reply_to TEXT,\
           secret_hash TEXT,\
           image BLOB,\
           image_mime TEXT,\
           status TEXT NOT NULL,\
           notified_at INTEGER);\
         CREATE INDEX IF NOT EXISTS support_by_secret ON support_tickets (secret_hash);\
         CREATE INDEX IF NOT EXISTS support_unnotified ON support_tickets (notified_at);\
         CREATE TABLE IF NOT EXISTS support_msgs (\
           ticket TEXT NOT NULL,\
           seq INTEGER NOT NULL,\
           at INTEGER NOT NULL,\
           who TEXT NOT NULL,\
           text TEXT NOT NULL,\
           delivered_at INTEGER,\
           PRIMARY KEY (ticket, seq));",
    )
    .map_err(|e| format!("support schema: {e}"))
}

/// Crockford-style alphabet without the letters that are read back wrongly over a phone
/// (I, L, O, U). Four characters is ~1e6 ids — ample for one operator, short enough to
/// quote — and the insert is what settles a collision, not a lookup before it.
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn candidate_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let s: String = (0..4).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
    format!("TKM-{s}")
}

pub fn secret_hash(secret: &str) -> String {
    let mut bytes = b"tokumai-support:".to_vec();
    bytes.extend_from_slice(secret.as_bytes());
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
}

/// File a ticket. Returns its id.
///
/// The row is written whole, in one statement, before anybody is told about it. A caller
/// that crashes right after this still has the report.
pub fn create(conn: &Connection, t: &NewTicket, now: i64) -> Result<String, String> {
    if !valid_category(&t.category) {
        return Err("unknown category".into());
    }
    let subject: String = t.subject.chars().take(MAX_SUBJECT).collect();
    let body: String = t.body.chars().take(MAX_BODY).collect();
    if body.trim().is_empty() {
        return Err("empty message".into());
    }
    if let Some((bytes, _)) = &t.image {
        if bytes.len() > MAX_IMAGE {
            return Err("image too large".into());
        }
    }
    let (img, mime) = match &t.image {
        Some((b, m)) => (Some(b.as_slice()), Some(m.as_str())),
        None => (None, None),
    };
    // Ten tries is far past the point where a collision means something is wrong with the
    // random source rather than with luck.
    for _ in 0..10 {
        let id = candidate_id();
        let r = conn.execute(
            "INSERT INTO support_tickets \
             (id, created_at, updated_at, via, category, subject, body, diag, reply_to, secret_hash, image, image_mime, status, notified_at) \
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'open', NULL)",
            params![id, now, t.via.as_str(), t.category, subject, body, t.diag, t.reply_to, t.secret_hash, img, mime],
        );
        match r {
            Ok(_) => {
                // The reporter's own words are message 0, so a thread reads in one place.
                let _ = conn.execute(
                    "INSERT INTO support_msgs (ticket, seq, at, who, text, delivered_at) VALUES (?1, 0, ?2, 'user', ?3, ?2)",
                    params![id, now, body],
                );
                return Ok(id);
            }
            Err(e) if e.to_string().contains("UNIQUE") => continue,
            Err(e) => return Err(format!("could not file the ticket: {e}")),
        }
    }
    Err("could not allocate a support id".into())
}

/// Tickets still owed a notification. The mail carries no content, so this is only ids —
/// see mail.rs for what actually goes out.
pub fn unnotified(conn: &Connection) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Ok(mut st) = conn.prepare(
        "SELECT id, category, via FROM support_tickets WHERE notified_at IS NULL ORDER BY created_at LIMIT 20",
    ) else {
        return out;
    };
    if let Ok(rows) = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) {
        out.extend(rows.flatten());
    }
    out
}

pub fn mark_notified(conn: &Connection, id: &str, now: i64) {
    let _ = conn.execute("UPDATE support_tickets SET notified_at = ?2 WHERE id = ?1", params![id, now]);
}

/// How many are open, and how many have been waiting longer than a day. Both numbers go
/// into the one line of the notification mail.
pub fn open_counts(conn: &Connection, now: i64) -> (i64, i64) {
    let open = conn
        .query_row("SELECT COUNT(*) FROM support_tickets WHERE status != 'done'", [], |r| r.get(0))
        .unwrap_or(0);
    let stale = conn
        .query_row(
            "SELECT COUNT(*) FROM support_tickets WHERE status = 'open' AND created_at < ?1",
            params![now - 86_400_000],
            |r| r.get(0),
        )
        .unwrap_or(0);
    (open, stale)
}

pub fn list(conn: &Connection, include_done: bool, limit: i64) -> Vec<TicketRow> {
    let sql = format!(
        "SELECT id, created_at, via, category, subject, status, image IS NOT NULL, \
                (SELECT COUNT(*) FROM support_msgs m WHERE m.ticket = t.id AND m.who = 'tokumai') = 0 \
         FROM support_tickets t {} ORDER BY created_at DESC LIMIT ?1",
        if include_done { "" } else { "WHERE status != 'done'" }
    );
    let mut out = Vec::new();
    let Ok(mut st) = conn.prepare(&sql) else { return out };
    if let Ok(rows) = st.query_map(params![limit], |r| {
        Ok(TicketRow {
            id: r.get(0)?,
            created_at: r.get(1)?,
            via: r.get(2)?,
            category: r.get(3)?,
            subject: r.get(4)?,
            status: r.get(5)?,
            has_image: r.get(6)?,
            waiting: r.get(7)?,
        })
    }) {
        out.extend(rows.flatten());
    }
    out
}

/// One ticket with its thread, for the console. The image comes separately (`image`) so a
/// listing never drags megabytes through JSON.
pub fn get(conn: &Connection, id: &str) -> Option<serde_json::Value> {
    let row = conn
        .query_row(
            "SELECT id, created_at, via, category, subject, body, diag, reply_to, status, image IS NOT NULL, image_mime \
             FROM support_tickets WHERE id = ?1",
            params![id],
            |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "createdAt": r.get::<_, i64>(1)?,
                    "via": r.get::<_, String>(2)?,
                    "category": r.get::<_, String>(3)?,
                    "subject": r.get::<_, String>(4)?,
                    "body": r.get::<_, String>(5)?,
                    "diag": r.get::<_, Option<String>>(6)?,
                    "replyTo": r.get::<_, Option<String>>(7)?,
                    "status": r.get::<_, String>(8)?,
                    "hasImage": r.get::<_, bool>(9)?,
                    "imageMime": r.get::<_, Option<String>>(10)?,
                }))
            },
        )
        .optional()
        .ok()
        .flatten()?;
    let mut msgs = Vec::new();
    if let Ok(mut st) = conn.prepare("SELECT at, who, text, delivered_at FROM support_msgs WHERE ticket = ?1 ORDER BY seq") {
        if let Ok(rows) = st.query_map(params![id], |r| {
            Ok(serde_json::json!({
                "at": r.get::<_, i64>(0)?,
                "who": r.get::<_, String>(1)?,
                "text": r.get::<_, String>(2)?,
                "delivered": r.get::<_, Option<i64>>(3)?.is_some(),
            }))
        }) {
            msgs.extend(rows.flatten());
        }
    }
    let mut row = row;
    row["msgs"] = serde_json::Value::Array(msgs);
    Some(row)
}

pub fn image(conn: &Connection, id: &str) -> Option<(Vec<u8>, String)> {
    conn.query_row(
        "SELECT image, COALESCE(image_mime, 'image/jpeg') FROM support_tickets WHERE id = ?1 AND image IS NOT NULL",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

/// Write an answer. For an app ticket it waits here until the device collects it; for a
/// web ticket it is a record of what was sent by hand from Proton.
pub fn reply(conn: &Connection, id: &str, text: &str, now: i64) -> Result<(), String> {
    let seq: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), -1) + 1 FROM support_msgs WHERE ticket = ?1", params![id], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO support_msgs (ticket, seq, at, who, text, delivered_at) VALUES (?1, ?2, ?3, 'tokumai', ?4, NULL)",
        params![id, seq, now, text],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "UPDATE support_tickets SET status = 'answered', updated_at = ?2 WHERE id = ?1 AND status != 'done'",
        params![id, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn set_status(conn: &Connection, id: &str, status: &str, now: i64) -> Result<(), String> {
    if !["open", "answered", "done"].contains(&status) {
        return Err("unknown status".into());
    }
    conn.execute("UPDATE support_tickets SET status = ?2, updated_at = ?3 WHERE id = ?1", params![id, status, now])
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// What the app collects: every answer on the tickets whose secrets it holds. Undelivered
/// ones are marked delivered as they go out — a second fetch then returns nothing new, so
/// the badge in the app clears by itself.
///
/// A ticket whose secret does not match is simply absent from the answer. There is no
/// "wrong secret" error to tell apart from "nothing waiting": one would let somebody probe
/// for which ids exist, and the app has no use for the distinction.
/// How long a report is kept after the last thing that happened to it. The privacy notice
/// promises this number ("at the latest twelve months after the last message"), so it is a
/// constant and not a setting: a box configured differently would make the notice untrue.
pub const KEEP_MS: i64 = 365 * 24 * 3600 * 1000;

/// Delete every report — its text, its picture, the whole thread — whose last activity is
/// older than `KEEP_MS`. Runs on the server's housekeeping beat, so it needs nobody to
/// remember it, which is the only kind of deletion promise worth printing. Returns how many
/// reports went.
pub fn sweep_old(conn: &Connection, now: i64) -> usize {
    let cutoff = now - KEEP_MS;
    let _ = conn.execute(
        "DELETE FROM support_msgs WHERE ticket IN (SELECT id FROM support_tickets WHERE updated_at < ?1)",
        params![cutoff],
    );
    conn.execute("DELETE FROM support_tickets WHERE updated_at < ?1", params![cutoff]).unwrap_or(0)
}

pub fn collect(conn: &Connection, secrets: &[String], now: i64) -> serde_json::Value {
    let mut tickets = Vec::new();
    for secret in secrets.iter().take(MAX_FETCH) {
        let h = secret_hash(secret);
        let Ok(Some((id, status))) = conn
            .query_row(
                "SELECT id, status FROM support_tickets WHERE secret_hash = ?1",
                params![h],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
        else {
            continue;
        };
        let mut msgs = Vec::new();
        if let Ok(mut st) =
            conn.prepare("SELECT at, who, text FROM support_msgs WHERE ticket = ?1 AND who = 'tokumai' ORDER BY seq")
        {
            if let Ok(rows) = st.query_map(params![&id], |r| {
                Ok(serde_json::json!({ "at": r.get::<_, i64>(0)?, "who": r.get::<_, String>(1)?, "text": r.get::<_, String>(2)? }))
            }) {
                msgs.extend(rows.flatten());
            }
        }
        let _ = conn.execute(
            "UPDATE support_msgs SET delivered_at = ?2 WHERE ticket = ?1 AND who = 'tokumai' AND delivered_at IS NULL",
            params![&id, now],
        );
        tickets.push(serde_json::json!({ "id": id, "status": status, "msgs": msgs }));
    }
    serde_json::json!({ "tickets": tickets })
}

/// A follow-up from the app on a thread it can prove it owns.
pub fn append_from_user(conn: &Connection, secret: &str, text: &str, now: i64) -> Result<String, String> {
    let h = secret_hash(secret);
    let id: String = conn
        .query_row("SELECT id FROM support_tickets WHERE secret_hash = ?1", params![h], |r| r.get(0))
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or("no such thread")?;
    let body: String = text.chars().take(MAX_BODY).collect();
    if body.trim().is_empty() {
        return Err("empty message".into());
    }
    let seq: i64 = conn
        .query_row("SELECT COALESCE(MAX(seq), -1) + 1 FROM support_msgs WHERE ticket = ?1", params![&id], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO support_msgs (ticket, seq, at, who, text, delivered_at) VALUES (?1, ?2, ?3, 'user', ?4, ?3)",
        params![&id, seq, now, body],
    )
    .map_err(|e| e.to_string())?;
    // Back to 'open': it is waiting on us again, and the notification drain will knock.
    conn.execute(
        "UPDATE support_tickets SET status = 'open', updated_at = ?2, notified_at = NULL WHERE id = ?1 AND status != 'done'",
        params![&id, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init(&c).unwrap();
        c
    }

    fn ticket(via: Via, secret: Option<&str>) -> NewTicket {
        NewTicket {
            via,
            category: "technical".into(),
            subject: "It hangs".into(),
            body: "A prompt sits at thinking forever.".into(),
            diag: None,
            reply_to: None,
            secret_hash: secret.map(secret_hash),
            image: None,
        }
    }

    /// The privacy notice says twelve months after the LAST message — so a thread that is
    /// still being answered must survive however old its first line is.
    #[test]
    fn reports_go_twelve_months_after_the_last_word_and_not_before() {
        let c = db();
        let day: i64 = 24 * 3600 * 1000;
        let old = create(&c, &ticket(Via::App, Some("s-old")), 0).unwrap();
        let live = create(&c, &ticket(Via::App, Some("s-live")), 0).unwrap();
        reply(&c, &old, "looking into it", 10 * day).unwrap();
        reply(&c, &live, "still on it", 300 * day).unwrap();

        assert_eq!(sweep_old(&c, 370 * day), 0, "nothing is twelve months silent yet");
        assert_eq!(sweep_old(&c, 376 * day), 1, "the thread last touched on day 10 goes");
        assert!(get(&c, &old).is_none());
        let msgs: i64 = c.query_row("SELECT COUNT(*) FROM support_msgs WHERE ticket = ?1", params![old], |r| r.get(0)).unwrap();
        assert_eq!(msgs, 0, "and its messages with it");
        assert!(get(&c, &live).is_some(), "the one answered on day 300 stays");
    }

    #[test]
    fn a_ticket_is_complete_before_anybody_is_told() {
        let c = db();
        let id = create(&c, &ticket(Via::Web, None), 1_000).unwrap();
        assert!(id.starts_with("TKM-") && id.len() == 8, "id was {id}");
        // Nothing has been notified yet, and the drain can see exactly that.
        assert_eq!(unnotified(&c).len(), 1);
        mark_notified(&c, &id, 2_000);
        assert!(unnotified(&c).is_empty());
        // The report itself is there either way — that is the point of writing it first.
        assert_eq!(get(&c, &id).unwrap()["subject"], "It hangs");
    }

    #[test]
    fn the_app_collects_only_what_its_secret_opens() {
        let c = db();
        let mine = create(&c, &ticket(Via::App, Some("secret-a")), 1_000).unwrap();
        let theirs = create(&c, &ticket(Via::App, Some("secret-b")), 1_000).unwrap();
        reply(&c, &mine, "Fixed on our side.", 2_000).unwrap();
        reply(&c, &theirs, "Not for you.", 2_000).unwrap();

        let got = collect(&c, &["secret-a".into()], 3_000);
        let ts = got["tickets"].as_array().unwrap();
        assert_eq!(ts.len(), 1, "a secret must open exactly one thread");
        assert_eq!(ts[0]["id"], mine);
        assert_eq!(ts[0]["msgs"][0]["text"], "Fixed on our side.");
        assert_ne!(ts[0]["id"], theirs);

        // A wrong secret is silence, not an error — otherwise it is an oracle for which
        // ids exist.
        assert!(collect(&c, &["nope".into()], 3_000)["tickets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_follow_up_puts_the_ticket_back_in_the_queue() {
        let c = db();
        let id = create(&c, &ticket(Via::App, Some("s")), 1_000).unwrap();
        mark_notified(&c, &id, 1_100);
        reply(&c, &id, "Try 0.6.5.", 2_000).unwrap();
        assert_eq!(get(&c, &id).unwrap()["status"], "answered");

        append_from_user(&c, "s", "Still broken.", 3_000).unwrap();
        assert_eq!(get(&c, &id).unwrap()["status"], "open");
        assert_eq!(unnotified(&c).len(), 1, "a reply from the user must knock again");
        assert!(append_from_user(&c, "wrong", "hello", 3_000).is_err());
    }

    #[test]
    fn oversized_input_is_cut_not_refused() {
        let c = db();
        let mut t = ticket(Via::Web, None);
        t.body = "x".repeat(MAX_BODY + 500);
        t.subject = "y".repeat(MAX_SUBJECT + 50);
        let id = create(&c, &t, 1_000).unwrap();
        let got = get(&c, &id).unwrap();
        assert_eq!(got["body"].as_str().unwrap().chars().count(), MAX_BODY);
        assert_eq!(got["subject"].as_str().unwrap().chars().count(), MAX_SUBJECT);
    }

    #[test]
    fn an_unknown_category_is_refused_rather_than_filed_under_other() {
        let c = db();
        let mut t = ticket(Via::Web, None);
        t.category = "urgent".into();
        assert!(create(&c, &t, 1_000).is_err());
    }
}
