# tokumai / Murmur

Anonymous AI chat over the [Nym mixnet](https://nymtech.net).

The server runs its own nym-client and is reachable **only by Nym address** — no
public IP, no DNS record, no TLS certificate. Clients address it directly and
send with reply SURBs, so the server answers someone it cannot identify.

Nothing is written to disk on the server: no prompt, no response, no identity,
no billing record.

---

## Current state

A working CLI client and server. This is the foundation for the Tauri desktop
app; the transport lives behind one seam (`src/nym/`) so replacing the shelled-out
`nym-client` with the embedded Rust SDK does not disturb anything above it.

| | |
|---|---|
| Transport | native `nym-client`, addressed by Nym address |
| Streaming | yes — chunk frames, reordered client-side |
| Billing | per-turn, priced server-side, margin never leaves the server |
| Images | Nano Banana (needs billing) + Pollinations (free, keyless) |
| Progress | live indicator driven by real events, not a timer |
| Client | persistent by default via `daemon` — cover traffic while connected |
| Credentials | not yet — see the build plan |

---

## Prerequisites

**1. `nym-client` on your PATH**

```bash
npm run nym:install      # Linux x86_64 — downloads + verifies checksum
```

On macOS and Windows there is no published binary, so build it:

```bash
cargo install --git https://github.com/nymtech/nym --bin nym-client --locked
```

Check it worked:

```bash
nym-client --version
```

**2. At least one provider credential.**

```bash
cp .env.example .env
```

Every provider is optional. One whose credential is missing is simply not
offered, and the server lists what it skipped on startup — so you can add them
one at a time.

| Provider | Env | Free tier |
|---|---|---|
| Google Gemini | `GEMINI_API_KEY` | text yes; **images `limit: 0`**, needs billing |
| Groq | `GROQ_API_KEY` | llama-3.3-70b 1k/day, llama-3.1-8b 14.4k/day |
| Cloudflare Workers AI | `CLOUDFLARE_API_TOKEN` + `CLOUDFLARE_ACCOUNT_ID` | 10k Neurons/day, covers FLUX |
| Pollinations | — none — | free, keyless, weak, third party |

**Replicate is not usable here:** it has no free tier at all — every prediction
is billed per second of compute.

**3. Node 22+** and `npm install`.

---

## Step by step

You need **two terminals**. The server and client each run their own nym-client,
on websocket ports 1977 and 1978 respectively.

### Terminal 1 — the server

**Step 1.** Create the server's mixnet identity. Once only.

```bash
npm run server:setup
```

Registers with a randomly chosen entry gateway and writes
`~/.nym/clients/scrai-server/`. To pin a gateway instead:

```bash
npm run server:setup -- --gateway <ed25519-identity>
```

**Step 2.** Start it.

```bash
npm run server
```

Wait for the banner — startup takes ~10–20s while it handshakes with the gateway:

```
──────────────────────────────────────────────────────────────
  scrai-server is listening on the mixnet

  6WsF1ptR….2QGsbDHT…@FLmQbD4c…

  point a client at it:
    npm run client -- server 6WsF1ptR….2QGsbDHT…@FLmQbD4c…
──────────────────────────────────────────────────────────────
```

**Copy that Nym address.** It is the only way to reach this server.

Leave this terminal running.

### Terminal 2 — the client

**Step 3.** Create the client's own mixnet identity. Once only.

```bash
npm run client:setup
```

**Step 4.** Point it at the server, pasting the address from step 2.

```bash
npm run client -- server 6WsF1ptR….2QGsbDHT…@FLmQbD4c…
```

**Step 5.** Ask the server what it offers. This is your first real round trip —
if it answers, the mixnet path works.

```bash
npm run client -- models
```

```
  gemini-3.5-flash-lite        text   google   trains on input
  gemini-flash-latest          text   google   trains on input
  gemini-3.5-flash             text   google   trains on input
  gemini-3.1-flash-lite-image  image  google   trains on input
  gemini-3.1-flash-image       image  google   trains on input
  gemini-2.5-flash-image       image  google   trains on input
```

**Step 6.** Choose one.

```bash
npm run client -- model gemini-3.5-flash-lite
```

**Step 7.** Ask something. The answer streams in.

```bash
npm run client -- chat "Explain a mixnet in two sentences."
```

```
… asking gemini-3.5-flash-lite over the mixnet

A mixnet is a routing protocol that encrypts and shuffles data through a series
of intermediate nodes to obscure the connection between senders and receivers…

  12 TOKU · USD 0.00012  ·  19 in · 115 out
```

**Step 8.** Images (Nano Banana). Same command — pick an image model and the
answer is written to `./images/`:

```bash
npm run client -- models                          # kinds are listed
npm run client -- model gemini-3.1-flash-lite-image
npm run client -- chat "a single red circle on white"
```

```
  ▸ images/2026-08-11T15-42-07.png  (1204 KB image/png)
  free  ·  12 in · 1,290 out
```

> **Nano Banana needs billing enabled.** Every Gemini image model reports
> `limit: 0` for the free tier — no allowance at all, not an exhausted one.
> Enable billing on the Google Cloud project behind the key and it works
> unchanged. The adapter says so explicitly rather than making you decode a 429.

For testing the image path today there are three **free, keyless** models via
[Pollinations](https://pollinations.ai):

```bash
npm run client -- model pollinations-512     # ~20-50 KB
npm run client -- model pollinations-1024    # ~85-110 KB
npm run client -- model pollinations-1536    # ~90-120 KB
npm run client -- chat "a single red circle on white"
```

```
  ▸ images/2026-08-11T14-18-42.jpg  (52 KB image/jpeg)
  52 KB · 30.6s end to end (startup + generation + transfer)
  free  ·  0 in · 0 out
```

They cost nothing and bill nothing. **But they are a third party with no API
key and no contract** — the mixnet still hides *who* is asking, but *what* is
asked is visible to an operator we have no agreement with. Fine for testing
transport, not for real use.

**Step 9.** Or hold a conversation. Each turn is its own mixnet round trip.

```bash
npm run client -- repl
```

Ctrl-D to exit. Nothing is stored.

---

## Expect it to feel slow — and what the indicator tells you

Every request crosses five hops in each direction, and the client starts a fresh
nym-client per invocation. **10–30 seconds end to end is normal.** That is the
cost of unlinkability, not a bug — a long-lived client (coming with the desktop
app) removes the startup share of it.

While it works you get a live line:

```
⠹ fetching network topology · 1s
⠼ mixnet client ready · 2s
⠦ sent · waiting for first frame · 4s
⠧ sent · waiting for first frame · 1 frame · 96 B · 5s
```

**The glyph only advances when something really happened** — a startup milestone
nym-client reported, or a frame that actually arrived. If it sits still, nothing
is arriving; that is information, not a freeze. The seconds and the frame/byte
counts tick independently because they are measured. After a few idle seconds
the line says `idle 12s` outright, so a motionless glyph never reads as a crash.

### Measured transfer

Payload size barely moves the clock — 5.6x the bytes cost 15% more wall time:

| payload | end to end |
|---|---|
| 19 KB | 28.3 s |
| 106 KB | 32.6 s |

So the time is dominated by client startup and by the model generating, not by
the mixnet moving bytes. That is why the CLI reports **duration and what is in
it** rather than a bytes-per-second figure — dividing by total time would look
like a transfer rate and be wrong. A true rate is only printed when a reply
arrives as several frames, where first-frame-to-last-frame is genuinely transfer.

---

## Commands

```
npm run client -- <command>

  install-nym              download the nym-client binary into ./bin
  setup [--gateway <id>]   initialise the local nym client
        [--latency]        ...choosing the lowest-latency gateway
  gateways                 list gateways this client knows
           [--available]   ...or browse the whole network
           [--cc DE]       ...filtered to one country
  gateway <id>             switch entry gateway (no re-init needed)
  reset                    delete this identity and start over
  address                  show this client's own nym address

  server <nym-address>     point at a scrai-server
  models                   list models the server offers
  model <name>             choose the default model

  chat <prompt…>           one question, streamed answer
       [--no-stream]       ...delivered whole instead
  repl                     interactive session (starts the client itself)

  daemon                   hold the mixnet client open (cover traffic)
  stop                     stop the held client
```

Run with no arguments to see current settings, including whether a mixnet
client is currently up.

---

## The interactive session

```bash
npm run client -- repl
```

It brings the mixnet client up **before** the prompt appears — so cover traffic
is already flowing when you type your first question — and shuts it down on
exit. If a `daemon` is already running it attaches to that one instead and
leaves it alone.

Inside the prompt:

```
  /ask <frage>        ask a question (or just type — no slash needed)
  /model              all models, numbered, current marked ▸
  /model text         text models only
  /model img-gen      image models only
  /model <n>          switch to number n from the list you last saw
  /model <name>       switch by name
  /model refresh      re-fetch the list from the server
  /gateway            show the active entry gateway
  /clear              forget the conversation history
  /help               this list
  /exit               quit (or ctrl-d)
```

Numbering always refers to the **most recently shown list**, so `/model img-gen`
followed by `/model 1` picks the first image model — not the first model
overall. The prompt shows what is currently selected: `you (gemini-3.5-flash-lite) ›`.

The catalog is cached on disk, but the first `/model` in each session re-fetches
it — otherwise adding a provider on the server would leave clients showing a
stale list with no hint that anything was missing. `/model refresh` forces it.

---

## Keep the client connected

```bash
npm run client:daemon          # holds it open; ctrl-c to stop
npm run client -- stop         # from another terminal
```

Chats reuse a running client automatically and never shut it down — only a
client they started themselves gets torn down.

**This is a privacy setting, not a speed setting.** A connected nym-client emits
loop cover traffic continuously — `DEFAULT_LOOP_COVER_STREAM_AVERAGE_DELAY` is
200ms, so roughly five packets a second — and real messages travel inside that
stream. An observer at your gateway sees the same pattern whether you are asking
something or sitting idle.

Connecting only when you have something to send throws that away. Every
connection is then, by definition, a real request, and its timing *is* the
signal: a short-lived client leaks activity timestamps, a long-lived one leaks
only presence.

Measured on this machine, it is **not** faster — a text round trip takes about
4s either way, because nym-client startup is quick here. Take the daemon for the
cover traffic, not the latency.

> Your client identity is persistent either way (same keys, same Nym address),
> so the gateway can link your sessions regardless. What the daemon hides is
> *when* you are active, not *who* you are. Per-session identities would need
> the Rust SDK's `ForgetMe` / `client_pool`.

---

## Gateways: there is one per client, not two

A nym-client registers with exactly **one active gateway**. It is not a tunnel
with an entry and an exit end — it is a mailbox. The same gateway carries your
packets out and carries replies back.

A request between two clients therefore crosses exactly two gateways, and each
side owns one of them:

```
  client                                                     server
    │                                                          │
    ├──> CLIENT'S GATEWAY ──> mix ──> mix ──> mix ──> SERVER'S GATEWAY ──>┤
         (sees client IP)      random per packet      (sees server IP)
```

So "exit gateway" is not a setting. **The client's exit is the server's
gateway**, and the server's exit is the client's gateway. You choose them by
choosing each side's own gateway — there is nothing else to configure.

The three mix nodes in between are picked **randomly for every single packet**.
That is the mixing; pinning them would defeat it, and there is no option to.

Each gateway sees the IP of the side it belongs to, and nothing else — the
client's gateway never learns which server was addressed, the server's gateway
never learns which client asked. That is why each side's jurisdiction is a real
privacy decision.

> **Changing the server's gateway changes the server's Nym address.** The
> address is `identity.encryption@gateway`, so the `@` part moves with it and
> every client has to be re-pointed at the new address.

### Commands

```bash
# client side
npm run client -- gateways --available --cc CH   # find one
npm run client -- gateway <identity>             # switch
npm run client -- gateways                       # confirm [ACTIVE]

# server side
npm run server:gw -- gateways                    # what it is registered with
npm run server:gw -- gateway <identity>          # switch (address will change!)
```

The entry gateway is the one hop that sees your IP address. The mixnet hides who
you talk to and what you send, but the gateway watches you connect — so its
jurisdiction is a real privacy decision, not a performance tweak.

**You do not need to re-init to change it.** The client keeps its identity and
its Nym address:

```bash
npm run client -- gateways --available            # 601 gateways, by country
npm run client -- gateways --available --cc CH    # identities in Switzerland
npm run client -- gateway <identity>              # switch to one
npm run client -- gateways                        # confirm — [ACTIVE] marks it
```

Registrations accumulate, so switching back to one you have used before is
instant and local. Countries are operator-declared claims, not measurements.

If you really want a **new identity** — new keys, new Nym address, nothing
carried over — that is the destructive path:

```bash
npm run client -- reset                  # explains what will be deleted
npm run client -- reset --yes            # actually delete
npm run client:setup -- --gateway <identity>
```

---

## Troubleshooting

**`nym-client not found`** — see prerequisites. It looks in `./bin` then `$PATH`.

**`no reply from the mixnet within 120s`** — the server is not running, the
address is wrong, or a gateway is having a bad day. Confirm the server terminal
still shows its banner, then try `npm run client -- gateway <other-id>`.

**`stream stalled — chunk N never arrived`** — a frame was lost in transit.
Retry. If it repeats, raise the SURB budget in `~/.scrai/cli.json`
(`replySurbs`, default 200): the server can only reply with as many packets as
you gave it SURBs for.

**`failed to select valid gateway due to incomputable latency`** — `--latency`
cannot probe from this host. Drop the flag; random selection is the default.

**Port already in use** — the server owns 1977 and the client 1978. Override
with `SCRAI_NYM_WS_PORT` *before* `setup`, since the port is baked into the
config at init.

**`a nym-client for "…" is already running (pid N)`** — a leftover process still
holds the gateway connection, and a gateway allows only one per identity.
`kill N`, then start again.

**`the client hasn't finished the data flush` / `disk I/O error` on startup** —
nym-client's reply-SURB store was left inconsistent by an unclean kill. Clear it
(the identity keys are separate and must stay):

```bash
rm -f ~/.nym/clients/scrai-server/data/persistent_reply_store.sqlite*
```

Shutting down with Ctrl-C waits for the flush, so this should not recur. If you
kill a nym-client by hand, give it a moment rather than using `kill -9`.

**`has no free-tier quota (limit: 0)`** — image models need billing enabled on
the Google Cloud project. Text models are unaffected.

---

## Environment

| Variable | Default | Purpose |
|---|---|---|
| `GEMINI_API_KEY` | — | required, server only |
| `MARGIN` | `1.1` | markup on provider cost; server-side only |
| `MIN_CHARGE_SCRAI` | `0` | floor per request; 0 keeps free models free |
| `SCRAI_FLUSH_CHARS` | `120` | chunk size before a stream frame is sent |
| `SCRAI_FLUSH_MS` | `500` | max delay before flushing a partial chunk |
| `SCRAI_TIMEOUT_MS` | `120000` | overall request timeout |
| `SCRAI_IDLE_MS` | `45000` | give up after this long with no frame |
| `SCRAI_SHUTDOWN_MS` | `4000` | grace period for nym-client to flush on exit |
| `SCRAI_VERBOSE` | — | `1` to see raw nym-client logs |

---

## TOKU — the unit

**1 TOKU = USD 0.00001**, so **10 USD = 1,000,000 TOKU**. TOKU is the only
money unit in this codebase — there is no second one and no conversion.

The unit is deliberately fine. Every exchange is priced with `ceil()`, so the
size of the unit *is* the worst-case overcharge per request — at USD 0.0001 a
one-token answer was overcharged by a full hundredth of a cent; at USD 0.00001
that error is ten times smaller.

### Where prices come from

**The price table lives on the server. The client has no pricing code at all** —
it receives retail rates in the `models.ok` response and caches them, and the
repl re-fetches at startup so a session never quotes from a stale copy.

Rates are **not** fetched from provider APIs. Providers publish prices as
documentation, not as an endpoint, so `pricing.json` is maintained by hand — the
`source` field in it is a citation of where the numbers were copied from, not a
URL anything calls.

That is a real operational gap: if a provider raises a rate, this file keeps
charging the old one and the operator absorbs the difference, silently and
indefinitely. The server therefore warns at startup when the table's `version`
date is more than 30 days old (`SCRAI_STALE_PRICING_DAYS`).

If you want the table refreshed automatically, `pricing.ts` already supports it:
set `PRICING_URL` to a table **you** publish and the server re-fetches on a TTL
(`PRICING_TTL_SEC`, default 3600 — once an hour). That does not solve the
underlying problem; it just moves the hand-maintenance somewhere central.

The `/model` listing shows retail TOKU per 1M tokens, in/out, margin included:

```
  MODEL                        KIND   VENDOR            PROMPT      ANSWER  PRIVACY
                                                  TOKU per 1,000 tokens
  gemini-3.5-flash-lite        text   google                33         275  trains on input
  llama-3.3-70b-versatile      text   groq                free        free  zero-retention
```

Read the two number columns as **two independent prices**, not a ratio: sending
1,000 prompt tokens costs 33 TOKU, receiving 1,000 answer tokens costs 275.
Answers are the expensive half — roughly 8x here — which is why `maxTokens`
dominates the cost ceiling.

Per million tokens that is 33,000 / 275,000 TOKU, i.e. USD 0.33 / 2.75 — which
is Google's published $0.30 / $2.50 plus the 10% margin.

Worked example, verified against a live request:

```
  6 prompt tokens  × 0.033 = 0.198 TOKU
  1 answer token   × 0.275 = 0.275 TOKU
                            ───────────
                             0.473  ->  ceil  ->  1 TOKU  (USD 0.00001)
```

A more typical turn — 500 in, 800 out — costs 237 TOKU (USD 0.00237), so 10 USD
buys roughly 4,200 of them.

**`maxTokens` is not a price.** It is the output ceiling per request (4096 by
default), and it exists so a worst-case cost can be computed at all — without a
ceiling there is no upper bound to check a balance against.

### Buying TOKU

Two layers, and they must not collapse into one:

| Layer | Knows | Why |
|---|---|---|
| **Account** — 24-word phrase | which account bought how much | payment, recovery |
| **Spending** — random session key | nothing about the account | unlinkable usage |

Blind issuance sits between them. Spending directly from a seed-derived account
would link every request to the purchase forever and make the mixnet decoration.

```bash
npm run client -- account new     # 24 words + a fingerprint to note
npm run client -- credit 10       # raises an invoice, waits, withdraws, funds a session
npm run client -- claim           # collect a purchase that was interrupted
```

`claim` matters more than it looks: a purchase is two round trips with a human
payment in the middle, so it *will* be interrupted. Without it the issuer holds
an entitlement nobody can collect.

**24 words, not 12.** BIP39's checksum is 4 bits at 12 words, so ~6% of
single-word typos still validate and silently open a different, empty account —
measured over 400 trials. At 24 words (8 bits) none did. The account fingerprint
is the second line of defence: note it at creation, compare it after restoring.

### Faking the payments

No money moves yet. `SCRAI_FAKE_PAYMENTS=1` selects a gateway whose invoices
settle by command instead of by payment:

**Two terminals.** `credit` raises an invoice and then waits, polling; the
settle command is what a BTCPay webhook will do on its own.

```bash
# terminal 1
npm run client -- credit 10
#   Pay USD 10.00 for 1,000,000 TOKU
#   DEV MODE — no real payment. In ANOTHER terminal, run:
#         npm run issuer -- settle fake-a861826a-…
#   ⠹ waiting for payment · checked 3× · 9m18s left · 45s

# terminal 2
npm run issuer -- invoices              # what is outstanding
npm run issuer -- settle <provider-ref> # stands in for the webhook
npm run issuer -- accounts              # entitlements not yet withdrawn
```

Terminal 1 picks it up on its next poll and finishes on its own. Polling is
every 15s rather than every second because **each check is a full mixnet round
trip** — and the thing being waited on is a human making a payment.

If terminal 1 is closed, killed or times out, nothing is lost:

```bash
npm run client -- claim
```

**This is a removable layer, not scaffolding threaded through the code.**
`src/money/gateway.ts` defines one interface; `FakeGateway` and `BTCPayGateway`
implement it, and `selectGateway()` picks by env var. Wiring the real BTCPay
means filling in one class and changing one variable — no protocol change, no
issuer change, no client change.

Two guards against shipping the fake by accident: `FakeGateway` throws unless
`SCRAI_FAKE_PAYMENTS=1` is explicitly set, and `selectGateway()` never falls
back to it — a missing configuration is an error, not a free-money default.

### Going live with BTCPay

```bash
BTCPAY_URL=https://pay.example.com
BTCPAY_STORE_ID=…
BTCPAY_API_KEY=…
# and remove SCRAI_FAKE_PAYMENTS
```

`SCRAI_FAKE_PAYMENTS` takes precedence over everything, so **comment it out** —
otherwise BTCPay is configured and silently ignored. The server warns when that
happens rather than leaving you to guess.

Before the first invoice, the store needs a **wallet linked** in BTCPay
(Store → Settings → Wallets → Bitcoin → Setup). Without one, invoice creation
fails with "No wallet has been linked to your BTCPay Store" — nothing to do with
this code. On testnet let BTCPay generate a hot wallet; on mainnet connect your
own xpub, so BTCPay can watch for payments but never spend.

`selectGateway()` then returns `BTCPayGateway`, which creates real invoices via
the Greenfield API and maps BTCPay's statuses onto ours:

| BTCPay | ours | why |
|---|---|---|
| `Settled` | paid | confirmed to the store's own configured satisfaction |
| `Processing` | pending | seen on the network but **not** confirmed |
| `New` | pending | nothing has arrived |
| `Expired` / `Invalid` | expired | window closed, underpaid, or reversed |

`Processing` deliberately does not count as paid. How many confirmations are
needed is a BTCPay setting, and honouring `Settled` means honouring whatever the
operator chose rather than second-guessing it.

**The customer never opens a web page.** Payment destinations travel to the
client over the mixnet and the QR code is rendered locally, in the terminal:

```
  ── Lightning (instant) ──
     0.00004821 BTC
     █▀▀▀▀▀█ ▄▀▄█ █▀▀▀▀▀█
     █ ███ █ ▀█▄▀ █ ███ █
     …
     lnbc48210n1p…
```

Sending the user to a hosted checkout would have their browser connect straight
to the payment server, handing over their IP at the exact moment they are least
anonymous — undoing at the till everything the mixnet protects.

### Slow confirmations

An on-chain payment can confirm an hour after the client stopped waiting. BTCPay
keeps watching — it only stops accepting *new* payments when the rate window
closes, and settles whenever a payment it already saw confirms. But somebody has
to ask.

The client gives up after 10 minutes, so the **server sweeps every 2 minutes**
(`SCRAI_SWEEP_SEC`) and re-checks every pending invoice. A withdrawal also
sweeps first, so `claim` never reports an empty account that is actually funded.

Without that sweep, a late confirmation is money taken and never credited — the
worst failure this system can have. It is covered by a test.

```bash
npm run issuer -- pending    # what is still awaiting payment or confirmation
```

**Polling, not webhooks.** `issuer.status()` asks BTCPay directly, so no inbound
connection to the issuer is needed and it requires no public endpoint. A webhook
would be faster; it would also mean opening a port. Settlement is idempotent, so
adding one later is safe — both paths can fire.

> **The issuer must eventually be its own service.** A payment gateway confirms
> by webhook over public HTTP, and the scrai-server has no public address by
> design. So the issuer is the piece that faces the internet while the AI server
> stays unreachable. They share a process today; the module boundary is already
> in place so splitting them is a deployment change.

### Test top-ups

```bash
npm run client -- credit 10        # +1,000,000 TOKU, simulates a 10 USD purchase
npm run client -- balance
```

or inside the repl: `/credit 10`, `/balance`.

Before each turn the client quotes a **ceiling** and refuses to send if the
balance cannot cover it:

```
  Not enough TOKU: 50 TOKU (USD 0.0005)
  This request costs up to 4,057 TOKU (USD 0.04057).
  Top up with:  /credit 10
```

After each turn it books the price **the server sent** — never a number computed
on this side:

```
  96 TOKU · USD 0.00096  ·  8 in · 95 out · 94 thinking
  Balance: 999,861 TOKU (USD 9.99861)
```

### Recovering a balance on another device

The 24 words rebuild everything:

```bash
npm run client -- account restore "word1 word2 … word24"
#   restored account 6889-c6f2
#   searching for funded sessions …
#   session 0: 1,600,000 TOKU (USD 16.00)  ← now active
```

Session keys are **derived from the phrase by index**, so recovery is a scan:
derive 0, 1, 2 … and ask which hold a balance, stopping after three empty ones.

Indexed rather than fixed for a reason. One permanent key would let the server
link every session you ever open; indexing gives the property HD wallets have —
one seed, many keys an observer cannot connect. The server sees unrelated public
keys and only the seed holder knows they are siblings.

> A balance created before this existed sits on a **random** key that the phrase
> cannot rebuild. `npm run client -- account` says so, and the only protection
> is to back up `~/.scrai/cli.json` until it is spent.

### The balance lives on the server

The client holds an **ed25519 private key**, not a balance. `sessionId` is the
hash of the matching public key, and every chat request carries a signature over
`sessionId ‖ counter ‖ hash(body)`.

That buys three properties:

- knowing the sessionId does not let you spend — the private key never leaves the client
- a **compromised server cannot spend user balances** either; it only stores public keys
- a captured request cannot be replayed, because the counter must strictly increase

The signature covers the **body**, so a captured signature authorises exactly the
request it came with — not a longer, costlier prompt swapped in behind it.

Verified against a running server:

| attack | result |
|---|---|
| faked local balance, no session | `unknown-session` |
| replayed a signed request | `replay` |
| swapped the prompt, kept the signature | `bad-signature` |
| raised `maxTokens`, kept the signature | `bad-signature` |
| claimed someone else's sessionId | `unknown-session` |
| redeemed the same funding token twice | `spent-token` |

### When the counter drifts

The counter must strictly increase or the server refuses the request. A client
whose counter falls behind — config restored from a backup, the same session key
used from a second machine, a wiped config — would otherwise be locked out of
its own balance permanently.

So a `replay` rejection carries the counter the server has actually seen. The
client adopts it, re-signs, and retries **once**. A second rejection is a real
problem, not drift.

Disclosing the counter is safe: it orders requests, it does not authorise them.
Replaying still needs a signature, which still needs the private key the server
has never held. And `syncCounter` only ever moves **forward** — accepting a lower
value from the server would hand an attacker a way to rewind it.

`session.status` (signed, read-only, no counter of its own) resyncs balance and
counter at repl startup. It deliberately skips the counter check, because
needing a valid counter to repair a broken counter would be circular.

### Escrow

Cost is unknown until an answer finishes, so the server **reserves the ceiling**
before calling the provider and refunds the difference afterwards. Reserving the
worst case is what keeps two in-flight requests on one session from jointly
overspending. A provider failure returns the whole reservation — you pay nothing
for an answer that never came.

```
  up to 1,127 TOKU (USD 0.01127)     <- reserved
  1 TOKU · USD 0.00001               <- actually charged
  Balance: 999,999 TOKU              <- server-reported, authoritative
```

> **Historical note (TypeScript prototype only):** the retired TS server minted
> HMAC-signed funding tokens (`SCRAI_DEV_MINT=1`), which let the issuer link a
> session to the mint that funded it. The deployed **Rust** server replaced that
> with blind Coconut issuance (`nym-compact-ecash`, see `core/src/coconut.rs`) —
> the issuer never sees a coin's serial. The TS stack remains only as the
> localhost dev UI bridge (`npm run dev`) and is never deployed.

---

## What a price line means

```
  96 TOKU · USD 0.00096  ·  8 in · 95 out · 94 thinking
```

`8 in` / `115 out` are the tokens the provider reported; `thinking` is a subset
of output, shown for transparency and never billed twice.

A free-tier model shows `free` and deducts nothing. That is deliberate — `MIN_CHARGE_SCRAI`
defaults to 0, so a model that costs nothing to serve costs nothing to use. Paid
models can never round down to zero, because the price is formed with `ceil()`.

**Rounding always favours the operator, never silently the other way.** The one
place that needs care is the internal cost figure: 57 input tokens at $0.30/1M
computes to `27960.000000000004`, and ceiling that naively would invent a cost
the provider never charged. `ceilScrai()` normalises the significant digits
first, so genuine fractions round up and float noise does not.

### Estimating before you send

The catalog carries retail rates (TOKU per 1M tokens, margin applied), so the
client can quote a ceiling without a second round trip:

```
  up to 1,127 TOKU (USD 0.01127)
  ...
  1 TOKU · USD 0.00001  ·  6 in · 1 out
```

The ceiling assumes a full `maxTokens` of output; real answers are shorter and
cheaper. It is **advisory only**. Tampering with the client's cached rates
changes the displayed number and nothing else — verified:

```
  Client manipuliert: gemini-3.5-flash Tarif auf 0/0 gesetzt
  free                 <- what the client believed
  14 TOKU             <- what the server charged
```

The server prices every exchange from its own table. A client can lie to its
user; it cannot lie to the ledger.

---

## Layout

```
src/
  protocol.ts       wire envelope — the client/server contract
  assembler.ts      reorders streamed chunks (see note below)
  adapter.ts        provider contract + registry
  adapters/         one file per provider (gemini text, gemini image)
  billing.ts        token counts -> TOKU; margin lives here
  pricing.ts        provider price table, margin-free
  types.ts          neutral internal shapes
  nym/              nym-client lifecycle, websocket, installer
  cli/              client and server entry points
  server.ts         HTTP server for the web UI preview (not on the mixnet path)
public/             web UI — design reference for the desktop app
test/               billing, protocol, assembler
```

### Why `assembler.ts` exists

nym-client splits and reassembles **one message** of any length, so the
`--no-stream` path needs no help. But streaming means many separate messages,
and the mixnet gives no ordering between them — it deliberately delays and
reorders packets, which is exactly how it breaks timing correlation.

So chunks carry a sequence number and `chat.end` carries the total, letting the
client distinguish "still arriving" from "chunk 4 never came". Nym's own Rust
SDK does the same thing in `mixnet::stream` (sequence numbers plus a 256-entry
reorder buffer). When the Tauri Rust core lands, `MixnetStream` replaces this
file entirely.

---

## Web UI

The browser UI in `public/` is not on the mixnet path — it is the design the
desktop app will carry. Preview it with:

```bash
npm run dev      # http://localhost:8787
```

Its mixnet toggle reports that routing is CLI-only, which it is until the Tauri
Rust backend exists.
