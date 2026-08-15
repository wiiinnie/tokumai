// ---------------------------------------------------------------------------
// vault.js — encrypted local persistence, behind a thin swappable interface.
//
// Two concerns, both platform-agnostic (Web Crypto + IndexedDB exist identically
// in a browser, Tauri, and a Capacitor WebView):
//
//   Vault            — the on-device working store. Sessions are AES-256-GCM
//                      encrypted at rest with a NON-EXTRACTABLE key. Nothing
//                      here assumes a filesystem, so the same code ships to
//                      mobile unchanged.
//
//   PassphraseCrypto — encrypt/decrypt a handover.md for archiving or moving
//                      between devices, unlocked by a passphrase (PBKDF2 →
//                      AES-GCM). Openable anywhere the passphrase is known.
//
// MOBILE SEAMS (marked below):
//   - KEY STORAGE: the device key lives in IndexedDB today. On Tauri/mobile,
//     swap KeyStore for the OS keystore (iOS Keychain / Android Keystore /
//     Tauri Stronghold) — the Vault interface does not change.
//   - The UI and chat logic only ever call Vault.{init,list,load,save,remove};
//     they never see IndexedDB, crypto, or a platform. That is the whole point.
// ---------------------------------------------------------------------------

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

// ---- KeyStore (MOBILE SEAM) -----------------------------------------------
// Persists a random 32-byte secret and imports it as an AES-GCM key each
// session. Raw bytes are universally structured-cloneable across browsers
// (a CryptoKey object is not), so this is the robust choice for the WebView
// tier. Tradeoff: the key bytes are readable from the DB — real key protection
// arrives with the OS keystore on Tauri/mobile, which is this seam.

async function getDeviceKey(db) {
  const raw = await tx(db, KEYS, "readonly", (s) => s.get(DEVICE_KEY_ID));
  let bytes = null;
  if (raw instanceof Uint8Array) bytes = raw;
  else if (raw instanceof ArrayBuffer) bytes = new Uint8Array(raw);
  // regenerate if missing or if a stale/incompatible record is found
  if (!bytes || bytes.length !== 32) {
    bytes = crypto.getRandomValues(new Uint8Array(32));
    await tx(db, KEYS, "readwrite", (s) => s.put(bytes, DEVICE_KEY_ID));
  }
  return crypto.subtle.importKey("raw", bytes, { name: "AES-GCM" }, false, [
    "encrypt",
    "decrypt",
  ]);
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

// ---- Vault: the on-device working store -----------------------------------

let _db = null;
let _key = null;

export const Vault = {
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
