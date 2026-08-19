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

async function streamChat(body, { onDelta, onDone, onError, onPhase }) {
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
  // Response headers back = the request has certainly been sent in full.
  if (onPhase) onPhase("sent");
  let gotFirst = false;
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
      // First event from upstream = the answer has started to arrive.
      if (!gotFirst) { gotFirst = true; if (onPhase) onPhase("receiving"); }
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
  invoice: (usd, method) => jsonCall("POST", "/api/invoice", { usd, method }),
  invoiceStatus: (id) => jsonCall("GET", "/api/invoice/" + encodeURIComponent(id)),
  invoiceCancel: (id) => jsonCall("POST", "/api/invoice/cancel", { id }),
  ocr: () => Promise.reject(new Error("no native OCR in dev")),   // dev browser → frontend falls back to WASM
  pdfText: () => Promise.reject(new Error("no native PDF in dev")), // dev browser → frontend falls back to pdf.js
  pdfOcr: () => Promise.reject(new Error("no native PDF-OCR in dev")),
  pdfPages: () => Promise.reject(new Error("no native PDF render in dev")),
  smartAvailable: () => Promise.resolve(false),           // no native GLiNER engine in dev
  smartDetect: () => Promise.reject(new Error("no native semantic guard in dev")),
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
  // Browser dev: exports are plain anchor downloads too.
  shareText: (filename, text) => {
    const a = document.createElement("a");
    a.href = URL.createObjectURL(new Blob([text], { type: "application/octet-stream" }));
    a.download = filename;
    document.body.appendChild(a); a.click(); a.remove();
    URL.revokeObjectURL(a.href);
    return Promise.resolve();
  },
};

const tauriBackend = (invoke) => ({
  state: () => invoke("state"),
  setServer: (address) => invoke("set_server", { address }),
  accountNew: () => invoke("account_new"),
  accountReveal: () => invoke("account_reveal"),
  accountRestore: (mnemonic) => invoke("account_restore", { mnemonic }),
  credit: () => Promise.reject(new Error("this build uses real payments — pick an amount to raise an invoice")),
  invoice: (usd, method) => invoke("invoice", { usd, method }),
  invoiceStatus: (id) => invoke("invoice_status", { id }),
  invoiceCancel: (id) => invoke("invoice_cancel", { id }),
  ocr: (image) => invoke("ocr_scan", { image }),   // native OS OCR (macOS: Apple Vision); errors → WASM fallback
  pdfText: (image) => invoke("pdf_text", { image }), // native PDF text extraction (Rust); errors/empty → pdf.js fallback
  pdfOcr: (image) => invoke("pdf_ocr", { image }),
  pdfPages: (image) => invoke("pdf_pages", { image }),
  smartAvailable: () => invoke("smart_available"),        // engine built AND model installed?
  smartDetect: (texts, labels) => invoke("smart_detect", { texts, labels }),  // zero-shot NER batch
  collect: () => invoke("collect"),
  redeem: () => invoke("redeem"),
  mixnetRoute: () => invoke("mixnet_route"),
  listEntryGateways: () => invoke("list_entry_gateways"),
  setEntryGateway: (id) => invoke("set_entry_gateway", { id }),
  openExternal: (url) => invoke("open_external", { url }),
  saveImage: (dataB64, filename) => invoke("save_image", { data: dataB64, filename }),
  shareText: (filename, text) => invoke("share_text", { filename, text }),
  uploadBegin: (mimeType, totalBytes) => invoke("upload_begin", { mimeType, totalBytes }),
  uploadChunk: (uploadId, seq, data) => invoke("upload_chunk", { uploadId, seq, data }),
  // Non-streaming over the mixnet: one reply carrying the whole answer. Phase
  // signals are REAL: Rust emits "chat-sent" the instant the request has fully
  // left for the mixnet, and the invoke resolving IS the reply arriving.
  chat: async (body, { onDelta, onDone, onError, onPhase }) => {
    let unlisten = null;
    try {
      const ev = window.__TAURI__ && window.__TAURI__.event;
      if (onPhase && ev && ev.listen) unlisten = await ev.listen("chat-sent", () => onPhase("sent"));
      const r = await invoke("chat", { model: body.model, messages: body.messages, maxTokens: body.maxTokens, free: !!body.free, bigReply: !!body.bigReply });
      if (onPhase) onPhase("receiving");
      if (r && r.text) {
        // The whole answer arrived in one mixnet reply — feed it out in slices so
        // it reads as an incoming stream instead of appearing as a wall of text.
        const text = r.text;
        const step = Math.max(24, Math.ceil(text.length / 40));
        for (let i = 0; i < text.length; i += step) {
          onDelta(text.slice(i, i + step));
          await new Promise((res) => setTimeout(res, 12));
        }
      }
      onDone(r || {});
    } catch (e) {
      onError(e && e.message ? e.message : String(e));
    } finally {
      if (unlisten) try { unlisten(); } catch (_) {}
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
  invoice: (usd, method) => pick("invoice", usd, method),
  invoiceStatus: (id) => pick("invoiceStatus", id),
  invoiceCancel: (id) => pick("invoiceCancel", id),
  ocr: (image) => pick("ocr", image),
  pdfText: (image) => pick("pdfText", image),
  pdfOcr: (image) => pick("pdfOcr", image),
  pdfPages: (image) => pick("pdfPages", image),
  smartAvailable: () => pick("smartAvailable"),
  smartDetect: (texts, labels) => pick("smartDetect", texts, labels),
  collect: () => pick("collect"),
  redeem: () => pick("redeem"),
  mixnetRoute: () => pick("mixnetRoute"),
  listEntryGateways: () => pick("listEntryGateways"),
  setEntryGateway: (id) => pick("setEntryGateway", id),
  openExternal: (url) => pick("openExternal", url),
  saveImage: (dataB64, filename, mimeType) => pick("saveImage", dataB64, filename, mimeType),
  shareText: (filename, text) => pick("shareText", filename, text),
  uploadBegin: (mimeType, totalBytes) => pick("uploadBegin", mimeType, totalBytes),
  uploadChunk: (uploadId, seq, data) => pick("uploadChunk", uploadId, seq, data),
  chat: (body, handlers) => pick("chat", body, handlers),
};
