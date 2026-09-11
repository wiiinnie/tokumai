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
  localState: () => jsonCall("GET", "/api/state"),
  setServer: () => Promise.resolve({}), // dev backend is in-process; no server address
  accountNew: (force) => jsonCall("POST", "/api/account/new", { force }),
  accountReveal: () => jsonCall("GET", "/api/account/reveal"),
  accountRestore: (mnemonic, force) => jsonCall("POST", "/api/account/restore", { mnemonic, force }),
  accountDelete: (force) => jsonCall("POST", "/api/account/delete", { force }),
  accountMigrateQr: () => jsonCall("GET", "/api/account/migrate-qr"),
  credit: (usd) => jsonCall("POST", "/api/credit", { usd }),
  pickImage: () => Promise.reject(new Error("native picker is iOS-only")),
  invoice: (usd, method, testnet, inviteCode, consent) => jsonCall("POST", "/api/invoice", { usd, method, testnet: !!testnet, inviteCode: inviteCode || "", consent: consent || null }),
  // The dev bridge has no faucet ledger to ask — the invite tile simply stays closed there.
  inviteCheck: () => Promise.resolve({ valid: false, usd: 1 }),
  invoiceStatus: (id) => jsonCall("GET", "/api/invoice/" + encodeURIComponent(id)),
  invoiceCancel: (id) => jsonCall("POST", "/api/invoice/cancel", { id }),
  voucherRedeem: () => Promise.reject(new Error("codes are redeemed in the app, not the dev bridge")),
  phraseCheckStart: () => Promise.resolve({ positions: [4, 11, 19], total: 24, verified: true }),
  phraseCheckVerify: () => Promise.resolve({ ok: true }),
  phraseBackupGet: () => Promise.resolve({ available: false, on: false }),
  phraseBackupSet: () => Promise.reject(new Error("the phrase backup is an app feature")),
  iapProducts: () => Promise.reject(new Error("App Store purchases exist only in the iPhone app")),
  iapPurchase: () => Promise.reject(new Error("App Store purchases exist only in the iPhone app")),
  iapRestore: () => Promise.resolve({ found: 0, claimed: 0, toku: 0, pending: 0 }),
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
  mixnetPing: () => Promise.reject(new Error("mixnet ping is native-only")),
  cancelChat: () => Promise.resolve(),
  appResumed: () => Promise.resolve({ action: "alive", ms: 0 }),
  appHidden: () => Promise.resolve(),
  resumeStats: () => Promise.resolve({ count: 0, alive: 0, dead: 0, rebuilt: 0, longest_alive_ms: 0, shortest_dead_ms: null, log: "", path: "" }),
  onMixnetPhase: async () => () => {},
  listEntryGateways: () => Promise.resolve([]),
  serverIdentities: () => Promise.resolve([]),
  setEntryGateway: () => Promise.resolve({ entry_gateway: null }),
  setEntryRandom: (on) => Promise.resolve({ entry_gateway: null, entry_random: !!on }),
  setMixnetPerf: () => Promise.resolve({}), // dev backend has no mixnet
  // nosemgrep: scrai-js-window-open -- dev bridge only; the app routes through open_external
  openExternal: (url) => { window.open(url, "_blank", "noopener"); return Promise.resolve(); },
  // Browser dev keeps the IndexedDB vault (vault.js picks it when !isTauri) — never called.
  vaultList: () => Promise.reject(new Error("native vault is Tauri-only")),
  vaultLoad: () => Promise.reject(new Error("native vault is Tauri-only")),
  vaultSave: () => Promise.reject(new Error("native vault is Tauri-only")),
  vaultRemove: () => Promise.reject(new Error("native vault is Tauri-only")),
  vaultPurgeWebdata: () => Promise.resolve(),
  // Dev pending store: plain localStorage is fine — dev payments are fake.
  pendingLoad: () => { try { return Promise.resolve(JSON.parse(localStorage.getItem("scrai.pending") || "[]")); } catch (_) { return Promise.resolve([]); } },
  pendingSave: (list) => { try { localStorage.setItem("scrai.pending", JSON.stringify(list)); } catch (_) {} return Promise.resolve(); },
  // Dev has no mixnet: images ride inline in the chat instead of chunk-uploading.
  uploadBegin: () => Promise.reject(new Error("no chunked upload in dev")),
  uploadChunk: () => Promise.reject(new Error("no chunked upload in dev")),
  uploadPipeline: () => Promise.reject(new Error("no chunked upload in dev")),
  // Browser dev: fall back to an anchor download (no native dialog available).
  saveImage: (dataB64, filename, mimeType) => {
    const a = document.createElement("a");
    a.href = `data:${mimeType || "image/jpeg"};base64,${dataB64}`;
    a.download = filename;
    document.body.appendChild(a); a.click(); a.remove();
    return Promise.resolve(filename);
  },
  // Browser dev: the receipt is an anchor download, like the images above.
  saveFile: (dataB64, filename) => {
    const a = document.createElement("a");
    a.href = `data:application/pdf;base64,${dataB64}`;
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
  localState: () => invoke("local_state"),
  setServer: (address) => invoke("set_server", { address }),
  accountNew: (force) => invoke("account_new", { force: !!force }),
  accountReveal: () => invoke("account_reveal"),
  accountRestore: (mnemonic, force) => invoke("account_restore", { mnemonic, force: !!force }),
  accountDelete: (force) => invoke("account_delete", { force: !!force }),
  accountMigrateQr: () => invoke("account_migrate_qr"),
  credit: () => Promise.reject(new Error("this build uses real payments — pick an amount to raise an invoice")),
  pickImage: (source) => invoke("pick_image", { source }),
  invoice: (usd, method, testnet, inviteCode, consent) => invoke("invoice", { usd, method, testnet: !!testnet, inviteCode: inviteCode || "", consent: consent || null }),
  inviteCheck: (code) => invoke("invite_check", { code }),
  invoiceStatus: (id) => invoke("invoice_status", { id }),
  invoiceCancel: (id) => invoke("invoice_cancel", { id }),
  voucherRedeem: (code) => invoke("voucher_redeem", { code }),
  phraseCheckStart: () => invoke("phrase_check_start"),
  phraseCheckVerify: (positions, words) => invoke("phrase_check_verify", { positions, words }),
  phraseBackupGet: () => invoke("phrase_backup_get"),
  phraseBackupSet: (on) => invoke("phrase_backup_set", { on: !!on }),
  iapProducts: () => invoke("iap_products"),
  iapPurchase: (productId) => invoke("iap_purchase", { productId }),
  iapRestore: () => invoke("iap_restore"),
  ocr: (image) => invoke("ocr_scan", { image }),   // native OS OCR (macOS: Apple Vision); errors → WASM fallback
  pdfText: (image) => invoke("pdf_text", { image }), // native PDF text extraction (Rust); errors/empty → pdf.js fallback
  pdfOcr: (image) => invoke("pdf_ocr", { image }),
  pdfPages: (image) => invoke("pdf_pages", { image }),
  smartAvailable: () => invoke("smart_available"),        // engine built AND model installed?
  smartDetect: (texts, labels) => invoke("smart_detect", { texts, labels }),  // zero-shot NER batch
  collect: () => invoke("collect"),
  redeem: () => invoke("redeem"),
  mixnetRoute: () => invoke("mixnet_route"),
  mixnetPing: () => invoke("mixnet_ping"),
  // Stop waiting for the in-flight reply; the pending request stays replayable.
  cancelChat: () => invoke("cancel_chat"),
  // App back in the foreground after hiddenMs; force=true rebuilds the route unconditionally.
  appResumed: (hiddenMs, force) => invoke("app_resumed", { hiddenMs: Math.max(0, Math.round(hiddenMs||0)), force: !!force }),
  // Android: hidden long enough → drop the client so cover traffic stops burning battery.
  appHidden: () => invoke("app_hidden"),
  // Local resume log (hidden duration → route alive?), summary + tail. Never leaves the device.
  resumeStats: () => invoke("resume_stats"),
  // Rust emits mixnet-phase {step, detail} during every (re)connect — keys · client · gateway · cover · ready · failed · check.
  onMixnetPhase: async (cb) => { const ev = window.__TAURI__ && window.__TAURI__.event; if (ev && ev.listen) return ev.listen("mixnet-phase", (e) => { try { cb(e.payload); } catch (_) {} }); return () => {}; },
  listEntryGateways: () => invoke("list_entry_gateways"),
  serverIdentities: () => invoke("server_identities"),
  setEntryGateway: (id) => invoke("set_entry_gateway", { id }),
  setEntryRandom: (on) => invoke("set_entry_random", { on: !!on }),
  setMixnetPerf: (coverMs, mixMs, sendMs, continuous) => invoke("set_mixnet_perf", { coverMs, mixMs, sendMs, continuous }),
  openExternal: (url) => invoke("open_external", { url }),
  saveImage: (dataB64, filename) => invoke("save_image", { data: dataB64, filename }),
  saveFile: (dataB64, filename) => invoke("save_file", { data: dataB64, filename }),
  shareText: (filename, text) => invoke("share_text", { filename, text }),
  // Chat vault (Rust-side files, key in the OS keychain — see src-tauri/src/vault.rs).
  vaultList: () => invoke("vault_list"),
  vaultLoad: (id) => invoke("vault_load", { id }),
  vaultSave: (session, updated) => invoke("vault_save", { session, updated: (typeof updated === "number" ? updated : null) }),
  vaultRemove: (id) => invoke("vault_remove", { id }),
  vaultPurgeWebdata: () => invoke("vault_purge_webdata"),
  pendingLoad: () => invoke("pending_load"),
  pendingSave: (list) => invoke("pending_save", { list }),
  uploadBegin: (mimeType, totalBytes) => invoke("upload_begin", { mimeType, totalBytes }),
  uploadChunk: (uploadId, seq, data) => invoke("upload_chunk", { uploadId, seq, data }),
  uploadPipeline: async (uploadId, chunks, onProgress) => {
    const ev = window.__TAURI__ && window.__TAURI__.event;
    let unlisten = null;
    if (onProgress && ev && ev.listen) unlisten = await ev.listen("upload-progress", (e) => { try { onProgress(e.payload); } catch (_) {} });
    try { return await invoke("upload_pipeline", { uploadId, chunks }); }
    finally { if (unlisten) try { unlisten(); } catch (_) {} }
  },
  // Non-streaming over the mixnet: one reply carrying the whole answer. Phase
  // signals are REAL: Rust emits "chat-sent" the instant the request has fully
  // left for the mixnet, and the invoke resolving IS the reply arriving.
  chat: async (body, { onDelta, onDone, onError, onPhase }) => {
    const unlisten = [];
    try {
      const ev = window.__TAURI__ && window.__TAURI__.event;
      if (onPhase && ev && ev.listen) {
        unlisten.push(await ev.listen("chat-sent", () => onPhase("sent")));
        // Rust auto-redeems held credit when the session runs short mid-request.
        unlisten.push(await ev.listen("chat-redeeming", () => onPhase("redeeming")));
        // Big generated pictures are fetched chunk by chunk after the reply lands;
        // Rust emits {done, total} per chunk so the UI can show real download progress.
        unlisten.push(await ev.listen("image-progress", (e) => onPhase("image", e.payload)));
        // The route is down: this request waits for the rebuild before it leaves.
        unlisten.push(await ev.listen("chat-route", () => onPhase("route")));
      }
      const r = await invoke("chat", { model: body.model, messages: body.messages, maxTokens: body.maxTokens, live: !!body.live, thinkingBudget: (typeof body.thinkingBudget==="number"?body.thinkingBudget:null), bigReply: !!body.bigReply, retry: !!body.retry, imageSize: (typeof body.imageSize==="string"?body.imageSize:null) });
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
      unlisten.forEach((u) => { try { u(); } catch (_) {} });
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
  localState: () => pick("localState"),
  setServer: (address) => pick("setServer", address),
  accountNew: (force) => pick("accountNew", force),
  accountReveal: () => pick("accountReveal"),
  accountRestore: (m, force) => pick("accountRestore", m, force),
  accountDelete: (force) => pick("accountDelete", force),
  accountMigrateQr: () => pick("accountMigrateQr"),
  phraseCheckStart: () => pick("phraseCheckStart"),
  phraseCheckVerify: (positions, words) => pick("phraseCheckVerify", positions, words),
  phraseBackupGet: () => pick("phraseBackupGet"),
  phraseBackupSet: (on) => pick("phraseBackupSet", on),
  iapProducts: () => pick("iapProducts"),
  iapPurchase: (productId) => pick("iapPurchase", productId),
  iapRestore: () => pick("iapRestore"),
  pickImage: (source) => pick("pickImage", source),
  credit: (usd) => pick("credit", usd),
  invoice: (usd, method, testnet, inviteCode, consent) => pick("invoice", usd, method, testnet, inviteCode, consent),
  inviteCheck: (code) => pick("inviteCheck", code),
  invoiceStatus: (id) => pick("invoiceStatus", id),
  invoiceCancel: (id) => pick("invoiceCancel", id),
  voucherRedeem: (code) => pick("voucherRedeem", code),
  ocr: (image) => pick("ocr", image),
  pdfText: (image) => pick("pdfText", image),
  pdfOcr: (image) => pick("pdfOcr", image),
  pdfPages: (image) => pick("pdfPages", image),
  smartAvailable: () => pick("smartAvailable"),
  smartDetect: (texts, labels) => pick("smartDetect", texts, labels),
  collect: () => pick("collect"),
  redeem: () => pick("redeem"),
  mixnetRoute: () => pick("mixnetRoute"),
  mixnetPing: () => pick("mixnetPing"),
  cancelChat: () => pick("cancelChat"),
  appResumed: (...a) => pick("appResumed", ...a),
  appHidden: () => pick("appHidden"),
  resumeStats: () => pick("resumeStats"),
  onMixnetPhase: (...a) => pick("onMixnetPhase", ...a),
  listEntryGateways: () => pick("listEntryGateways"),
  serverIdentities: () => pick("serverIdentities"),
  setEntryGateway: (id) => pick("setEntryGateway", id),
  setEntryRandom: (on) => pick("setEntryRandom", on),
  setMixnetPerf: (coverMs, mixMs, sendMs, continuous) => pick("setMixnetPerf", coverMs, mixMs, sendMs, continuous),
  openExternal: (url) => pick("openExternal", url),
  vaultList: () => pick("vaultList"),
  vaultLoad: (id) => pick("vaultLoad", id),
  vaultSave: (session, updated) => pick("vaultSave", session, updated),
  vaultRemove: (id) => pick("vaultRemove", id),
  vaultPurgeWebdata: () => pick("vaultPurgeWebdata"),
  pendingLoad: () => pick("pendingLoad"),
  pendingSave: (list) => pick("pendingSave", list),
  saveImage: (dataB64, filename, mimeType) => pick("saveImage", dataB64, filename, mimeType),
  saveFile: (dataB64, filename) => pick("saveFile", dataB64, filename),
  shareText: (filename, text) => pick("shareText", filename, text),
  uploadBegin: (mimeType, totalBytes) => pick("uploadBegin", mimeType, totalBytes),
  uploadChunk: (uploadId, seq, data) => pick("uploadChunk", uploadId, seq, data),
  uploadPipeline: (uploadId, chunks, onProgress) => pick("uploadPipeline", uploadId, chunks, onProgress),
  chat: (body, handlers) => pick("chat", body, handlers),
};
