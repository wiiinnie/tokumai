# Support: how a user reaches us

Designed 2026-09-15, nothing built yet. Mockups (the visual source of truth):

- Website `/support` — https://claude.ai/code/artifact/aa5c5aeb-e72f-408e-a570-932174d196b5
- In-app support + admin console — https://claude.ai/code/artifact/d413ae20-b031-4e9a-8fea-880aed15c6ec

## Two routes, deliberately unequal

**From the app (the good one).** The report travels the mixnet, exactly like a question.
We learn that somebody has a problem, never who. It needs **no email address**, because
the answer comes back into the app (see *the mailbox* below).

**From the website (because the app is sometimes the thing that is broken).** A browser
request shows us the sender's IP for the duration of that request. We do not store it and
keep no access log — but that is us *promising*, where the app route is us being *unable*.
The page says so in those words. Anyone who cannot use the app still has to be able to
write; someone whose app will not start cannot be told to open the app.

**No deep link from the site into the app, ever.** A link that launches the app puts a
click from a known IP and a mixnet message a couple of seconds apart, and two events that
close together stop being two events. The page prints the path as text —
`Settings › Help › Report a problem` — and the user opens it themselves. The site also
explains *why* there is no button, because a product like this is believed when it
explains its own inconvenience.

Today the correlation is not possible after the fact: Caddy has no `log` directive
(`/var/log/caddy` is empty) and the faucet stores only `sha256(daily salt ‖ IP ‖ UA)`,
salt in memory, rotated daily, pruned after 35 days. **That is currently an accident of
configuration, not a decision** — whoever adds access logging for some other reason makes
the correlation possible again. Keep it commented in the Caddyfile.

## The mailbox: answering without an address

The server cannot push to an app. A mixnet return path lives minutes, not days, and
afterwards the server does not know who that was. So:

1. On send, the app rolls a secret and keeps it. The server gets only `sha256(secret)`.
2. The answer is written **in the admin console**, stored against the ticket.
3. On its next start the app asks `support.fetch` with a proof of the secret and collects
   whatever is waiting. The Settings row grows a dot.

Costs: the answer waits until the app is opened (no desktop push; mobile could reuse the
expiry notification). Repeated fetches tell the server those questions belong together —
not whose they are, and a thread that could not be recognised would not be a thread.
Device gone, thread gone: the secret is local, like the coins. Say that on screen.

## Mail is a knock on the door, nothing more

A normal Proton account has no SMTP (Business feature; the Bridge needs a desktop). That
stops mattering once the content stays in the console:

```
To:      <operator>@proton.me
Subject: [TKM-7QF3] New support message · technical

3 open, 1 waiting since yesterday.
Read and answer: http://127.0.0.1:8792/admin   (through the SSH tunnel)
```

- **Send it ourselves**: Postfix send-only on the VPS, DKIM-signed. It only ever writes to
  one address — ours — so there is no stranger's spam filter to satisfy and no third party
  in the path. A lost notification costs a delay, not a report.
- **Replies to web reporters are sent by hand, from Proton.** The console offers "copy
  address" and "copy subject". Our machine therefore never sends mail to a stranger, which
  is the genuinely hard half of email. Automate later if the volume ever justifies it.
- Needed: reverse lookup for the VPS, SPF + DKIM + DMARC (`p=none` first), Postfix on
  loopback only, one Proton filter so our own notification is never spam.
- **No longer needed** now that content never travels by mail: forwarding for
  `support@tokumai.com`, a paid Proton plan for a custom sending domain, and any worry
  about attachments in transit.

## Categories

`technical` · `bug` · `payment` · `idea` · `other`.

Payment is its own category because it is the only kind of report where somebody has lost
money. There is deliberately **no "account recovery"** category — we cannot help, and a
tile for it would promise otherwise; it goes in the self-help strip instead, next to "are
you on the latest version?" and "payment not arrived?", the three answers we would
otherwise type out by hand every time.

## Support ID

`TKM-7QF3`, four characters, collision-checked in SQL. It matters more here than in normal
support because reporters may be anonymous: without an address, the ID is the only thread
back. Shown large with a copy button after sending, and carried in the subject of any mail.

## Attachments: one screenshot, reusing the composer

Every step the chat pipeline performs is one a support screenshot wants anyway:

| Step | Reused |
| --- | --- |
| `attachTap()` → iOS native picker, elsewhere the file input | almost: on iOS go straight to `pickNative("library")` — no camera, no "Choose File" |
| `stripImageMeta()` — canvas re-encode: EXIF/GPS/camera gone, HEIC→JPEG, downscaled | as is |
| `makeThumb()` | as is |
| guard: OCR + `scanFileForSensitive()` | **only when it is on** |
| `upload.begin` / `upload.chunk` | as is, one image so one id |

- **One image per message** — the composer already holds a single `pendingFile`.
- **Images only** (narrower `ACCEPT_RE`), no PDF, no text file.
- **The guard is never forced on.** Support reads the same setting as chat. It may be sold
  separately and may not ship in 1.0, so the flow must work in three states: on (blur
  screen appears), off (chip says only "metadata removed", plus a plain line asking the
  user to look at the picture), and *not built at all* (identical to off). That is one
  condition, not a second code path.
- `stripImageMeta` runs in all three states — it is the canvas re-encode, not a feature.
- **Do not advertise the guard on the support screen.** Someone filing a bug report is
  having a bad day; an upsell there reads as charging for safety at the moment it is
  needed. Sell it where it works — in chat.

## Diagnostics (app only)

The reason to build this in the app at all. An opt-in block the app fills itself:

```
app 0.6.5 (sassicaia) · macOS 15.1 arm64 · server tokumai-1
gateway 38zcSs…ZHUj · connected, 2.1 s round trip
last: chat timed out after 120 s (2026-09-15 19:31 UTC)
wallet: 3 books, 1 spare note
```

Never included: prompts and answers, the balance in money, the account or seed, any device
or install id. The gateway is in the list because a stuck prompt is usually a stuck
gateway, and it is the one thing a user cannot read off a screenshot.

Shown **on** by default with the exact lines one tap away — visible consent rather than a
quiet default. Still open for a final call.

The failure sheets (a timed-out prompt, a payment that will not settle) get a quiet
"Report this" that opens the form with *that* failure's diagnostics already attached.

## Transport

| Piece | Decision |
| --- | --- |
| send | `support.send`, a new mixnet kind |
| collect | `support.fetch`, proving knowledge of the ticket secret |
| signature | **none** — support has to work precisely when paying is broken |
| abuse | 3/hour per connection, a global daily ceiling, 4 000 chars, one image. Over a mixnet there is no IP to limit; caps and a global valve are what is left |
| storage | the ticket row is written **before** the notification is attempted — a mail that fails must not be a report that never existed |

```jsonc
// app → server
{ "kind": "support.send", "category": "technical", "subject": "…", "body": "…",
  "secret_hash": "…", "diag": { … }, "upload": "u_8f21…" }
{ "kind": "support.ok", "id": "TKM-7QF3" }

// later, on any app start
{ "kind": "support.fetch", "proof": "…" }
{ "kind": "support.msgs", "msgs": [{ "from": "tokumai", "at": …, "text": "…" }] }
```

Both `tokumai-faucet` (web form) and `tokumai-server` (mixnet) write into one
`support_tickets` table in the state.db they already share, and both hand the notification
to the same local Postfix. A drain loop retries what failed.

## Order of work

1. `support_tickets` + the Support panel in the admin console. Everything reads or writes
   through it, and on its own it is already usable.
2. The web form: page, handler, honeypot, two-second time lock, per-IP ceiling — the style
   the faucet pages already use.
3. The notification mail: DNS, Postfix, DKIM, one test mail. Small now that it carries
   nothing, but it depends on other people's systems, so not last.
4. The app: `support.send`, `support.fetch`, the settings rows, the thread, the attach slot
   from the composer. Last, because a mistake there ships in a binary that cannot be fixed
   from the server.

## Still open

- The exact destination address (`hermes-stakepool@proton.me`?).
- Diagnostics default: on (as mocked) or off.
- Whether the web form's privacy sidebar keeps the green semantic colour or goes violet.
