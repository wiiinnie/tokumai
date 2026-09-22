// ---------------------------------------------------------------------------
// backend.js — the interface's ONE connection to the app's Rust side.
//
// Every call is a Tauri command (src-tauri/src/lib.rs). There is no browser backend: the
// app talks to the enclave only through tokumai_client (attested, over the mixnet), and a
// page in a browser has no business holding a recovery phrase.
//
// What is NOT here any more, on purpose: coins, invoices, vouchers, the faucet, coin
// returns, the purchase client, chunked uploads. The balance is on the account, inside the
// enclave; attachments travel inside the chat request (the mixnet framing splits it).
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

function invoke(cmd, args) {
  const inv = tauriInvoke();
  if (!inv) return Promise.reject(new Error("tokumai runs as an app — open it from the app, not in a browser"));
  return inv(cmd, args);
}

function listen(name, cb) {
  const ev = window.__TAURI__ && window.__TAURI__.event;
  if (ev && ev.listen) return ev.listen(name, (e) => { try { cb(e.payload); } catch (_) {} });
  return Promise.resolve(() => {});
}

export const Backend = {
  // ---- state and account
  state: () => invoke("state"),
  localState: () => invoke("local_state"),
  accountNew: (force) => invoke("account_new", { force: !!force }),
  accountReveal: () => invoke("account_reveal"),
  accountRestore: (mnemonic) => invoke("account_restore", { mnemonic }),
  accountDelete: () => invoke("account_delete"),
  accountMigrateQr: () => invoke("account_migrate_qr"),
  phraseCheckStart: () => invoke("phrase_check_start"),
  phraseCheckVerify: (positions, words) => invoke("phrase_check_verify", { positions, words }),
  phraseBackupGet: () => invoke("phrase_backup_get"),

  // ---- plans (card, via Stripe; the App Store comes with the iPhone app)
  planLadder: () => invoke("plan_ladder"),
  // consent = { version, immediateStart, waiverAck } — the enclave refuses without it.
  planCheckout: (tier, yearly, consent) => invoke("plan_checkout", { tier, yearly: !!yearly, consent }),
  planPoll: () => invoke("plan_poll"),
  planChange: (tier, yearly) => invoke("plan_change", { tier, yearly: !!yearly }),
  planForget: () => invoke("plan_forget"),

  // ---- the route (entry gateway: never one of tokumai's — rule A1)
  mixnetRoute: () => invoke("mixnet_route"),
  listEntryGateways: () => invoke("list_entry_gateways"),
  setEntryGateway: (id) => invoke("set_entry_gateway", { id }),
  setEntryRandom: (on) => invoke("set_entry_random", { on: !!on }),
  // Rust emits mixnet-phase {step, detail} during every connect:
  // directory · keys · gateway · cover · ready · failed.
  onMixnetPhase: (cb) => listen("mixnet-phase", cb),
  // Settings → Network & privacy: the speed/anonymity trade-off (Nym's defaults otherwise).
  setMixnetPerf: (coverMs, mixMs, sendMs, continuous) => invoke("set_mixnet_perf", { coverMs, mixMs, sendMs, continuous: !!continuous }),
  mixnetPing: () => invoke("mixnet_ping"),
  // The local resume log of the first app; the desktop keeps none.
  resumeStats: () => Promise.resolve({ count: 0, alive: 0, dead: 0, rebuilt: 0, longest_alive_ms: 0, shortest_dead_ms: null, log: "", path: "" }),
  appHidden: () => invoke("app_hidden"),
  appResumed: (hiddenMs, force) => invoke("app_resumed", { hiddenMs: Math.max(0, Math.round(hiddenMs || 0)), force: !!force }),
  mixnetHeartbeat: () => invoke("mixnet_heartbeat"),

  // ---- support (the messages themselves come with a later version)
  supportSend: () => invoke("support_send"),
  supportList: () => invoke("support_list"),
  supportDiag: (lastError) => invoke("support_diag", { lastError: lastError || null }),
  supportFetch: () => Promise.resolve({ threads: [] }),
  supportSeen: () => Promise.resolve(),

  // ---- the chat vault (files in Rust, key in the OS keychain)
  vaultList: () => invoke("vault_list"),
  vaultLoad: (id) => invoke("vault_load", { id }),
  vaultSave: (session, updated) => invoke("vault_save", { session, updated: (typeof updated === "number" ? updated : null) }),
  vaultRemove: (id) => invoke("vault_remove", { id }),
  vaultPurgeWebdata: () => invoke("vault_purge_webdata"),

  // ---- files and links
  saveImage: (dataB64, filename) => invoke("save_image", { data: dataB64, filename }),
  saveFile: (dataB64, filename) => invoke("save_file", { data: dataB64, filename }),
  openExternal: (url) => invoke("open_external", { url }),
  // The phone apps' native picker and share sheet come with the phone apps.
  pickImage: () => Promise.reject(new Error("the native picker is part of the phone app")),
  shareText: () => Promise.reject(new Error("the share sheet is part of the phone app")),

  // ---- the privacy guard's readers, on the device
  ocr: (image) => invoke("ocr_scan", { image }),
  pdfText: (image) => invoke("pdf_text", { image }),
  pdfOcr: (image) => invoke("pdf_ocr", { image }),
  pdfPages: (image) => invoke("pdf_pages", { image }),
  smartAvailable: () => invoke("smart_available"),
  smartDetect: (texts, labels) => invoke("smart_detect", { texts, labels }),

  // ---- chat
  // One answer per question, whole: the reply is fed out in slices so it reads like a
  // stream. onPhase("sent") fires when the question leaves; the answer arriving resolves
  // the call. The reply: { text, images, cost, balance, usage, estimated }.
  cancelChat: () => invoke("cancel_chat"),
  chat: async (body, { onDelta, onDone, onError, onPhase }) => {
    const unlisten = [];
    try {
      if (onPhase) unlisten.push(await listen("chat-sent", () => onPhase("sent")));
      const r = await invoke("chat", {
        model: body.model, messages: body.messages, maxTokens: body.maxTokens, live: !!body.live,
        thinkingBudget: (typeof body.thinkingBudget === "number" ? body.thinkingBudget : null),
        imageSize: (typeof body.imageSize === "string" ? body.imageSize : null),
      });
      if (onPhase) onPhase("receiving");
      if (r && r.text) {
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
};
