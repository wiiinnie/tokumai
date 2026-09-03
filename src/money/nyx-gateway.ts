// ---------------------------------------------------------------------------
// nyx-gateway.ts — accept NYM on the Nyx chain, no payment processor.
//
// There is no BTCPay for NYM. NYM is the native token of the Nyx blockchain
// (Cosmos-SDK), so "getting paid" means: hand the user ONE receive address plus
// a unique MEMO, then watch the chain for a transfer that carries that memo.
//
// WEBSOCKET FIRST, HTTP-POLL FALLBACK: the Tendermint RPC can PUSH every
// committed tx over a WebSocket subscription (`transfer.recipient='<addr>'`) —
// the instant-notification path, ideal when we run our own nyxd node. But most
// PUBLIC Nyx RPC providers disable the `/websocket` endpoint (subscriptions are
// resource-heavy) while leaving the HTTP RPC open. So we TRY the socket, verify
// it actually answers, and if it doesn't we fall back to polling the HTTP RPC
// (status heartbeat + a tx_search for new blocks every few seconds). Either way
// the PaymentGateway interface stays poll-shaped (`checkStatus`) reading an
// in-memory map, and `watchState()` reports which mode is live.
//
// WHY A MEMO, NOT A PER-INVOICE ADDRESS: Cosmos has no cheap sub-addresses like
// Monero. The memo is how nym.com correlates, too. Its one weakness is the user
// forgetting it — so the UI must show it as prominently as the amount.
//
// WHAT SETTLES: like BTCPay, the TOKU amount is fixed when the invoice is
// RAISED (the issuer does that). Here we only fix the NYM amount, at the locked
// USD/NYM rate, so "pay exactly this many NYM" stays honest for the window.
//
// PRIVACY NOTE: the SERVER fetches the price and watches the chain. The user's
// wallet talks to the chain, never to us — so nothing about the payment reveals
// their IP to the scrai-server, the same guarantee the mixnet gives the chat.
// Querying a public Nyx RPC does reveal OUR receive address to that endpoint;
// running our own nyxd node would remove even that. Env-swappable on purpose.
// ---------------------------------------------------------------------------

import { randomBytes } from "node:crypto";
import { Tendermint37Client, WebsocketClient, HttpClient } from "@cosmjs/tendermint-rpc";
import { decodeTxRaw } from "@cosmjs/proto-signing";
import type { InvoiceStatus, PaymentGateway, Invoice, WatchState } from "./gateway.js";

type WatchMode = "websocket" | "polling";
const withTimeout = <T>(p: Promise<T>, ms: number, label: string): Promise<T> =>
  Promise.race([p, new Promise<T>((_, r) => setTimeout(() => r(new Error(`${label} timed out`)), ms))]);
// wss://host/websocket <-> https://host  (derive one endpoint from the other)
function wsToHttp(ws: string): string {
  return ws.replace(/^ws/, "http").replace(/\/websocket\/?$/, "").replace(/\/$/, "");
}
function httpToWs(http: string): string {
  return http.replace(/^http/, "ws").replace(/\/$/, "") + "/websocket";
}

const MICRO = 1_000_000; // 1 NYM = 1_000_000 unym
const DENOM = "unym"; // native NYM only — never an IBC-wrapped denom

interface Pending {
  memo: string;
  expectedUnym: bigint; // exact amount we quoted, in unym
  nymAmount: string; // human string we showed, for the option
  status: InvoiceStatus;
  expiresAt: number;
  txHash?: string; // set once seen, for the operator log
}

export class NyxGateway implements PaymentGateway {
  readonly name = "nyx";
  readonly isFake = false;

  private readonly pending = new Map<string, Pending>(); // memo -> Pending
  private tm: Tendermint37Client | null = null;
  private mode: WatchMode | null = null;
  private connecting: Promise<void> | null = null;
  private subscription: { unsubscribe(): void } | null = null;
  private blockSub: { unsubscribe(): void } | null = null;
  private reconnectTimer: NodeJS.Timeout | null = null;
  private pollTimer: NodeJS.Timeout | null = null;
  private lastScanned = 0; // highest block height scanned in polling mode

  private readonly wsUrl: string | null;
  private readonly httpUrl: string;

  // Liveness. The tx subscription only fires on an actual payment, which is far
  // too rare to prove the socket is alive — so we ALSO watch block headers as a
  // heartbeat (subscription in WS mode, status() in poll mode). If blocks are
  // arriving, we would hear a payment too.
  private connected = false;
  private lastBlock: { height: number; at: number } | null = null;

  // price cache — one CoinGecko hit per minute is plenty and keeps a burst of
  // invoices from rate-limiting us.
  private price = { usdPerNym: 0, at: 0 };

  constructor(
    private readonly receiveAddress: string,
    endpoints: { ws?: string; http?: string },
    private readonly opts: {
      ttlMs?: number;
      priceUrl?: string; // full CoinGecko URL override, mostly for tests
    } = {},
  ) {
    if (!receiveAddress) throw new Error("NyxGateway: NYX_RECEIVE_ADDRESS is required");
    const ws = endpoints.ws?.trim() || null;
    const http = endpoints.http?.trim() || null;
    if (!ws && !http) throw new Error("NyxGateway: set NYX_RPC_WS and/or NYX_RPC_HTTP");
    // WS is attempted ONLY when explicitly configured. Setting HTTP alone is the
    // operator saying "this provider has no websocket" — so we go straight to
    // polling, with no wasted WS probe. HTTP is always derived from WS as the
    // fallback when only WS is given.
    this.wsUrl = ws;
    this.httpUrl = http || (ws ? wsToHttp(ws) : "");
  }

  private ttl(): number {
    return this.opts.ttlMs ?? 15 * 60_000;
  }

  /**
   * Connect the chain watcher and pre-fetch the price at STARTUP, so the first
   * real invoice doesn't eat the connection handshake (notably the ~5s WS probe
   * before the HTTP fallback). Fire-and-forget; failures are logged, not fatal.
   */
  warmup(): void {
    this.ensureConnected()
      .then(() => this.usdPerNym().catch(() => 0))
      .catch((e) => console.warn("[nyx] warmup failed (non-fatal):", e instanceof Error ? e.message : e));
  }

  // ---- chain connection -----------------------------------------------------

  /** Connect (WebSocket if it truly answers, else HTTP polling). Idempotent; safe
   *  to call before every invoice so a first payment never races an open. */
  private async ensureConnected(): Promise<void> {
    if (this.tm) return;
    if (this.connecting) return this.connecting;
    this.connecting = (async () => {
      if (this.wsUrl) {
        try {
          await this.connectWs();
          return;
        } catch (e) {
          console.warn(
            `[nyx] websocket ${this.wsUrl} unavailable (${e instanceof Error ? e.message : e}); ` +
              `falling back to HTTP polling of ${this.httpUrl}`,
          );
          this.teardown();
        }
      }
      await this.connectHttp();
    })();
    try {
      await this.connecting;
    } finally {
      this.connecting = null;
    }
  }

  private async connectWs(): Promise<void> {
    const client = new WebsocketClient(this.wsUrl!, (err) => {
      console.error("[nyx] websocket error:", err instanceof Error ? err.message : err);
      this.scheduleReconnect();
    });
    const tm = await Tendermint37Client.create(client);
    // Public providers often ACCEPT the connection but never upgrade/answer, so
    // prove it really replies before committing to the socket.
    await withTimeout(tm.status(), 5_000, "websocket status");
    this.tm = tm;
    this.mode = "websocket";
    this.subscription = tm.subscribeTx(`transfer.recipient='${this.receiveAddress}'`).subscribe({
      next: (ev) => this.onTx(ev.tx, ev.result.events, ev.result.code, ev.hash),
      error: (err) => {
        console.error("[nyx] subscription dropped:", err instanceof Error ? err.message : err);
        this.scheduleReconnect();
      },
    });
    this.blockSub = tm.subscribeNewBlockHeader().subscribe({
      next: (ev) => {
        const height = (ev as { height?: number; header?: { height?: number } }).height ??
          (ev as { header?: { height?: number } }).header?.height ?? 0;
        this.connected = true;
        this.lastBlock = { height, at: Date.now() };
      },
      error: (err) => {
        console.error("[nyx] block heartbeat dropped:", err instanceof Error ? err.message : err);
        this.scheduleReconnect();
      },
    });
    this.connected = true;
    console.log(`[nyx] watching ${this.receiveAddress} via websocket ${this.wsUrl}`);
    await this.catchUp().catch((e) =>
      console.warn("[nyx] catch-up failed (non-fatal):", e instanceof Error ? e.message : e),
    );
  }

  private async connectHttp(): Promise<void> {
    const tm = await Tendermint37Client.create(new HttpClient(this.httpUrl));
    this.tm = tm;
    this.mode = "polling";
    console.log(`[nyx] watching ${this.receiveAddress} via HTTP polling ${this.httpUrl}`);
    await this.pollOnce(); // immediate heartbeat + baseline height
    this.pollTimer = setInterval(() => {
      void this.pollOnce();
    }, 6_000);
  }

  /** One polling cycle: heartbeat (status) + scan new blocks for payments to us. */
  private async pollOnce(): Promise<void> {
    if (!this.tm) return;
    try {
      const s = await this.tm.status();
      const latest = s.syncInfo.latestBlockHeight;
      this.connected = true;
      this.lastBlock = { height: latest, at: Date.now() };
      if (this.lastScanned === 0) this.lastScanned = Math.max(1, latest - 20); // small first-poll look-back
      const anyPending = [...this.pending.values()].some((p) => p.status === "pending");
      if (anyPending && latest > this.lastScanned) {
        const res = await this.tm.txSearchAll({
          query: `transfer.recipient='${this.receiveAddress}' AND tx.height>${this.lastScanned} AND tx.height<=${latest}`,
        });
        for (const tx of res.txs) this.onTx(tx.tx, tx.result.events, tx.result.code, tx.hash);
      }
      this.lastScanned = latest;
    } catch (e) {
      this.connected = false;
      console.warn("[nyx] poll failed:", e instanceof Error ? e.message : e);
    }
  }

  private teardown(): void {
    try {
      this.subscription?.unsubscribe();
      this.blockSub?.unsubscribe();
    } catch {
      /* ignore */
    }
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
    this.subscription = null;
    this.blockSub = null;
    this.tm = null;
    this.mode = null;
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer) return;
    this.connected = false;
    this.teardown();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if ([...this.pending.values()].some((p) => p.status === "pending")) {
        this.ensureConnected().catch((e) =>
          console.error("[nyx] reconnect failed:", e instanceof Error ? e.message : e),
        );
      }
    }, 5_000);
  }

  /** One-shot search for recent transfers to us (WS mode), to settle anything the
   *  live socket missed. Bounded to recent blocks so it never walks all history. */
  private async catchUp(): Promise<void> {
    if (!this.tm) return;
    if (![...this.pending.values()].some((p) => p.status === "pending")) return;
    const status = await this.tm.status();
    const latest = status.syncInfo.latestBlockHeight;
    const from = Math.max(1, latest - 5_000);
    const res = await this.tm.txSearchAll({
      query: `transfer.recipient='${this.receiveAddress}' AND tx.height>=${from}`,
    });
    for (const tx of res.txs) {
      this.onTx(tx.tx, tx.result.events, tx.result.code, tx.hash);
    }
  }

  /** Match one chain tx against a pending invoice by memo. Idempotent. */
  private onTx(txBytes: Uint8Array, events: readonly { type: string; attributes: readonly { key: string; value: string }[] }[], code: number, hash: Uint8Array): void {
    if (code !== 0) return; // failed tx moved no money
    let memo = "";
    try {
      memo = decodeTxRaw(txBytes).body.memo?.trim() ?? "";
    } catch {
      return; // not decodable — not ours
    }
    if (!memo) return;
    const inv = this.pending.get(memo);
    if (!inv || inv.status === "paid") return;

    // Sum every unym actually received at OUR address in this tx.
    let received = 0n;
    for (const ev of events) {
      if (ev.type !== "transfer") continue;
      const attrs = ev.attributes;
      const toUs = attrs.some((a) => a.key === "recipient" && a.value === this.receiveAddress);
      if (!toUs) continue;
      for (const a of attrs) {
        if (a.key === "amount") received += parseUnym(a.value);
      }
    }
    if (received < inv.expectedUnym) {
      console.warn(
        `[nyx] underpaid memo=${memo}: got ${received} unym, expected ${inv.expectedUnym} — left pending`,
      );
      return;
    }
    inv.status = "paid";
    inv.txHash = toHex(hash);
    console.log(`[nyx] PAID memo=${memo} ${received} unym tx=${inv.txHash}`);
  }

  // ---- price ----------------------------------------------------------------

  private async usdPerNym(): Promise<number> {
    const fresh = Date.now() - this.price.at < 60_000 && this.price.usdPerNym > 0;
    if (fresh) return this.price.usdPerNym;
    const url =
      this.opts.priceUrl ??
      "https://api.coingecko.com/api/v3/simple/price?ids=nym&vs_currencies=usd";
    const res = await fetch(url, { signal: AbortSignal.timeout(10_000) });
    if (!res.ok) throw new Error(`price feed ${res.status}`);
    const body = (await res.json()) as { nym?: { usd?: number } };
    const usd = body?.nym?.usd;
    if (!usd || !(usd > 0)) throw new Error("price feed returned no NYM/USD");
    this.price = { usdPerNym: usd, at: Date.now() };
    return usd;
  }

  // ---- PaymentGateway -------------------------------------------------------

  async createInvoice(
    amountUsd: number,
    _reference: string,
  ): Promise<Omit<Invoice, "id" | "amountScrai">> {
    await this.ensureConnected();
    const usdPerNym = await this.usdPerNym();

    // Quote the exact NYM the user must send, to 6 dp (unym granularity). We fix
    // it here and require AT LEAST this much — display rounding can only ever make
    // them send a hair more, never less.
    const nym = amountUsd / usdPerNym;
    const expectedUnym = BigInt(Math.ceil(nym * MICRO));
    const nymAmount = (Number(expectedUnym) / MICRO).toFixed(6);

    const memo = newMemo();
    const expiresAt = Date.now() + this.ttl();
    this.pending.set(memo, { memo, expectedUnym, nymAmount, status: "pending", expiresAt });

    return {
      providerRef: memo, // the memo IS our reference — checkStatus reads it back
      payTo: this.receiveAddress,
      instruction:
        `Send exactly ${nymAmount} NYM to the address below AND include the memo. ` +
        `The payment cannot be credited without the memo.`,
      options: [
        {
          method: "NYM",
          destination: this.receiveAddress,
          uri: this.receiveAddress, // Nyx has no universal payment URI — the QR is the bare address
          amount: nymAmount,
          currency: "NYM",
          memo,
        },
      ],
      amountUsd,
      expiresAt,
    };
  }

  /** Stop watching a cancelled invoice — drop its memo so the socket/poll ignores
   *  any late transfer for it (an unpaid, user-abandoned invoice). */
  cancel(providerRef: string): void {
    this.pending.delete(providerRef);
  }

  async checkStatus(providerRef: string): Promise<InvoiceStatus> {
    const inv = this.pending.get(providerRef);
    if (!inv) return "expired"; // unknown / evicted
    if (inv.status === "paid") return "paid";
    if (Date.now() > inv.expiresAt) {
      inv.status = "expired";
      return "expired";
    }
    return "pending";
  }

  /**
   * Is the chain watcher actually live? "connected" means the block heartbeat
   * arrived recently — proof that a payment WOULD reach us, not just that a socket
   * object exists. Surfaced to the client so the pay screen can show it.
   */
  watchState(): WatchState {
    const ageMs = this.lastBlock ? Date.now() - this.lastBlock.at : null;
    const staleAfter = this.mode === "polling" ? 20_000 : 30_000;
    const healthy = this.connected && ageMs != null && ageMs < staleAfter;
    return {
      connected: healthy,
      height: this.lastBlock?.height ?? null,
      lastBlockAgoMs: ageMs,
      mode: this.mode,
    };
  }
}

// SCRAI-XXXXXXXX — short, unique, human-copyable. Uppercase + digits only so it
// survives a wallet's memo field without ambiguity.
function newMemo(): string {
  const b = randomBytes(6);
  const abc = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I
  let s = "";
  for (const byte of b) s += abc[byte % abc.length];
  return `SCRAI-${s}`;
}

// "1000000unym" or "100ibc/ABC…,1000000unym" -> unym only, as bigint.
function parseUnym(amount: string): bigint {
  let total = 0n;
  for (const part of amount.split(",")) {
    const m = /^(\d+)unym$/.exec(part.trim());
    if (m) total += BigInt(m[1]);
  }
  return total;
}

function toHex(b: Uint8Array): string {
  return Buffer.from(b).toString("hex").toUpperCase();
}
