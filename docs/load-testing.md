# Load testing & capacity — how many users can one scrai-server serve?

`scrai-loadtest` (workspace crate `loadtest/`) stands in for N users at once: every
simulated user is its **own Nym client** (own identity, own entry gateway), speaks the
exact protocol the app speaks (envelopes, `app` stamp, account/session signatures,
canonical chat body), and records one CSV row per request. The result is a latency curve
(p50/p90/p99) and an error/timeout count **as a function of concurrent users** — the
number we need before launch, and the number that tells us *where* the server saturates.

## Where a scrai-server can saturate

```
 users ──mixnet──▶ [ server's ONE Nym client on ONE gateway ]  ← ingress: Sphinx packets/s,
                                    │                            SURB handling, gateway link
                                    ▼
                    [ single dispatch loop (main.rs) ]          ← parse, signature check,
                                    │                            reserve, persist (sqlite fsync)
                        ┌───────────┴───────────┐
                        ▼                       ▼
            [ chat: semaphore 64 ] → provider  [ pay: semaphore 16 ] → gateway/LCD
                        │  (Gemini/OpenAI latency + their rate limits)
                        ▼
                     settle on the loop → reply via SURBs (large replies = many packets)
```

Four distinct levers, each with its own signature in the numbers:

| bottleneck | what you see | fix |
|---|---|---|
| mixnet ingress (server's single client/gateway) | `ping` p50 rises with clients even though the server is idle; "duplicate fragment" noise; timeouts with no `busy` errors | more server Nym identities (multi-address server, §"Load distribution") |
| dispatch loop / disk | `session.status` + `redeem` slow down together; server CPU/iowait up | batch persistence, move signature checks off-loop |
| chat cap (64) / provider | `busy: the server is busy with too many chats` after ~30 s waits; p99 ≈ QUEUE_WAIT | raise `MAX_INFLIGHT_CHATS`, more provider keys, second server |
| reply size (SURBs) | big `models`/image replies time out while `ping` is fine | fewer/lighter replies, chunking (already done for images) |

`ping` measures the mixnet + loop only (no state, no provider) — it is the baseline every
other op is compared against. `models` adds a spawned task + a (cached) provider list.
`chat` is the money path: `session.status` → signed request → reserve → provider → settle
→ persist → reply.

## Running it

### Local, full chat path, no model costs (recommended first)

```
scripts/loadtest.sh local                 # chat, stages 5,10,20,40 clients
scripts/loadtest.sh local chat 10,25,50   # your own stages
scripts/loadtest.sh local ping 20,50,100  # ingress + loop only
```

The script starts a **throw-away server** under `.loadtest/server/` (own `.env`, own
data dir, own Nym identity) with

- `FAKE_PAYMENTS=1` — invoices settle on the first poll, so each simulated user can
  really buy $5, withdraw a 500-coin ticketbook and redeem 100 coins, exactly like the app;
- `MOCK_PROVIDER=1500:800` — every chat answers a canned 800-char text after 1.5 s
  instead of calling Gemini. The variable is **only honoured together with
  `FAKE_PAYMENTS=1`** (a real-money server ignores it and says so at boot), so it can
  never fake an answer someone paid for.

Then it runs one `scrai-loadtest` per stage. The server log is `.loadtest/server/server.log`
(`grep -c busy` for cap refusals). Knobs: `REQUESTS` (per user), `THINK_MS` (pause between a
user's requests — 0 is a stress test, 10 000–30 000 is a realistic chat user), `RAMP_MS`,
`MOCK` (`<delay_ms>:<chars>`, e.g. `MOCK=6000:2500` for a long thinking answer),
`MAX_INFLIGHT_CHATS`, `EXTRA="--fast"` (mixnet slider at the performance end).

The local box runs the server **and** N Nym clients, so above ~50 clients the harness
itself starts to matter (each client is a full Sphinx client; expect ~1 CPU-second and
~40 MB per client just to connect). For the real number, run the harness from a second
machine (or two) against the local server's address, and watch the server's CPU there.

### Against the live server (ping/models only)

```
scripts/loadtest.sh <server nym address> ping 10,25,50
scripts/loadtest.sh <server nym address> models 10,25
```

Real money, real model — so **no chat** against the VPS from the harness. `ping`/`models`
still answer the ingress question (the one that is specific to the VPS's gateway and
box). Watch the other side with `scrai-admin` on the VPS ("peak" = distinct clients with a
request in flight) and `journalctl -u scrai -f`.

### From a second machine (the real capacity numbers)

One laptop saturates its own uplink at ~40 simulated users (each user = a full Nym client
sending 80–150 SURB packets per request). `scripts/loadtest-remote.sh` puts the harness on a
Linux box with a fat link — one of our Nym nodes — entirely under `/var/tmp/scrai-loadtest`
(`LT_ROOT` to change; `/tmp` is a small RAM tmpfs on our nodes) with its own Rust toolchain,
sources, binaries and results; `clean` removes everything, nothing needs root:

```
scripts/loadtest-remote.sh install user@node                  # sync + build (10–30 min once)
scripts/loadtest-remote.sh run user@node <addr,addr,…> ping 50,100,150 both
scripts/loadtest-remote.sh status user@node                   # live progress + node CPU
scripts/loadtest-remote.sh fetch user@node                    # → loadtest/results/remote-<host>/
scripts/loadtest-remote.sh clean user@node
```

For chat load, start a throw-away fake-payments + mock server on a SECOND node
(`server-start`, addresses via `server-addr`) and point `run … chat` at it. Sizing: ~50 MB per
simulated user → ~120 users on an 8 GB box; run two boxes for more.

### Direct

```
cargo run --release -p scrai-loadtest -- --help
cargo run --release -p scrai-loadtest -- --server <addr> --mode chat --clients 25 \
    --duration 300 --think-ms 15000 --label realistic-25
```

`--duration` (seconds per user) instead of `--requests`; `--inflight K` lets one client
keep K pings/models in flight (chat stays strictly sequential per user: counter + 1);
`--gateway <id>` pins every client to one entry gateway — compare against the default
(SDK picks per client) to see whether the *harness's* entry gateway is the limit.

## Reading the results

Each run writes `loadtest/results/<ts>-<label>/samples.csv` (one row per request:
client, op, start, latency, ok, error, reply bytes) and `summary.json`, and prints

```
op                    n     ok   t/o  busy      p50      p90      p99      max     mean   req/s
chat                250    248     2     0     3.9s     6.1s    11.2s    31.0s     4.3s    1.61
coconut.withdraw     25     25     0     0     7.8s    12.0s    ...
connect              25     25     0     0     9.2s    14.5s    ...
ping                 ...
session.status       250    250     0     0     2.1s     3.4s    ...
```

- **The curve, not one point.** Run the stages and put p50/p90 of `ping` and `chat`
  against the client count. The "knee" — where p90 stops growing linearly with users and
  timeouts appear — is the server's practical capacity at that think-time. A user who
  sends every 15 s and gets a 4 s answer occupies ~27 % of a "slot"; 64 chat slots ≈ 240
  such users before the cap bites, *if* ingress keeps up.
- **`ping` vs `chat` gap** = server work + provider delay. `ping` alone rising = ingress.
- **`t/o`** (no reply in `--timeout-ms`) vs **`busy`** (the server answered: chat cap
  hit). Timeouts with an idle server mean packets are lost/late on the mixnet path — the
  server's gateway is the first suspect.
- **`connect`** = time to get a Nym client onto the mixnet; failures here are gateway
  problems on the harness side, not server capacity.
- **Funding ops** (`invoice.*`, `entitlement`, `coconut.keys`, `coconut.withdraw`,
  `redeem`) show the *onboarding storm*: withdraw is O(500) BLS signatures on the loop,
  redeem verifies O(coins) — 40 users buying in the same minute is a real scenario at
  launch day.

Compare runs with `--fast` (the app's performance slider) — the mixnet's own per-packet
delays dominate small-request latency, so the *same* server looks 2–3× faster to a client
at the performance end.

## Multi-identity server (`MIX_CLIENTS`) — built and measured 2026-09-02

One process, one state, K Nym identities on K gateways: `GATEWAY_MASTER` pins the
primary, `GATEWAY_FALLBACK=gw1,gw2` pins the extra identities (one each; K follows
from the list, `MIX_CLIENTS` only overrides it). A pin is applied on every start and
moves the identity if needed; an unpinned existing identity keeps its gateway. Every identity is another front door to the
same dispatch loop; the reply always leaves through the identity that received the
request (its SURBs live there). All addresses are written to `data/addresses.txt`.

**Traffic shape — read this before touching `MIX_SEND_MS`.** The SDK's real-traffic
stream is a constant-rate Poisson stream: every tick sends a real packet if one is queued,
else a loop-cover packet. `MIX_SEND_MS=4` with 10 identities therefore burned all
6 VPS cores on idle padding (2,500 Sphinx packets/s). `MIX_BURST=1` disables the
Poisson distribution and the loop-cover stream: real packets go out at once, idle costs
nothing (10 identities idle at 0.2 % CPU). The server has no traffic pattern to hide —
the client's anonymity is in the SURB replies, not in the server's sending shape.

**VPS ping series, harness on one Mac, burst on (p50 / p90, 0 timeouts unless noted):**

| clients | primary identity only | spread over 10 identities |
|---|---|---|
| 10 | 5.9 s / 49.9 s (1 t/o — backlog right after a restart) | 1.5 s / 2.6 s |
| 20 | 2.9 s / 5.7 s | 3.5 s / 7.3 s |
| 40 | 6.0 s / 11.8 s | 7.1 s / 12.5 s |
| 40, 10 SURBs per ping instead of 80 | 3.9 s / 8.8 s | — |

Server CPU 1–3 % throughout. Read: **up to 40 concurrent ping users, one identity is not
the bottleneck** — spreading changes nothing. The growth from 20 to 40 tracks the
*harness* (latency peaks while all 40 Mac clients are active and falls as they finish;
fewer SURBs per request = faster; `connect` slows with N too), i.e. one laptop's CPU/uplink
sending 40 × 80 SURB packets. The next step needs the harness on a second machine
(cloud VM), then 80/160 users. The 10-client "primary" row shows a real hazard of the
single path: a backlog (here: the abandoned previous series + restart) drains slowly.

**Same series from a Nym node (pl01, 6 vCPU, 1 Gbit) — the numbers that count** (ping,
think 5 s, 10 requests per user, burst on, 2026-09-02):

| users | primary identity only (p50 / p90 / p99) | spread over 10 identities |
|---|---|---|
| 50 | 2.1 s / 3.4 s / 4.3 s · 5.2 req/s | 3.1 s / 5.6 s / 13.2 s |
| 100 | 3.2 s / 5.4 s / 10.7 s · 7.7 req/s | 3.2 s / 6.1 s / 9.7 s |

0 timeouts, 0 server-side errors; VPS CPU 0–4 % throughout. **One identity carries 100
concurrently active users (~7.7 requests/s ≈ 600 SURB packets/s ingress) with a 1 s rise in
p50 over 50 users, and spreading over identities changes nothing** — the single client
path is not the limit at this scale. The harness node was: 100 simulated users cost only
66 MB RSS but 4.5 of 6 cores (SURB construction), so stages beyond ~120 users need a
second harness node. A few harness clients failed to connect with their random entry
gateway ("Internal gateway storage error") — user-side gateway luck, the reason the app
lets users pick their gateway.

**Money path from a node (pl01 → fake-payments + mock server on the same node), 40 users,
think 5 s, 10 chats each, 2026-09-02:**

| op | n | ok | p50 | p90 | p99 |
|---|---|---|---|---|---|
| invoice.create | 40 | 30 | 2.2 s | 3.1 s | 4.6 s — **10 refused: 30 invoices/min server-wide cap** (`INVOICE_GLOBAL_PER_MIN`, compiled in) |
| coconut.withdraw (500 BLS sigs on the loop) | 30 | 30 | 7.5 s | 20.9 s | 26.4 s |
| redeem (100-coin verify on the loop) | 30 | 30 | 14.3 s | 22.4 s | 30.1 s |
| session.status | 300 | 300 | 1.6 s | 6.9 s | 17.6 s |
| chat (mock answers after 1.5 s) | 300 | 300 | 2.8 s | 6.3 s | 19.7 s |

Read: **steady-state chat is fine** (2.8 s p50 of which 1.5 s is the mock, i.e. ~1.3 s mixnet
+ server). The **onboarding storm is the first real server-side limit**: 30 users buying in
the same minute queue behind each other's withdraw (500 BLS signatures) and redeem (100
BLS verifications), both of which run ON the dispatch loop — every other request
(session.status p90 6.9 s) waits behind them. And a launch spike above 30 purchases/min
gets "retry in ~60 s". Both are fixable without touching the protocol: move the BLS work
off the loop (spawn like chat/pay, mutate state on the loop), and make the invoice cap an
env knob sized for launch day.

**After the two fixes** (`INVOICE_PER_MIN`, default 120; withdraw issuance + redeem
verification in `spawn_blocking` behind `MAX_INFLIGHT_CRYPTO` = cores, entitlement
reserved before issuance and restored on failure): same run, all 40 users funded, 1040/1040
requests ok, steady-state chat 2.8–3.1 s p50. The onboarding storm itself did NOT get
faster on pl01 — it cannot: harness and server share the box, and 40 withdraws + 40 redeems
of BLS work drove the load average to 7.2 on 6 cores. Whether the loop now stays responsive
during a storm needs the fake server on its own node (harness on another).

**Isolated money path (harness pl01 → fake server pl02), server-side timing:** withdraw
issuance 12 ms, redeem verification ~810 ms, no queueing for crypto slots — yet clients saw
redeem at 14–60 s p50, getting worse with every run. Cause: the quorum store (every
accepted payment, ~100 KB each) was persisted as ONE JSON snapshot on EVERY state change,
on the loop: 17 MB and ~90 ms per write after 160 redeems, i.e. ~30 s of blocked loop per
40-user storm. Fixed 2026-09-02: append-only `quorum_records` rows + a small `quorum_meta`
blob (offenses/blacklist), legacy snapshot migrated at boot. The SURB-count experiments
(60 / 120 / 10 per request) run against that growing snapshot are therefore not
comparable; redo on fresh state. Also learned: 120 SURBs per request is worse than 60
(each SURB is an inbound packet), and 10 makes big replies (Keys, 55 packets) re-request
SURBs — size SURBs per request type instead (8 small, ~30 chat, 64 keys) and cache Keys in
the client per epoch.

**Fresh state, 40 users, 10 SURBs per request, isolated server (pl01 → pl02):**

| | snapshot persistence | append-only rows |
|---|---|---|
| chat p50 / p90 | 2.8 s / 5.4 s | 2.6 s / 4.3 s |
| session.status p50 / p90 | 1.2 s / 3.7 s | 1.0 s / 2.6 s |
| withdraw p50 | 3.0 s | 2.3 s |
| redeem p50 | 11.5 s | 9.7 s |
| slow persists (≥ 20 ms) | 30–90 ms each, every change | 0 |
| wall | 216 s | 168 s |

Remaining redeem time is transport + the 0.8 s verification; the server's loop is no
longer in the picture. **SDK hazard found on the way:** under the same storm the server
died once with a panic inside nym-client-core's ack controller
(`action_controller.rs:198`, `Arc::try_unwrap` during a retransmission of a 68-fragment
reply). Since 2026-09-02 every identity's receive task rebuilds its own client (same keys,
same address) when the SDK ends the stream, wiping a corrupt reply-SURB store on the second
attempt — one identity dying no longer takes the address, let alone the process, down.

**Built 2026-09-03 from the above:** the app sizes SURBs per reply (8 small / 16 catalogue /
30 chat / 64 keys / 64 picture chunk, was 80/150), caches the federation `Keys` reply per
server and epoch (dropped on any withdrawal failure), and treats a server's other
addresses as fallbacks: the catalog reply (and pong) carry `identities`, the app stores
them as `server_alternates` when its own address is among them, and the liveness check
switches to the first alternate that answers when the current address is silent — same
server, same balance, no user action. scrai-admin gained a "1 min" column (`peak_1m`):
most distinct clients that sent anything within one 60-second window, next to `peak`,
which only counts clients whose SLOW request (chat/catalog/payment/crypto) was in flight
at the same instant — two people whose chats don't overlap by a few seconds read as 1
there, which is correct but not what "at the same time" means to an operator.

What multi-identity DOES already buy: gateway failover (one gateway down ≠ server down)
and the ability to shard users by region/operator later. Its idle cost is ~40 MB RSS per
identity and, with burst on, no CPU.

## Load distribution — no manual server choice for users

There is no TCP front door to put a load balancer in front of: a scrai-server **is** a Nym
address, clients pick the address they send to. So "balancing" is **client-side selection
from a signed directory**, plus a **load signal the server publishes**. The pieces:

0. **Multi-identity — done** (`MIX_CLIENTS`, above); the directory lists all of a
   server's addresses.

1. **Load signal — done.** `ping` now answers
   `{"kind":"pong","load":{"inflight":N,"maxChats":M}}`: distinct clients with a request in
   flight vs the chat cap. Public, coarse, cheap (the app already pings for its latency
   badge).

2. **Multi-address server (first scaling step, cheap).** If the load test shows the knee
   in *ingress* (ping degrades before the loop or the cap), one process runs **K Nym
   identities on K different gateways** feeding the same dispatch loop and the same
   state. Nothing about money changes: every address is the same server. The directory
   lists all K addresses; the client pings them and uses the fastest / least loaded. This
   is the cheapest capacity we can add and needs no federation work.

3. **Signed directory (already planned for federation).** Instead of the hardcoded
   `KNOWN_SERVERS`/`OFFICIAL_SERVER`, the app fetches a **signed list** of servers
   (addresses, operator, authority VK, region, `maxChats`) — signed by the federation
   key, verified against a key pinned in the app. Every server can serve the directory
   over the mixnet (it is small and public); a clearnet mirror is optional.

4. **Selection policy in the client — what "not manual" means.**
   - *Choose automatically, once, at account creation/purchase*: ping the directory's
     candidates, pick by (load, latency, region preference); remember it in the wallet.
   - *Sticky while there is money on it*: a **session balance is server-local** (it does
     not federate — see `docs/federation-shared-ledger.md`) and, until the t-of-n DKG is
     live, a **coconut book is bound to the authority that issued it**. Moving a user
     mid-credit would strand credit. So re-balancing applies to *new* accounts and to
     users whose balance is 0 — which is fine: it is the arrival rate we need to spread,
     not the existing users.
   - *Automatic failover*: if the chosen server misses N pings, the client retries the
     next best directory entry for **free/non-money ops** (models, ping) and shows the
     existing "server not answering" overlay only for money ops; when the user has
     unredeemed coconut coins from the same authority (multi-address case), it just
     switches. Keep `REDEEM_CHUNK_COINS` small (100 = $1) so the stranded amount on a
     forced switch is small.
   - The manual field stays as an *advanced override* (own server / third-party), not as
     the default path.

5. **Shared DKG (t-of-n) — the real federation.** With one shared authority key, a coconut
   book is valid at every server; then the client can redeem into whichever server it
   picks *per session*, and balancing becomes per-session rather than per-account. The
   session balance still stays local — that is by design (no cross-server counter
   consensus needed).

What the load test decided (2026-09-02): the knee is NOT at the gateway up to 100 users.
Run **3 identities per server** (failover + spread, ideally not all at one gateway
operator), keep the loop/chat-cap/persistence as the capacity metrics to watch, and put
further capacity into serving nodes behind the directory. Open: the chat path at 100+
users (needs a fake-payments server on a node + a second harness node), and beyond 120
users in general.
