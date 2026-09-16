// ---------------------------------------------------------------------------
// mail.rs — the one mail this server sends: a knock on the operator's door.
//
// It carries NO report. Subject, a count, and the console URL — that is all, and that is
// the whole trick: a normal Proton account has no SMTP access (it is a Business feature,
// and the Bridge needs a desktop), which looked like the central problem until the content
// stopped travelling. What is left only ever goes to ONE address, our own, so there is no
// stranger's spam filter to satisfy and nothing worth handing to a sending service.
//
// Replies to a web reporter are written by hand from the operator's own mail client; the
// console offers the address and a ready subject line. Our machine therefore never mails
// a stranger, which is the genuinely hard half of e-mail. See docs/support.md.
//
// Delivery is `sendmail -t` — the interface every MTA on a Linux box offers, so Postfix can
// be installed, configured or replaced without this file changing. With no MTA and no
// SUPPORT_MAIL_TO the send is skipped and said so in the log: a box that cannot mail must
// still be able to take support, and a developer machine must not need Postfix.
// ---------------------------------------------------------------------------
use std::io::Write;
use std::process::{Command, Stdio};

/// Where the knock goes. Unset = no mail at all (dev, and any box without an MTA).
pub fn recipient() -> Option<String> {
    crate::cfg("SUPPORT_MAIL_TO").ok().map(|s| s.trim().to_string()).filter(|s| s.contains('@'))
}

fn sender() -> String {
    crate::cfg("SUPPORT_MAIL_FROM")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains('@'))
        .unwrap_or_else(|| "support@tokumai.com".into())
}

/// Printed in the mail so the operator can go straight there. Loopback by default: the
/// console is reached through an SSH tunnel, never from the internet.
fn console_url() -> String {
    crate::cfg("ADMIN_URL").ok().unwrap_or_else(|| "http://127.0.0.1:8792/admin".into())
}

/// A header value must not be able to carry a second header. Everything interpolated into
/// one goes through here — the subject holds a ticket id and a category today, but the
/// next person to touch this will put something less controlled in it.
fn header_safe(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n').take(180).collect()
}

/// Tell the operator that ticket `id` arrived. Returns Ok(false) when there is nobody to
/// tell (which is not a failure — the ticket is in the database either way).
pub fn knock(id: &str, category: &str, via: &str, open: i64, stale: i64) -> Result<bool, String> {
    let Some(to) = recipient() else {
        println!("scrai-server: support {id} filed — no SUPPORT_MAIL_TO, so no notification was sent");
        return Ok(false);
    };
    let subject = format!("[{}] New support message · {}", header_safe(id), header_safe(category));
    let waiting = if stale > 0 {
        format!("{open} open, {stale} waiting longer than a day.")
    } else {
        format!("{open} open.")
    };
    // Deliberately dull, and deliberately empty of the report itself.
    let body = format!(
        "A support message arrived from the {via}.\n\n{waiting}\n\nRead and answer: {}\n\n\
         (The report is not in this mail. Nothing a user writes leaves the machine it\n\
         arrived on — this is only a knock on the door.)\n",
        console_url()
    );
    let msg = format!(
        "From: tokumai <{from}>\r\nTo: {to}\r\nSubject: {subject}\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Auto-Submitted: auto-generated\r\n\r\n{body}",
        from = sender(),
        to = header_safe(&to),
    );
    send(&msg)?;
    Ok(true)
}

fn send(msg: &str) -> Result<(), String> {
    // -t: take the recipients from the headers. -i: a lone dot in the body is not the end
    // of the message (it cannot be here, but the flag costs nothing and the day somebody
    // pastes a log into a mail body is the day it would).
    let mut child = Command::new(sendmail_path())
        .args(["-t", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("no local MTA ({e}) — install Postfix, or unset SUPPORT_MAIL_TO"))?;
    child
        .stdin
        .as_mut()
        .ok_or("no stdin on sendmail")?
        .write_all(msg.as_bytes())
        .map_err(|e| format!("writing to sendmail: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("sendmail: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sendmail exited {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn sendmail_path() -> String {
    crate::cfg("SENDMAIL_PATH").ok().unwrap_or_else(|| "/usr/sbin/sendmail".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_cannot_grow_a_second_header() {
        let nasty = "hello\r\nBcc: someone@example.com";
        let safe = header_safe(nasty);
        assert!(!safe.contains('\r') && !safe.contains('\n'));
        assert_eq!(safe, "helloBcc: someone@example.com");
    }
}
