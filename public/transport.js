// ---------------------------------------------------------------------------
// transport.js — how a chat request leaves the device.
//
// The UI and chat logic call `transport.chat(request, handlers)` and never learn
// whether the bytes went out directly or through the Nym mixnet. Swapping the
// path is swapping the transport — the one seam that lets the SAME frontend ship
// to browser, Tauri desktop, and mobile unchanged.
//
// Every HTTP-based transport shares the SSE reader below; they differ only in
// WHERE they send. That difference is the whole abstraction.
// ---------------------------------------------------------------------------

async function streamSSE(url, ticket, body, { onDelta, onDone, onError }) {
  let res;
  try {
    res = await fetch(url, {
      method: "POST",
      headers: { authorization: "Bearer " + ticket, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (_) {
    onError("transport unreachable — is the endpoint up?");
    return;
  }
  if (!res.ok || !res.body) {
    let t = "";
    try { t = await res.text(); } catch (_) {}
    onError("transport error " + res.status + (t ? ": " + t : ""));
    return;
  }
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, nl).trim();
      buf = buf.slice(nl + 1);
      if (!line.startsWith("data:")) continue;
      const json = line.slice(5).trim();
      if (!json) continue;
      let evt;
      try { evt = JSON.parse(json); } catch (_) { buf = line + "\n" + buf; break; }
      if (evt.error) { onError(evt.error); return; }
      if (evt.delta) onDelta(evt.delta);
      if (evt.done) { onDone(evt.usage); return; }
    }
  }
}

// Direct: straight HTTP(S) to the AI server.
//   dev: baseUrl "" = same-origin (the local server). prod: the remote server URL.
export class DirectTransport {
  constructor({ baseUrl = "", ticket }) {
    this.baseUrl = baseUrl;
    this.ticket = ticket;
    this.label = "Direct";
  }
  chat(body, handlers) {
    return streamSSE(this.baseUrl + "/chat", this.ticket, body, handlers);
  }
}

// Mixnet: the SAME request, routed through the Nym mixnet.
//
// The SOCKS5 bridge this used to call is gone. Mixnet routing now lives in the
// CLI (src/cli + src/nym), which runs a native nym-client and addresses the
// server by its Nym address — no exit gateway, no public HTTPS endpoint.
//
// This class is the placeholder for bringing that to the GUI:
//   Tauri desktop -> chat() calls into the Rust backend over IPC, which routes
//                    through the embedded nym-sdk client.
//   mobile        -> same core via uniffi.
// The interface never changes across these — only what sits behind chat().
export class MixnetTransport {
  constructor({ ticket }) {
    this.ticket = ticket;
    this.label = "Mixnet";
  }
  chat(_body, handlers) {
    handlers.onError(
      "mixnet routing is CLI-only right now — use `npm run client -- repl`. " +
        "The GUI gets it when the Tauri Rust backend lands.",
    );
  }
}

export function createTransport(mode, cfg) {
  return mode === "mixnet" ? new MixnetTransport(cfg) : new DirectTransport(cfg);
}
