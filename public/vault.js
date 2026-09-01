// ---------------------------------------------------------------------------
// vault.js — local persistence, behind a thin swappable interface.
//
//   Vault            — the on-device chat store. The UI only ever calls
//                      Vault.{init,list,load,save,remove}; it never sees storage,
//                      crypto, or a platform.
//
//     • In the app (Tauri) the store is NATIVE: sessions are files managed by Rust
//       (src-tauri/src/vault.rs), AES-256-GCM under a key that lives only in the OS
//       keychain. The webview receives plaintext sessions over IPC — the key never
//       enters JS, on disk or in memory.
//     • In the browser dev mode (no Tauri) the legacy IndexedDB store below is used:
//       AES-GCM with the key in the same IndexedDB. That is NOT protection at rest
//       (key next to ciphertext — a tester proved it on Windows, 2026-08-30), which is
//       why the app no longer uses it. It stays for dev and as the MIGRATION SOURCE:
//       on the first start of 0.4.2 the app moves every chat from IndexedDB into the
//       native vault, then deletes the IndexedDB and asks the webview to purge its
//       stored site data (so the old key does not linger in WebView2's LevelDB logs).
//
//   PassphraseCrypto — encrypt/decrypt a handover.md for archiving or moving
//                      between devices, unlocked by a passphrase (PBKDF2 →
//                      AES-GCM). Openable anywhere the passphrase is known.
// ---------------------------------------------------------------------------

import { Backend, isTauri } from "./backend.js";

const DB_NAME = "scrambleai";
const DB_VERSION = 1;
const STORE = "sessions";
const KEYS = "keys";
const DEVICE_KEY_ID = "device-aes-key";

// ---- IndexedDB helpers ----------------------------------------------------

function openDB() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE, { keyPath: "id" });
      if (!db.objectStoreNames.contains(KEYS)) db.createObjectStore(KEYS);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function tx(db, store, mode, fn) {
  return new Promise((resolve, reject) => {
    const t = db.transaction(store, mode);
    const s = t.objectStore(store);
    const out = fn(s);
    t.oncomplete = () => resolve(out && out.result !== undefined ? out.result : out);
    t.onerror = () => reject(t.error);
    t.onabort = () => reject(t.error);
  });
}

// ---- legacy KeyStore (browser dev only) ------------------------------------
// A random 32-byte secret in the `keys` store, imported as an AES-GCM key each
// session. The key bytes are readable from the DB — fine for dev, not for the app.

async function readDeviceKey(db) {
  const raw = await tx(db, KEYS, "readonly", (s) => s.get(DEVICE_KEY_ID));
  let bytes = null;
  if (raw instanceof Uint8Array) bytes = raw;
  else if (raw instanceof ArrayBuffer) bytes = new Uint8Array(raw);
  return bytes && bytes.length === 32 ? bytes : null;
}

async function importKey(bytes) {
  return crypto.subtle.importKey("raw", bytes, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

async function getDeviceKey(db) {
  let bytes = await readDeviceKey(db);
  // regenerate if missing or if a stale/incompatible record is found
  if (!bytes) {
    bytes = crypto.getRandomValues(new Uint8Array(32));
    await tx(db, KEYS, "readwrite", (s) => s.put(bytes, DEVICE_KEY_ID));
  }
  return importKey(bytes);
}

// ---- symmetric encrypt/decrypt with a CryptoKey ---------------------------

async function aesEncrypt(key, plaintext) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = new TextEncoder().encode(plaintext);
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, data);
  return { iv: Array.from(iv), ct: Array.from(new Uint8Array(ct)) };
}

async function aesDecrypt(key, blob) {
  const iv = new Uint8Array(blob.iv);
  const ct = new Uint8Array(blob.ct);
  const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, key, ct);
  return new TextDecoder().decode(pt);
}

// ---- LegacyVault: IndexedDB (browser dev; migration source in the app) ----

let _db = null;
let _key = null;

const LegacyVault = {
  async init() {
    _db = await openDB();
    _key = await getDeviceKey(_db);
  },

  /** metadata for the sidebar (decrypts each record; fine for a personal store) */
  async list() {
    const records = await tx(_db, STORE, "readonly", (s) => s.getAll());
    const out = [];
    for (const rec of await records) {
      try {
        const session = JSON.parse(await aesDecrypt(_key, rec.blob));
        out.push({
          id: session.id,
          title: session.title || "untitled",
          model: session.model,
          updated: rec.updated,
          count: session.messages.length,
        });
      } catch {
        /* skip unreadable record */
      }
    }
    return out.sort((a, b) => (b.updated || 0) - (a.updated || 0));
  },

  async load(id) {
    const rec = await tx(_db, STORE, "readonly", (s) => s.get(id));
    if (!rec) return null;
    return JSON.parse(await aesDecrypt(_key, rec.blob));
  },

  async save(session) {
    const blob = await aesEncrypt(_key, JSON.stringify(session));
    await tx(_db, STORE, "readwrite", (s) => s.put({ id: session.id, blob, updated: Date.now() }));
  },

  async remove(id) {
    await tx(_db, STORE, "readwrite", (s) => s.delete(id));
  },
};

// ---- NativeVault: Rust-managed files, key in the OS keychain ----------------

const NativeVault = {
  async init() {
    // Reachability of the native store is the real init; a failure here means
    // "not saved — vault unavailable" in the UI (e.g. the keychain refused).
    await Backend.vaultList();
    try { await migrateLegacy(); }
    catch (e) { console.warn("[vault] IndexedDB migration postponed:", e); }
  },
  list: () => Backend.vaultList(),
  load: (id) => Backend.vaultLoad(id),
  save: (session) => Backend.vaultSave(session).then(() => {}),
  remove: (id) => Backend.vaultRemove(id),
};

// One-time move of the pre-0.4.2 IndexedDB store into the native vault.
// Idempotent and safe to interrupt: a chat is only copied when the native vault does
// not have that id yet (so a chat edited natively is never overwritten by its old copy),
// and the IndexedDB is deleted only after every copied id is listed by the native side.
async function migrateLegacy() {
  if (typeof indexedDB === "undefined") return;
  if (indexedDB.databases) {
    try {
      const dbs = await indexedDB.databases();
      if (!dbs.some((d) => d.name === DB_NAME)) return;   // fresh install: nothing to do
    } catch (_) { /* fall through: open and look */ }
  }
  const db = await openDB();
  let moved = [];
  try {
    const records = await tx(db, STORE, "readonly", (s) => s.getAll());
    const keyBytes = await readDeviceKey(db);
    if (records.length && keyBytes) {
      const key = await importKey(keyBytes);
      const have = new Set((await Backend.vaultList()).map((m) => m.id));
      let unreadable = 0, failed = 0;
      for (const rec of records) {
        if (!rec || !rec.id || have.has(rec.id)) continue;
        let session;
        try { session = JSON.parse(await aesDecrypt(key, rec.blob)); }
        catch { unreadable++; continue; }            // was invisible in the sidebar already
        if (!session || session.id !== rec.id) { unreadable++; continue; }
        try { await Backend.vaultSave(session, rec.updated || Date.now()); moved.push(rec.id); }
        catch (e) { failed++; console.warn("[vault] could not move chat", rec.id, e); }
      }
      const after = new Set((await Backend.vaultList()).map((m) => m.id));
      const missing = moved.filter((id) => !after.has(id));
      if (failed || missing.length) {
        console.warn(`[vault] migration incomplete (${failed} failed, ${missing.length} unverified) — IndexedDB kept, retrying next start`);
        return;
      }
      console.info(`[vault] moved ${moved.length} chat(s) from IndexedDB into the native vault` + (unreadable ? ` (${unreadable} unreadable skipped)` : ""));
    } else if (records.length && !keyBytes) {
      console.warn("[vault] IndexedDB has chats but no key — they were unreadable before too; dropping the store");
    }
    // Belt and braces: tombstone the key record before the database goes.
    try { await tx(db, KEYS, "readwrite", (s) => s.delete(DEVICE_KEY_ID)); } catch (_) {}
  } finally {
    db.close();
  }
  await new Promise((resolve) => {
    const req = indexedDB.deleteDatabase(DB_NAME);
    req.onsuccess = req.onerror = req.onblocked = () => resolve();
  });
  await purgeWebData();
}

// Ask the webview to drop its stored site data (IndexedDB files, caches …) so the old
// key + ciphertext do not survive in WebView2's LevelDB log files until compaction.
// localStorage (settings) goes with it, so it is snapshotted and written back once the
// clear has landed (the platform clears asynchronously; we wait for it to take effect).
export async function purgeWebData() {
  let snapshot = [];
  try { for (let i = 0; i < localStorage.length; i++) { const k = localStorage.key(i); snapshot.push([k, localStorage.getItem(k)]); } }
  catch (_) { return; }
  try { await Backend.vaultPurgeWebdata(); }
  catch (e) { console.info("[vault] webview purge not available here:", (e && e.message) || e); return; }
  const restore = () => { try { for (const [k, v] of snapshot) if (localStorage.getItem(k) !== v) localStorage.setItem(k, v); } catch (_) {} };
  const t0 = Date.now();
  while (Date.now() - t0 < 3000) {                     // wait for the clear to land
    let n = 0; try { n = localStorage.length; } catch (_) {}
    if (n === 0) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  restore();
  setTimeout(restore, 2000);                            // in case the clear landed late
  console.info("[vault] webview site data purged; settings restored");
}

// Resolved per call, not at import: window.__TAURI__ is injected before page scripts
// run, but nothing here should depend on module evaluation order.
const impl = () => (isTauri() ? NativeVault : LegacyVault);
export const Vault = {
  init: () => impl().init(),
  list: () => impl().list(),
  load: (id) => impl().load(id),
  save: (session) => impl().save(session),
  remove: (id) => impl().remove(id),
};

// ---- PassphraseCrypto: for portable handover.md ---------------------------
// Envelope is JSON so it round-trips as text. Not age's on-disk format, but the
// same primitive shape; swap here if you later want `age` CLI interop.

const PBKDF2_ITERS = 210000;
const ENVELOPE_TAG = "scrambleai-enc@1";

async function deriveFromPassphrase(passphrase, salt) {
  const base = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(passphrase),
    "PBKDF2",
    false,
    ["deriveKey"],
  );
  return crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: PBKDF2_ITERS, hash: "SHA-256" },
    base,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

export const PassphraseCrypto = {
  /** @returns {Promise<string>} a text envelope safe to write to a .md.enc file */
  async encrypt(plaintext, passphrase) {
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const key = await deriveFromPassphrase(passphrase, salt);
    const ct = await crypto.subtle.encrypt(
      { name: "AES-GCM", iv },
      key,
      new TextEncoder().encode(plaintext),
    );
    return JSON.stringify({
      tag: ENVELOPE_TAG,
      kdf: `pbkdf2-sha256:${PBKDF2_ITERS}`,
      salt: b64(salt),
      iv: b64(iv),
      ct: b64(new Uint8Array(ct)),
    });
  },

  isEnvelope(text) {
    try {
      return JSON.parse(text).tag === ENVELOPE_TAG;
    } catch {
      return false;
    }
  },

  async decrypt(envelopeText, passphrase) {
    const env = JSON.parse(envelopeText);
    if (env.tag !== ENVELOPE_TAG) throw new Error("not a ScrambleAI envelope");
    const key = await deriveFromPassphrase(passphrase, ub64(env.salt));
    const pt = await crypto.subtle.decrypt(
      { name: "AES-GCM", iv: ub64(env.iv) },
      key,
      ub64(env.ct),
    );
    return new TextDecoder().decode(pt);
  },
};

function b64(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}
function ub64(str) {
  const bin = atob(str);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
