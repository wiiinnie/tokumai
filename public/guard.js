// ---------------------------------------------------------------------------
// guard.js — the sensitive-data / anti-doxxing guard. ENTIRELY LOCAL.
//
// This scans what you're about to send for things you probably didn't mean to
// share — a credit-card number, an IBAN, a password, your recovery phrase — and
// warns you BEFORE it leaves the device. It is the opposite of ChatControl: the
// result is shown ONLY to you, is never transmitted, logged, or stored, and the
// check can be turned off. It only ever warns; you always decide.
//
// Deterministic, offline, no model call. Images are handled by the UI via local
// OCR (Tesseract WASM) whose extracted text is fed back through scanText().
// ---------------------------------------------------------------------------

import { BIP39_EN } from "./bip39-en.js";

export const GUARD_DEFAULTS = Object.freeze({
  enabled: true, // master switch
  scanText: true, // scan the prompt before sending
  scanImages: true, // OCR + scan an image before attaching
});

function luhn(num) {
  let sum = 0;
  let alt = false;
  for (let i = num.length - 1; i >= 0; i--) {
    let d = +num[i];
    if (alt) {
      d *= 2;
      if (d > 9) d -= 9;
    }
    sum += d;
    alt = !alt;
  }
  return sum % 10 === 0;
}

function ibanValid(raw) {
  const s = raw.replace(/\s/g, "").toUpperCase();
  if (!/^[A-Z]{2}\d{2}[A-Z0-9]{10,30}$/.test(s)) return false;
  const r = s.slice(4) + s.slice(0, 4);
  const n = r.replace(/[A-Z]/g, (c) => String(c.charCodeAt(0) - 55));
  let rem = 0;
  for (const ch of n) rem = (rem * 10 + +ch) % 97;
  return rem === 1;
}

/**
 * Scan text for sensitive categories. Returns a de-duplicated list of
 * { type, label } — one entry per category found. Empty = looks clean.
 */
export function scanText(text, opts = {}) {
  const t = String(text ?? "");
  const found = new Map();
  const add = (type, label) => {
    if (!found.has(type)) found.set(type, { type, label });
  };

  // Credit-card: either a contiguous 13–19 digit block, OR card-style groups on
  // ONE line (4-4-4-… with a single space/dash between groups). Requiring that
  // structure — and never crossing a newline — stops a column of small table
  // numbers from being glued into a fake "card number". Normally Luhn-valid; in
  // OCR mode we drop Luhn (a misread digit off a card photo shouldn't let it slip).
  const cardRe = /\b(?:\d{4}[ -]\d{4}[ -]\d{2,6}(?:[ -]\d{1,5})?|\d{13,19})\b/g;
  for (const m of t.matchAll(cardRe)) {
    const d = m[0].replace(/\D/g, "");
    if (d.length >= 13 && d.length <= 19 && (luhn(d) || opts.ocr))
      add("card", opts.ocr ? "a card number (read from the image)" : "a credit-card number");
  }
  // OCR-only signals for scanned documents / cards:
  //  • MRZ — the "<<<…" machine-readable zone on passports, visas and ID cards.
  //    Almost nothing else produces a run of chevrons, so it's a strong tell.
  if (opts.ocr && /<{3,}/.test(t)) add("mrz", "an ID document (passport / visa / ID — machine-readable zone)");
  //  • Wording that appears on cards/IDs even when the number itself is embossed
  //    and misread by OCR.
  if (opts.ocr && /\b(credit\s*card|kreditkarte|debit|card\s*number|kartennummer|exp(?:iry|ires)|g(?:ü|u)ltig bis|valid thru|cvv|iban|personalausweis|reisepass|passport|passeport|paspoort|visa|schengen|identity card|driver'?s? licen[cs]e|führerschein)\b/i.test(t))
    add("doc", "an ID or payment document (from the image)");
  // IBAN (mod-97 checksum).
  for (const m of t.matchAll(/\b[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]){10,30}\b/gi)) {
    if (ibanValid(m[0])) add("iban", "an IBAN / bank account");
  }
  if (/\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b/.test(t)) add("email", "an email address");
  if (/\b\d{3}-\d{2}-\d{4}\b/.test(t)) add("ssn", "a social-security / national-ID number");
  if (/\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b/.test(t))
    add("ip", "an IP address");
  // Phone: a + prefix or clear grouping, 7–15 digits. Mask card-length digit runs
  // first, so a 16-digit card number can't be mis-read as a phone via a substring.
  const noCards = t.replace(/(?:\d[ \-\n]?){13,19}/g, " ");
  for (const m of noCards.matchAll(/(?:\+\d{1,3}[\s.-]?)?(?:\(?\d{2,4}\)?[\s.-]){2,}\d{2,4}/g)) {
    const raw = m[0].trim();
    // A bare date (ISO 2026-08-15 or 15.08.2026 / 15/08/26) is three short groups
    // — skip it so dates on invoices/letters aren't mis-flagged as phone numbers.
    // Only skips when the WHOLE match is a date; real phones have longer groups.
    if (/^\d{4}[.\/-]\d{1,2}[.\/-]\d{1,2}$/.test(raw) ||
        /^\d{1,2}[.\/-]\d{1,2}[.\/-]\d{2,4}$/.test(raw)) continue;
    const d = m[0].replace(/\D/g, "");
    if (d.length >= 7 && d.length <= 15 && !luhn(d)) add("phone", "a phone number");
  }
  // API keys / access tokens (common shapes).
  if (/\b(sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{35})\b/.test(t))
    add("apikey", "an API key / access token");
  if (/-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(t)) add("privkey", "a private key");
  // "password: …" style secrets.
  if (/\b(pass(?:word|wort)?|pwd|secret|token|api[_ -]?key)\b\s*[:=]\s*\S{6,}/i.test(t))
    add("password", "a password / secret");
  // 64-char hex — a raw private key or seed.
  if (/\b[0-9a-fA-F]{64}\b/.test(t)) add("hexsecret", "a 64-char hex secret (possible private key)");
  // BIP39 recovery phrase: ≥12 consecutive words all in the wordlist.
  const words = t.toLowerCase().match(/[a-z]+/g) || [];
  let run = 0;
  for (const w of words) {
    if (BIP39_EN.has(w)) {
      if (++run >= 12) {
        add("mnemonic", "a recovery phrase (BIP39 seed words)");
        break;
      }
    } else run = 0;
  }
  // Weak postal-address heuristic (number + street suffix). Low confidence.
  if (
    /\b\d{1,5}\s+([A-Za-zäöüß.\-]+\s){0,3}(street|st\.?|avenue|ave\.?|road|rd\.?|lane|ln\.?|boulevard|blvd\.?|stra(?:ß|ss)e|str\.?|weg|platz|gasse|allee)\b/i.test(t)
  )
    add("address", "a postal address");

  return [...found.values()];
}
