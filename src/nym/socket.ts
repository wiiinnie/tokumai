// ---------------------------------------------------------------------------
// socket.ts — typed client for the nym-client websocket API.
//
// A running `nym-client` exposes a websocket on 127.0.0.1:1977. Everything we
// send into the mixnet goes through it. The protocol is small; the shapes below
// mirror clients/native/websocket-requests/src/text.rs exactly (serde tags the
// enum with `type` and renames everything camelCase).
//
// The one choice here that matters for privacy: we send with `sendAnonymous`,
// never `send`. `send` puts our own Nym address in the message so the recipient
// can reply to it — which tells the server exactly who is asking.
// `sendAnonymous` attaches single-use reply blocks instead, so the server can
// answer a sender it cannot identify. That is the whole product.
//
// v2 SEAM: when the Rust core lands, this file is what it replaces. Nothing
// above it knows a websocket exists.
// ---------------------------------------------------------------------------

export const DEFAULT_WS_PORT = 1977;

type ServerResponse =
  | { type: "received"; message: string; senderTag?: string | null }
  | { type: "selfAddress"; address: string }
  | { type: "laneQueueLength"; lane: number; queueLength: number }
  | { type: "error"; message: string };

export interface InboundMessage {
  message: string;
  /** Present when the sender used sendAnonymous — this is how we reply to them. */
  senderTag?: string;
}

export class NymSocket {
  private ws: WebSocket;
  private addressWaiters: Array<(a: string) => void> = [];
  private messageHandlers: Array<(m: InboundMessage) => void> = [];
  private errorHandlers: Array<(e: string) => void> = [];

  private constructor(ws: WebSocket) {
    this.ws = ws;
    ws.onmessage = (ev) => this.dispatch(String(ev.data));
  }

  /** Connect to an already-running nym-client. */
  static connect(port = DEFAULT_WS_PORT, host = "127.0.0.1", timeoutMs = 10_000): Promise<NymSocket> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(`ws://${host}:${port}`);
      const timer = setTimeout(() => {
        try { ws.close(); } catch { /* already gone */ }
        reject(new Error(`nym-client websocket did not accept on ${host}:${port} within ${timeoutMs}ms`));
      }, timeoutMs);

      ws.onopen = () => {
        clearTimeout(timer);
        resolve(new NymSocket(ws));
      };
      ws.onerror = () => {
        clearTimeout(timer);
        reject(new Error(`cannot reach nym-client websocket on ${host}:${port} — is it running?`));
      };
    });
  }

  private dispatch(raw: string): void {
    let res: ServerResponse;
    try {
      res = JSON.parse(raw);
    } catch {
      return; // not our protocol; ignore rather than crash the loop
    }

    switch (res.type) {
      case "selfAddress": {
        const w = this.addressWaiters.shift();
        if (w) w(res.address);
        break;
      }
      case "received": {
        const msg: InboundMessage = {
          message: res.message,
          ...(res.senderTag ? { senderTag: res.senderTag } : {}),
        };
        for (const h of this.messageHandlers) h(msg);
        break;
      }
      case "error": {
        for (const h of this.errorHandlers) h(res.message);
        break;
      }
      case "laneQueueLength":
        break; // backpressure signal; unused in v1
    }
  }

  /** Our own Nym address. For the server this is what clients send to. */
  selfAddress(timeoutMs = 5_000): Promise<string> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("timed out asking nym-client for self address")), timeoutMs);
      this.addressWaiters.push((a) => {
        clearTimeout(timer);
        resolve(a);
      });
      this.ws.send(JSON.stringify({ type: "selfAddress" }));
    });
  }

  /**
   * Send without revealing our address. `replySurbs` is the reply budget: the
   * server can only answer with as many Sphinx packets as we gave it SURBs for,
   * so a long answer with a stingy budget arrives truncated or not at all.
   */
  sendAnonymous(recipient: string, message: string, replySurbs: number): void {
    this.ws.send(JSON.stringify({ type: "sendAnonymous", recipient, message, replySurbs }));
  }

  /** Answer someone who sent anonymously, using the tag they arrived with. */
  reply(senderTag: string, message: string): void {
    this.ws.send(JSON.stringify({ type: "reply", senderTag, message }));
  }

  onMessage(fn: (m: InboundMessage) => void): void {
    this.messageHandlers.push(fn);
  }

  onError(fn: (e: string) => void): void {
    this.errorHandlers.push(fn);
  }

  /** Resolve on the next inbound message, or reject on timeout. */
  nextMessage(timeoutMs: number): Promise<InboundMessage> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`no reply from the mixnet within ${Math.round(timeoutMs / 1000)}s`)),
        timeoutMs,
      );
      const once = (m: InboundMessage) => {
        clearTimeout(timer);
        this.messageHandlers = this.messageHandlers.filter((h) => h !== once);
        resolve(m);
      };
      this.messageHandlers.push(once);
    });
  }

  close(): void {
    try { this.ws.close(); } catch { /* already closed */ }
  }
}
