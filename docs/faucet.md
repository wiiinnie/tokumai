# Testnet faucet — tester onboarding

Testers run the **real purchase flow**: the app raises an ordinary $1 invoice on the NYM
rail, flagged `testnet:true`; the faucet on the same VPS pays that invoice from a Nyx
testnet wallet, and the server's chain watcher credits the account exactly as it would a
customer's payment. There is no special credit path in the server and no faucet UI in the
app — only a "Testnet purchase" toggle that the app renders **only when the server says
it is a testnet server**.

## The kill switch

One line in `/opt/scrai/.env`:

```
SCRAI_TESTNET=1
```

| state | server | clients | faucet |
|---|---|---|---|
| `SCRAI_TESTNET=1` | accepts `testnet:true` for exactly $1, flags the invoice, reports `testnet:true` + `faucetUrl` with the model list | show the toggle (default on), $1 tile only, NYM only | pays open $1 testnet invoices |
| unset / `0` | refuses `testnet:true` ("this server does not accept testnet purchases"), normal tiers only | toggle never renders; a stale client that still sends the flag gets the refusal | site serves downloads only, `/api/claim` → 403; `deploy.sh` disables the unit |

To switch off: remove the line, `sudo systemctl restart scrai scrai-faucet` (or run
`scripts/deploy.sh`, which disables `scrai-faucet.service` when the line is absent).
No client update is needed — the client has no switch of its own.

## Pieces

- **server** (`server/src/pay.rs`): `is_testnet_server()`, `TESTNET_USD = 1`, `Inv.testnet`,
  `Pay::testnet_invoices()` (read-only view for faucet + admin). Catalog reply carries
  `testnet` and `faucetUrl` (`SCRAI_FAUCET_URL`, https only).
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
- Invite code required (`scrai-faucet code new [uses] [note]`, default 3 uses).
- Daily cap `SCRAI_FAUCET_DAILY_MAX` (20), wallet reserve `SCRAI_FAUCET_RESERVE_UNYM` (5 NYM).
- 10 attempts per hour per client IP (in memory, IP hashed with a per-boot salt).
- Only `pending` invoices with ≥60 s left, `amount_usd == 1`, NYM rail.
- A $1 invoice without `testnet:true` is still refused (not a tier); `testnet:true` with
  any other amount is refused; `testnet:true` on a non-testnet server is refused.

Honest limitation: the faucet knows memo ↔ invite code and the server knows memo ↔
account — in the test a purchase is linkable to a tester. The site says so.

## VPS setup (once)

1. `.env` — add the block from `.env.example` ("Testnet faucet"): `SCRAI_TESTNET=1`,
   `SCRAI_FAUCET_URL`, `SCRAI_FAUCET_MNEMONIC` (quoted; a dedicated account on the chain
   the server watches — with `NYX_LCD_URL_TESTNET=https://validator-sandbox-1.nymtech.net/api`
   that is the Nym sandbox, funded with sandbox NYM), the chain RPC is derived from
   `NYX_LCD_URL_*` (`<validator>/api` → `<validator>`; `SCRAI_FAUCET_RPC` only overrides),
   optionally explorer + iOS links. The faucet prints its address at boot — fund that one.
2. `scripts/deploy.sh` builds all three binaries, installs `scrai-faucet.service` and
   enables it when `SCRAI_TESTNET=1`. The root-owned apply script changed with this
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
4. Invite codes: `sudo -u scrai SCRAI_DATA=/opt/scrai/data /opt/scrai/bin/scrai-faucet code new 3 "alice"`.
5. Check: `curl -s https://scrai-faucet.hermes-stakepool.de/api/status` →
   `{"testnet":true,"wallet":true,"claimsToday":0,"dailyMax":20}`.

## Downloads + version on the site

`scripts/publish-downloads.sh hermes@<vps>` uploads the newest local `.dmg` (from
`npm run tauri:build`) and any `dist/downloads/*.AppImage|*.deb` to `/opt/scrai/site/dl`
(Caddy `handle_path /dl/*` → `file_server`) plus a `manifest.json` with version, file
names, sha256 and sizes. The site reads the manifest on every page view, so the buttons,
the checksum lines and the "Testnet build 0.2.x" label always match what is downloadable
— no `.env` edit, no restart. Version = `src-tauri/tauri.conf.json`.

## Invite codes in scrai-admin

On a server with a faucet ledger the dashboard shows a FAUCET panel (funded · open
invoices · codes with uses left). Press **`c`** to mint a code (3 uses, note "admin");
it appears in the status line to copy and in the panel. This is scrai-admin's only
write besides `.env` — `state.db` stays read-only; the code goes into `faucet.db`.

## Ops

```
scrai-faucet code list      # codes + remaining uses
scrai-faucet claims         # ts · memo · code · NYM · stage · tx
journalctl -u scrai-faucet  # "funded memo … tx …" lines
```

The faucet never logs the mnemonic, the tester's IP, or the account behind a memo.
