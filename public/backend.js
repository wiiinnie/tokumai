// ---------------------------------------------------------------------------
// backend.js — the UI's ONE connection to its backend.
//
// The seam that lets the same frontend run in the browser (dev) and the native
// app (Tauri desktop, mobile). In Tauri it calls Rust commands via `invoke`; in
// the browser it uses HTTP to the local dev backend. Detection is done PER CALL
// (not once at import) so it is robust to when Tauri injects window.__TAURI__.
// ---------------------------------------------------------------------------

/** The Tauri invoke fn if we're running inside the app, else null. */
export function tauriInvoke() {
  if (typeof window === "undefined") return null;
  const t = window.__TAURI__;
  if (t && (t.core?.invoke || t.invoke)) return t.core?.invoke || t.invoke;
  const i = window.__TAURI_INTERNALS__;
  return i?.invoke ?? null;
}

export const isTauri = () => tauriInvoke() != null;

// ---- HTTP (browser dev) ---------------------------------------------------

async function jsonCall(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: body ? { "content-type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

async function streamChat(body, { onDelta, onDone, onError }) {
  let res;
  try {
    res = await fetch("/chat", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (_) {
    onError("backend unreachable — is the dev server up?");
    return;
  }
  if (!res.ok || !res.body) {
    let msg = "";
    try { msg = (await res.json()).error; } catch (_) {}
    onError(msg || "error " + res.status);
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
      if (evt.done) { onDone(evt); return; }
    }
  }
}

// ---- the two backends -----------------------------------------------------

const httpBackend = {
  state: () => jsonCall("GET", "/api/state"),
  setServer: () => Promise.resolve({}), // dev backend is in-process; no server address
  accountNew: () => jsonCall("POST", "/api/account/new"),
  accountReveal: () => jsonCall("GET", "/api/account/reveal"),
  accountRestore: (mnemonic) => jsonCall("POST", "/api/account/restore", { mnemonic }),
  credit: (usd) => jsonCall("POST", "/api/credit", { usd }),
  invoice: (usd) => jsonCall("POST", "/api/invoice", { usd }),
  invoiceStatus: (id) => jsonCall("GET", "/api/invoice/" + encodeURIComponent(id)),
  collect: () => jsonCall("POST", "/api/collect"),
  redeem: () => jsonCall("POST", "/api/redeem"),
  chat: (body, handlers) => streamChat(body, handlers),
  // No mixnet in the dev web backend — report an empty route.
  mixnetRoute: () => Promise.resolve({ entry: null, exit: null, chosen: null }),
  listEntryGateways: () => Promise.resolve([]),
  setEntryGateway: () => Promise.resolve({ entry_gateway: null }),
  openExternal: (url) => { window.open(url, "_blank", "noopener"); return Promise.resolve(); },
  // Dev has no mixnet: images ride inline in the chat instead of chunk-uploading.
  uploadBegin: () => Promise.reject(new Error("no chunked upload in dev")),
  uploadChunk: () => Promise.reject(new Error("no chunked upload in dev")),
  // Browser dev: fall back to an anchor download (no native dialog available).
  saveImage: (dataB64, filename, mimeType) => {
    const a = document.createElement("a");
    a.href = `data:${mimeType || "image/jpeg"};base64,${dataB64}`;
    a.download = filename;
    document.body.appendChild(a); a.click(); a.remove();
    return Promise.resolve(filename);
  },
};

const tauriBackend = (invoke) => ({
  state: () => invoke("state"),
  setServer: (address) => invoke("set_server", { address }),
  accountNew: () => invoke("account_new"),
  accountReveal: () => invoke("account_reveal"),
  accountRestore: (mnemonic) => invoke("account_restore", { mnemonic }),
  credit: () => Promise.reject(new Error("this build uses real payments — pick an amount to raise an invoice")),
  invoice: (usd) => invoke("invoice", { usd }),
  invoiceStatus: (id) => invoke("invoice_status", { id }),
  collect: () => invoke("collect"),
  redeem: () => invoke("redeem"),
  mixnetRoute: () => invoke("mixnet_route"),
  listEntryGateways: () => invoke("list_entry_gateways"),
  setEntryGateway: (id) => invoke("set_entry_gateway", { id }),
  openExternal: (url) => invoke("open_external", { url }),
  saveImage: (dataB64, filename) => invoke("save_image", { data: dataB64, filename }),
  uploadBegin: (mimeType, totalBytes) => invoke("upload_begin", { mimeType, totalBytes }),
  uploadChunk: (uploadId, seq, data) => invoke("upload_chunk", { uploadId, seq, data }),
  // Non-streaming over the mixnet: one reply carrying the whole answer.
  chat: async (body, { onDelta, onDone, onError }) => {
    try {
      const r = await invoke("chat", { model: body.model, messages: body.messages, maxTokens: body.maxTokens });
      if (r && r.text) onDelta(r.text);
      onDone(r || {});
    } catch (e) {
      onError(e && e.message ? e.message : String(e));
    }
  },
});

// Resolve per call so late __TAURI__ injection is never missed.
function pick(method, ...args) {
  const invoke = tauriInvoke();
  const b = invoke ? tauriBackend(invoke) : httpBackend;
  return b[method](...args);
}

export const Backend = {
  state: () => pick("state"),
  setServer: (address) => pick("setServer", address),
  accountNew: () => pick("accountNew"),
  accountReveal: () => pick("accountReveal"),
  accountRestore: (m) => pick("accountRestore", m),
  credit: (usd) => pick("credit", usd),
  invoice: (usd) => pick("invoice", usd),
  invoiceStatus: (id) => pick("invoiceStatus", id),
  collect: () => pick("collect"),
  redeem: () => pick("redeem"),
  mixnetRoute: () => pick("mixnetRoute"),
  listEntryGateways: () => pick("listEntryGateways"),
  setEntryGateway: (id) => pick("setEntryGateway", id),
  openExternal: (url) => pick("openExternal", url),
  saveImage: (dataB64, filename, mimeType) => pick("saveImage", dataB64, filename, mimeType),
  uploadBegin: (mimeType, totalBytes) => pick("uploadBegin", mimeType, totalBytes),
  uploadChunk: (uploadId, seq, data) => pick("uploadChunk", uploadId, seq, data),
  chat: (body, handlers) => pick("chat", body, handlers),
};
