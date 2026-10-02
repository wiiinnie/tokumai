# tokumai-admin: the operator's console

`cargo run -p tokumai-server --bin tokumai-admin` → http://127.0.0.1:8791, on the operator's
own machine, loopback only. Built 2026-10-02 in the shape of the first console
(`scrai-adminweb` in the old repository): one binary, one page, no login of its own, nothing
loaded from outside.

## What it shows, and from where

| Section | Source | What |
| --- | --- | --- |
| Enclave, Book | `admin.health` over the mixnet | uptime, requests in work, reply cache, cover queue, strikes today; the book's position, what the host has confirmed, records since the last snapshot, open holds |
| Host | SSH to the host (`~/.ssh/tokumai-probe.pem`) | the four units, enclaves running, the pulse's age, disk, memory, the book's volume, the image |
| Alarm | `aws cloudwatch describe-alarms` (profile `tokumai`) | state and reason of `tokumai-enclave-silent` |
| Usage | `admin.usage` | requests, TOKU, estimated provider cost, declines per model and kind, today / 7 / 30 days; the last 48 hours as bars; accounts active today and yesterday (a count) |
| Accounts and plans | `admin.plans` | accounts known to the book, plans by tier, rail, yearly; ending within seven days; disputed; notes minted and spent per month; revenue at list price |
| Support | `admin.account` | one account by the id the person sent in: balance, plan, past periods |
| Host log | SSH | the lines of egress.log that matter |

## What the enclave counts (`crates/enclave/src/admin.rs`)

Counts only. The hourly ring is in memory (48 hours). Daily counts and the notes' counts go
into the book's `tally` table under keys like `d:<day>:<model>:<kind>:toku` and
`notes:spent:<month>:<tier>`, flushed from memory on the tick (every 30 s), so a chat adds
no journal record of its own. No key names an account; the active-accounts count is a set
of stored names in memory for today and yesterday, and only its size leaves.

## The operator

An account like any other, with its phrase in `~/.tokumai-admin/phrase` (made on first run,
`tokumai-admin whoami` prints its id). The id is part of the enclave image
(`TOKUMAI_ADMIN_ACCOUNT` in `deploy/enclave/Dockerfile`), so the host cannot name another.
Every `admin.*` operation is signed by it and refused for anyone else.

## Trying it without the mixnet

```
TOKUMAI_ADMIN_ACCOUNT=$(cargo run -q -p tokumai-server --bin tokumai-admin -- whoami) cargo run -p tokumai-server --bin tokumai-enclave-dev
TOKUMAI_ENCLAVE_TCP=127.0.0.1:7707 cargo run -p tokumai-server --bin tokumai-admin
```

## Not yet

Stripe and App Store Connect reports (the real money), the website's statistics, the
support ticket flow of the first console, and the actions (drain, fold the book now).
