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
  // Postal address — see the address section below. A street line on its own is a weak
  // signal; a street line WITH a postcode/city line near it is a person's address.
  const addr = scanAddress(t);
  if (addr.block)
    add("addressblock", addr.name
      ? "a name and full postal address (someone is identifiable)"
      : "a full postal address (street and city together)");
  else if (addr.street) add("address", "a postal address");

  return [...found.values()];
}

// ---------------------------------------------------------------------------
// Addresses. Split out from the detectors above because this is the one finding
// that needs LAYOUT: on a letter the name, the street and the city sit on three
// separate lines, and only together do they identify a person. Feed this the whole
// document with its line breaks intact (see `docText` in index.html) — a per-line
// scan can never see the combination, which is exactly how an address label was
// missed on a photographed letter (2026-09-06).
// ---------------------------------------------------------------------------

// German glues the street type onto the name and puts the number LAST — the old
// single rule wanted "5 Musterstraße" and therefore matched no German address at all.
const DE_SUFFIX = "(?:stra(?:ß|ss)e|str\\.|weg|platz|gasse|allee|ring|damm|ufer|chaussee|steig)";
const RE_STREET_DE = new RegExp("\\p{Lu}[\\p{L}.\\-]*" + DE_SUFFIX + "\\s+\\d{1,4}\\s?[a-zA-Z]?\\b", "iu");
// English-speaking countries put the number first and the type last.
const RE_STREET_EN =
  /\b\d{1,5}\s+(?:[\p{L}.\-]+\s){0,3}(?:street|st\.|avenue|ave\.?|road|rd\.?|lane|ln\.?|boulevard|blvd\.?|drive|dr\.|court|ct\.|way|place|pl\.)\b/iu;
// Romance/Dutch/Polish: the type leads, the number sits on either side.
const RE_STREET_INTL =
  /(?:\d{1,4}[,\s]+)?\b(?:rue|avenue|boulevard|impasse|chemin|via|viale|corso|piazza|calle|avenida|plaza|straat|laan|plein|ulica)\b[\s.,]+\p{L}[\p{L}.\-]{2,}(?:[\s,]+\d{1,4})?/iu;

// Postcode + town. DELIBERATELY never a finding on its own: "2019 Bericht" has the same
// shape as "10115 Berlin", and a guard that cries wolf gets switched off. It only ever
// counts as the second half of an address.
const RE_POST_DACH = /(?:^|[^\d])(\d{4,5})\s+\p{Lu}[\p{L}.\-]{2,}(?:[ -]\p{Lu}[\p{L}.\-]+){0,2}(?![\d])/u;
const RE_POST_NL = /\b\d{4}\s?[A-Z]{2}\b[\s,]+\p{Lu}/u;
const RE_POST_UK = /\b[A-Z]{1,2}\d[A-Z\d]?\s?\d[A-Z]{2}\b/;
const RE_POST_US = /\b\p{Lu}[\p{L}.\-]+,\s*[A-Z]{2}\s+\d{5}(?:-\d{4})?\b/u;

const hasStreet = (line) =>
  RE_STREET_DE.test(line) || RE_STREET_EN.test(line) || RE_STREET_INTL.test(line);
const hasPostCity = (line) =>
  RE_POST_DACH.test(line) || RE_POST_NL.test(line) || RE_POST_UK.test(line) || RE_POST_US.test(line);

// Two or three capitalised words, no digits, no street type: on the line above an
// address that is a person's name — position says so, no name list required.
const RE_NAMELINE = /^\s*(?:(?:Herr|Frau|Mr|Mrs|Ms|Dr|Prof)\.?\s+)?\p{Lu}[\p{L}'\-]+(?:\s+\p{Lu}[\p{L}'\-]+){1,2}\s*$/u;

/**
 * `{ street, postcity, block, name }` for a piece of text.
 * `block` = a street line and a postcode/city line close enough to be one address.
 * Works on lines when the text has them, and falls back to character distance for
 * text that arrived as one run (a PDF text layer, a chat message).
 */
export function scanAddress(text) {
  const t = String(text ?? "");
  const lines = t.split(/\r?\n/).map((l) => l.trim()).filter((l) => l.length > 0);
  const out = { street: false, postcity: false, block: false, name: false };

  if (lines.length > 1) {
    const flags = lines.map((l) => ({ s: hasStreet(l), p: hasPostCity(l), n: RE_NAMELINE.test(l) }));
    out.street = flags.some((f) => f.s);
    out.postcity = flags.some((f) => f.p);
    for (let i = 0; i < flags.length; i++) {
      if (!flags[i].s) continue;
      // A letterhead is compact: the city follows the street within a line or two.
      for (let j = Math.max(0, i - 2); j <= Math.min(flags.length - 1, i + 2); j++) {
        if (j !== i && flags[j].p) {
          out.block = true;
          // …and the line just above the street is the addressee.
          if (i > 0 && flags[i - 1].n && !flags[i - 1].s && !flags[i - 1].p) out.name = true;
        }
      }
    }
    return out;
  }

  // One run of text: no layout to read, so fall back to proximity.
  out.street = hasStreet(t);
  out.postcity = hasPostCity(t);
  if (out.street && out.postcity) {
    const si = t.search(RE_STREET_DE) >= 0 ? t.search(RE_STREET_DE)
      : t.search(RE_STREET_EN) >= 0 ? t.search(RE_STREET_EN) : t.search(RE_STREET_INTL);
    const pi = t.search(RE_POST_DACH) >= 0 ? t.search(RE_POST_DACH)
      : t.search(RE_POST_NL) >= 0 ? t.search(RE_POST_NL)
      : t.search(RE_POST_UK) >= 0 ? t.search(RE_POST_UK) : t.search(RE_POST_US);
    out.block = si >= 0 && pi >= 0 && Math.abs(si - pi) < 120;
  }
  return out;
}
