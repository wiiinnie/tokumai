# Testnet faucet — tester onboarding

Testers run the **real purchase flow**: the app raises an ordinary $1 invoice on the NYM
rail, flagged `testnet:true`; the faucet on the same VPS pays that invoice from a Nyx
testnet wallet, and the server's chain watcher credits the account exactly as it would a
customer's payment. There is no special credit path in the server and no faucet UI in the
app — only a "Testnet purchase" toggle that the app renders **only when the server says
it is a testnet server**.

## The kill switch

Two lines in `/opt/scrai/.env`:

```
TESTNET=1
TESTNET_FAUCET_ADDRESS=n1…   # the faucet wallet; scrai-faucet prints it at boot
```

**Invite codes are the only way in.** A testnet server refuses every non-testnet purchase
(it watches a test chain, where coins are free), accepts testnet invoices only as native
NYM, and credits one only when the transfer's `sender` is the pinned faucet wallet
(`nyx.rs::scan_txs`). Sandbox NYM from Nym's public faucet with the right memo and amount
does **not** settle — so nobody self-funds model spend without a code. No pin → the server
refuses testnet purchases (fail closed) and says so at boot.

| state | server | clients | faucet |
|---|---|---|---|
| `TESTNET=1` | accepts ONLY `testnet:true` for exactly $1 in NYM, settles it only from `TESTNET_FAUCET_ADDRESS`, reports `testnet:true` + `faucetUrl` with the model list | show the testnet card (fixed on), $1 tile only, NYM only | pays open $1 testnet invoices |
| unset / `0` | refuses `testnet:true` ("this server does not accept testnet purchases"), normal tiers only | toggle never renders; a stale client that still sends the flag gets the refusal | site serves downloads only, `/api/claim` → 403; `deploy.sh` disables the unit |

## Android

Built locally (no CI yet): `npm run tauri android build -- --apk --target aarch64` with
`JAVA_HOME=/opt/homebrew/opt/openjdk@17`, `ANDROID_HOME=~/Library/Android/sdk`,
`NDK_HOME=$ANDROID_HOME/ndk/27.3.13750724`. Output
`src-tauri/gen/android/app/build/outputs/apk/…/release/*.apk`, signed with the release
keystore in `~/.scrai-android/` (properties file next to it; never in the repo). Copy the
APK into `dist/downloads/` and run `publish-downloads.sh` — manifest key `android`, the site
shows the card and the Termux checksum line. First-build limits: no picture save/share, no
handover export on Android (the rest — chat, buy, guard, image generation — is the same core).

## Limits — where they live and how you notice

| limit | value | set where | when hit |
|---|---|---|---|
| faucet claims per UTC day | 20 | `FAUCET_DAILY_MAX` in `/opt/scrai/.env`, then `systemctl restart scrai-faucet` | tester sees "daily limit is reached — try again tomorrow"; **log** `scrai-faucet: DAILY LIMIT reached …`; **scrai-admin** FAUCET panel `today N / max` turns red + ECONOMY line says `DAILY LIMIT` |
| faucet wallet floor | 5 NYM | `FAUCET_RESERVE_UNYM` | tester sees "wallet is running low"; **log** `scrai-faucet: WALLET LOW — … top up …` |
| invoices per account | 5 per 10 min | `INVOICE_PER_ACCT` / `INVOICE_ACCT_WINDOW_MS` in `server/src/pay.rs` (compiled in) | app shows "too many invoices from this account — retry in ~Ns"; **log** `scrai-server: INVOICE LIMIT — account …` |
| invoices server-wide | 30 per minute | `INVOICE_GLOBAL_PER_MIN` in `server/src/pay.rs` (compiled in) | app shows "issuing too many invoices right now"; **log** `scrai-server: INVOICE LIMIT — 30 invoices/min …` (once a minute) |
| uses per invite code | 1 (default) | `scrai-faucet code new [uses]`, or `c` in scrai-admin | tester sees "this invite code has been used up"; FAUCET panel shows `0` left in grey |

Logs: `journalctl -u scrai-faucet -f` and `journalctl -u scrai -f` on the VPS.

## Release gate — forcing testers onto a new build

Two lines in `/opt/scrai/.env`, set **after** the new bundles are on the download site:

```
MIN_APP=0.3.0
UPDATE_URL=https://scrai-faucet.hermes-stakepool.de/
```

Every request now carries the app's version (`app`, from tauri.conf.json). The server
(`scrai_server::app_outdated`) refuses anything older — and anything with no version at
all, which is every 0.2.x build. What an outdated app sees: 0.2.x gets a one-entry
pseudo catalogue (`⚠ Update required — get 0.3.0 at <url>` in the model header) and the
same sentence as an error on every action; 0.3.0+ gets `update: {required, minApp, url}`
with the catalogue and shows a blocking **Update available** sheet with the link.
Log: `scrai-server: UPDATE GATE — refused …`. Unset the variable → no gate.

Developer diagnostics (cost audit with the provider's price vs. ours) exist only in
debug builds (`devBuild` from the backend) **and** only when the server sends the cost
(`DEV_AUDIT=1`, never on a tester/customer server) — a shipped binary shows no
Developer section and a release server sends no margin.

To switch off: remove the line, `sudo systemctl restart scrai scrai-faucet` (or run
`scripts/deploy.sh`, which disables `scrai-faucet.service` when the line is absent).
No client update is needed — the client has no switch of its own.

## Pieces

- **server** (`server/src/pay.rs`): `is_testnet_server()`, `TESTNET_USD = 1`, `Inv.testnet`,
  `Pay::testnet_invoices()` (read-only view for faucet + admin). Catalog reply carries
  `testnet` and `faucetUrl` (`FAUCET_URL`, https only).
- **app**: Buy dialog toggle card → `Backend.invoice(usd, method, testnet)` → Rust
  `invoice(.., testnet)` → `"testnet": true` on `invoice.create`. Invoice view shows a
  faucet note with the memo instructions and the faucet URL.
- **`scrai-faucet`** (`server/src/bin/scrai-faucet.rs`, unit `scrai-faucet.service`):
  serves the distribution site (`server/site/index.html`) + a three-call API on
  `127.0.0.1:8790`. Reads `state.db` read-only (kv `pay` snapshot), keeps its own
  `faucet.db` (invite codes, claims).
- **`scrai-admin`**: ECONOMY line `testnet faucet: N funded · X NYM · paid · open`, DAILY
  column `faucet` (UTC days). `STUCK` in red = a claim whose broadcast failed or never
  confirmed — look at it (`scrai-faucet claims`).

## What stops abuse

- Amount is the **server-pinned** `expected_unym` of the invoice — the page has no amount field.
- One payment per memo and per invoice id (`UNIQUE`, row inserted *before* broadcast).
- Invite code required (`scrai-faucet code new [uses] [note]`, default **1 use** — one code, one $1 claim).
- Daily cap `FAUCET_DAILY_MAX` (20), wallet reserve `FAUCET_RESERVE_UNYM` (5 NYM).
- 10 attempts per hour per client IP (in memory, IP hashed with a per-boot salt).
- Only `pending` invoices with ≥60 s left, `amount_usd == 1`, NYM rail.
- A $1 invoice without `testnet:true` is still refused (not a tier); `testnet:true` with
  any other amount is refused; `testnet:true` on a non-testnet server is refused.

Honest limitation: the faucet knows memo ↔ invite code and the server knows memo ↔
account — in the test a purchase is linkable to a tester. The site says so.

## VPS setup (once)

1. `.env` — add the block from `.env.example` ("Testnet faucet"): `TESTNET=1`,
   `FAUCET_URL`, `FAUCET_MNEMONIC` (quoted; a dedicated account on the chain
   the server watches — with `NYX_LCD_URL_TESTNET=https://validator-sandbox-1.nymtech.net/api`
   that is the Nym sandbox, funded with sandbox NYM), the chain RPC is derived from
   `NYX_LCD_URL_*` (`<validator>/api` → `<validator>`; `FAUCET_RPC` only overrides),
   optionally explorer + iOS links. The faucet prints its address at boot — fund that one.
2. `scripts/deploy.sh` builds all three binaries, installs `scrai-faucet.service` and
   enables it when `TESTNET=1`. The root-owned apply script changed with this
   feature — run `scripts/deploy.sh --install-apply` once (sudo prompt) before the
   normal deploy, or the old apply script will not install the faucet.
3. Caddy — one site block; DNS `scrai-faucet.hermes-stakepool.de` → the VPS IP (an A
   record cannot carry a port; the port lives only behind the proxy):

   ```
   scrai-faucet.hermes-stakepool.de {
       reverse_proxy 127.0.0.1:8790
       header {
           Strict-Transport-Security "max-age=31536000"
           -Server
       }
   }
   ```
   `sudo systemctl reload caddy`. Caddy fetches the certificate itself.
4. Invite codes: `sudo -u scrai DATA=/opt/scrai/data /opt/scrai/bin/scrai-faucet code new 3 "alice"`.
5. Check: `curl -s https://scrai-faucet.hermes-stakepool.de/api/status` →
   `{"testnet":true,"wallet":true,"claimsToday":0,"dailyMax":20}`.

## Ticketbook size: $1 on testnet, $5 on mainnet

Credit is withdrawn as uniform Coconut ticketbooks. Mainnet books are 500 coins
($5 — the minimum purchase); a testnet server (`TESTNET=1`) issues 100-coin ($1)
books so a faucet-paid $1 is exactly one book and collects immediately. Redeeming into
a session stays at 100-coin ($1) slices on both. The size is baked into the authority
keys (`data/authority.json`): the server refuses to boot if the persisted authority's
size differs from its mode, instead of silently re-keying. Switching a server's mode
therefore means, once: `systemctl stop scrai`, move `authority.json` away, start — every
ticketbook clients hold from the old key is void (they are dropped automatically on the
next spend; server-side entitlement is unaffected).

## Downloads + version on the site

`scripts/publish-downloads.sh hermes@<vps>` uploads the newest local `.dmg` (from
`npm run tauri:build`) and any `dist/downloads/*.exe|*.AppImage|*.deb` (Windows/Linux come
from the GitHub Actions workflows `build-windows.yml` / `build-linux.yml`, ~35 min each:
`gh workflow run build-<os>.yml --ref main`, then `gh run download <id> -D dist/downloads`
and flatten) to `/opt/scrai/site/dl`
(Caddy `handle_path /dl/*` → `file_server`) plus a `manifest.json` with version, file
names, sha256 and sizes. The site reads the manifest on every page view, so the buttons,
the checksum lines and the "Testnet build 0.2.x" label always match what is downloadable
— no `.env` edit, no restart. Version = `src-tauri/tauri.conf.json`.

## Invite codes in scrai-admin

On a server with a faucet ledger the dashboard shows a FAUCET panel (funded · open
invoices · codes with uses left). Press **`c`** to mint a code (1 claim, note "admin");
it appears in the status line to copy and in the panel. This is scrai-admin's only
write besides `.env` — `state.db` stays read-only; the code goes into `faucet.db`.

## Ops

```
scrai-faucet code list      # codes + remaining uses
scrai-faucet claims         # ts · memo · code · NYM · stage · tx
journalctl -u scrai-faucet  # "funded memo … tx …" lines
```

The faucet never logs the mnemonic, the tester's IP, or the account behind a memo.
