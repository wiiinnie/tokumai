// ---------------------------------------------------------------------------
// pii.js — the personal-information assistant. ENTIRELY ON THIS DEVICE.
//
// Before anything leaves the device — the prompt, the Memory Palace's index, a memory
// the model asked for — this finds what would tell a model who you are: your employer,
// people's names, email, phone, postal address, IBAN or card number, an exact birth date.
// The person decides: anonymize (a stable placeholder such as [EMPLOYER_1] goes out
// instead), send as is, or never ask again for that value. The mapping from placeholder
// to value stays on the device (in the chat, and in the palace, sealed), and the answer
// is shown with the real values put back.
//
// Detection is deterministic: patterns for structured data and trigger phrases ("ich
// arbeite bei …", "I work at …", "meine Frau …"). A company named without such a phrase
// is not personal: "I drive a BMW" says nothing about who the person is, and a list of
// known companies flagged every brand (2026-11-23, a car remembered as [EMPLOYER_1]).
// A small on-device NER model can come later behind the same interface. It can be turned
// off completely (mode "off").
//
// Pure: no storage, no DOM, no network. Runs the same in the app and under node --test.
// ---------------------------------------------------------------------------

export const PII_MODES = ["ask", "always", "off"];
export const PII_DEFAULTS = Object.freeze({ mode: "ask" });

const PREFIX = {
  employer: "EMPLOYER", company: "COMPANY", person: "PERSON", email: "EMAIL", phone: "PHONE",
  address: "ADDRESS", iban: "IBAN", card: "CARD", birthdate: "BIRTHDATE",
};

const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

// Words after a trigger that are not names.
const NOT_NAMES = new Set(["Ich", "I", "Er", "Sie", "Es", "Wir", "Ihr", "Der", "Die", "Das", "Den", "Dem", "Ein", "Eine",
  "Und", "Oder", "Aber", "Ist", "Hat", "War", "The", "A", "An", "And", "Or", "But", "Is", "Has", "Was", "Home", "Hause",
  "Heute", "Gestern", "Morgen", "Today", "Yesterday", "Tomorrow", "Moment", "Zeit", "Teilzeit", "Vollzeit", "Montag",
  "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag", "Sonntag", "Monday", "Tuesday", "Wednesday", "Thursday",
  "Friday", "Saturday", "Sunday", "Remote", "Homeoffice", "Home-Office", "Also", "Auch", "Noch", "Schon", "Mir", "Mich",
  "Uns", "Euch", "Ihm", "Ihn", "Ihnen", "Dir", "Dich", "Me", "My", "Mein", "Meine", "Dort", "Hier", "There", "Here"]);

const CAP = "[A-ZÄÖÜ][\\p{L}\\p{N}&.'’\\-]*";
const ORG_TAIL = "(?:\\s+(?:" + CAP + "|AG|GmbH|SE|KG|KGaA|e\\.V\\.|Inc\\.?|Ltd\\.?|LLC|plc|Corp\\.?|& Co\\.?(?: KG)?))";
const ORG_AT = new RegExp("(?:(?:der|dem|den|die|the)\\s+)?(" + CAP + ORG_TAIL + "{0,3})", "uy");
const PERSON_AT = new RegExp("(?:(?:Herr|Frau|Dr\\.|Mr\\.?|Mrs\\.?|Ms\\.?)\\s+)?(" + CAP + "(?:\\s+" + CAP + ")?)", "uy");

const EMPLOYER_TRIGGER = /(?<![\p{L}])(?:arbeite[nt]?\s+(?:jetzt\s+|seit\s+\S+\s+|noch\s+)?(?:bei|für)|bin\s+(?:jetzt\s+|seit\s+\S+\s+)?(?:angestellt\s+|beschäftigt\s+|tätig\s+)?bei|(?:mein|unser)\s+arbeitgeber\s+(?:ist|heißt)|(?:mein|meine)\s+(?:chef|chefin)\s+bei|(?:job|stelle|praktikum|ausbildung)\s+bei|angefangen\s+bei|wechsle\s+zu|work(?:s|ing)?\s+(?:now\s+)?(?:at|for)|employed\s+(?:at|by)|(?:my|our)\s+employer\s+is|(?:my\s+)?boss\s+at|(?:job|internship|position)\s+at|(?:i|we)\s+joined)\s+/giu;
const PERSON_TRIGGER = /(?<![\p{L}])(?:(?:mein|meine|meinem|meiner|meinen)\s+(?:chef|chefin|frau|mann|ehefrau|ehemann|freund|freundin|kollege|kollegin|sohn|tochter|bruder|schwester|mutter|vater|partner|partnerin|nachbar|nachbarin|vermieter|vermieterin|ärztin|arzt|therapeut|therapeutin|lehrer|lehrerin)(?:\s+heißt|,)?|ich\s+heiße|mein\s+name\s+ist|namens|my\s+(?:boss|wife|husband|friend|girlfriend|boyfriend|colleague|coworker|son|daughter|brother|sister|mother|mom|mum|father|dad|partner|neighbou?r|landlord|landlady|doctor|therapist|teacher)(?:\s+is\s+called|\s+named|\s+is|,)?|my\s+name\s+is|i'?m\s+called|named)\s+/giu;
const BIRTH_TRIGGER = /(?<![\p{L}])(?:geboren\s+(?:am\s+)?|geb\.\s*|geburtstag\s+(?:ist\s+)?(?:am\s+)?|geburtsdatum:?\s*|born\s+(?:on\s+)?|birthday\s+(?:is\s+)?(?:on\s+)?|date\s+of\s+birth:?\s*|dob:?\s*)/giu;
const MONTHS = "Januar|Februar|März|Maerz|April|Mai|Juni|Juli|August|September|Oktober|November|Dezember|January|February|March|May|June|July|October|December|Jan\\.?|Feb\\.?|Mar\\.?|Apr\\.?|Jun\\.?|Jul\\.?|Aug\\.?|Sep\\.?|Sept\\.?|Oct\\.?|Okt\\.?|Nov\\.?|Dec\\.?|Dez\\.?";
const DATE_AT = new RegExp("(\\d{1,2}\\.\\s?\\d{1,2}\\.\\s?(?:19|20)?\\d{2}|(?:19|20)\\d{2}-\\d{2}-\\d{2}|\\d{1,2}\\.?\\s+(?:" + MONTHS + ")\\s+(?:19|20)\\d{2}|(?:" + MONTHS + ")\\s+\\d{1,2}(?:st|nd|rd|th)?,?\\s+(?:19|20)\\d{2}|\\d{1,2}/\\d{1,2}/(?:19|20)?\\d{2})", "uy");

const EMAIL = /[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}/g;
const PHONE = /(?<![\w+.\/])(?:(?:\+|00)[1-9]\d{0,2}[\s\-/.]?(?:\(0\)\s?)?\d(?:[\s\-/]?\d){6,13}|0\d{2,4}(?:[\s\-/]?\d){5,11})(?![\w.])/g;
const IBAN = /\b[A-Z]{2}\d{2}(?:\s?[A-Z0-9]){11,30}\b/g;
const CARD = /(?<!\d)(?:\d[ \-]?){12,18}\d(?!\d)/g;
const ADDRESS_DE = /(?<![\p{L}])[A-ZÄÖÜ][\p{L}\-]*(?:straße|strasse|str\.|weg|platz|allee|gasse|ring|damm|ufer|chaussee|steig|pfad)\s+\d{1,4}\s?[a-zA-Z]?(?![\p{L}\d])(?:,?\s+\d{5}\s+[A-ZÄÖÜ][\p{L}\-]+)?/gu;
const ADDRESS_EN = /\b\d{1,5}\s+(?:[A-Z][a-z]+\s){1,3}(?:Street|St|Avenue|Ave|Road|Rd|Lane|Ln|Boulevard|Blvd|Drive|Dr|Court|Ct|Way|Place|Pl|Terrace|Close)\b\.?(?:,?\s+[A-Z][a-z]+(?:,\s*[A-Z]{2})?\s+\d{5})?/g;

function luhn(num) {
  let sum = 0, alt = false;
  for (let i = num.length - 1; i >= 0; i--) {
    let d = +num[i];
    if (alt) { d *= 2; if (d > 9) d -= 9; }
    sum += d; alt = !alt;
  }
  return sum % 10 === 0;
}

function ibanValid(raw) {
  const s = raw.replace(/\s/g, "").toUpperCase();
  if (!/^[A-Z]{2}\d{2}[A-Z0-9]{11,30}$/.test(s)) return false;
  const r = s.slice(4) + s.slice(0, 4);
  const n = r.replace(/[A-Z]/g, (c) => String(c.charCodeAt(0) - 55));
  let rem = 0;
  for (const ch of n) rem = (rem * 10 + +ch) % 97;
  return rem === 1;
}

function trimName(s) {
  return s.replace(/[.,;:!?'’\-]+$/u, "").trim();
}

function* triggered(text, trigger, at, kind) {
  trigger.lastIndex = 0;
  let m;
  while ((m = trigger.exec(text)) !== null) {
    at.lastIndex = m.index + m[0].length;
    const n = at.exec(text);
    if (!n) continue;
    const words = trimName(n[1]).split(/\s+/).filter((w) => !NOT_NAMES.has(w));
    if (!words.length || NOT_NAMES.has(trimName(n[1]).split(/\s+/)[0])) continue;
    const value = trimName(n[1]);
    if (!value || value.length < 2) continue;
    const start = n.index + n[0].length - n[1].length;
    yield { kind, value, start, end: start + value.length };
  }
}

/**
 * Find personal information in a text.
 * @param {string} text
 * @param {{ignore?: string[]}} [opts] values never to report ("don't ask again")
 * @returns {Array<{kind:string,value:string,start:number,end:number}>} sorted, not overlapping
 */
export function detectPII(text, opts = {}) {
  if (!text) return [];
  const ignore = new Set((opts.ignore || []).map((v) => v.toLowerCase()));
  const found = [];
  const push = (kind, value, start) => found.push({ kind, value, start, end: start + value.length });
  for (const m of text.matchAll(EMAIL)) push("email", m[0], m.index);
  for (const m of text.matchAll(IBAN)) if (ibanValid(m[0])) push("iban", m[0], m.index);
  for (const m of text.matchAll(CARD)) {
    const digits = m[0].replace(/\D/g, "");
    if (digits.length >= 13 && digits.length <= 19 && luhn(digits)) push("card", m[0], m.index);
  }
  for (const m of text.matchAll(PHONE)) {
    const digits = m[0].replace(/\D/g, "");
    if (digits.length >= 8 && digits.length <= 15 && !/^\d{1,2}\.\d{1,2}\.\d{2,4}$/.test(m[0])) push("phone", m[0].trim(), m.index);
  }
  for (const m of text.matchAll(ADDRESS_DE)) push("address", m[0], m.index);
  for (const m of text.matchAll(ADDRESS_EN)) push("address", m[0], m.index);
  for (const f of triggered(text, BIRTH_TRIGGER, DATE_AT, "birthdate")) found.push(f);
  for (const f of triggered(text, EMPLOYER_TRIGGER, ORG_AT, "employer")) found.push(f);
  for (const f of triggered(text, PERSON_TRIGGER, PERSON_AT, "person")) found.push(f);
  // Structured finds and triggered ones first, then the longer; no overlaps.
  const rank = { email: 0, iban: 0, card: 0, phone: 1, address: 1, birthdate: 1, employer: 2, person: 2, company: 3 };
  found.sort((a, b) => rank[a.kind] - rank[b.kind] || (b.end - b.start) - (a.end - a.start) || a.start - b.start);
  const kept = [];
  for (const f of found) {
    if (ignore.has(f.value.toLowerCase())) continue;
    if (kept.some((k) => f.start < k.end && k.start < f.end)) continue;
    kept.push(f);
  }
  return kept.sort((a, b) => a.start - b.start);
}

/**
 * The placeholder for a value: the one it already has in any of the mappings, or the
 * next free number of its kind.
 * @param {Array<{ph:string,value:string,kind:string}>} mapping
 */
export function placeholderFor(value, kind, mapping) {
  const have = mapping.find((m) => m.value.toLowerCase() === value.toLowerCase());
  if (have) return { ph: have.ph, isNew: false };
  // An employer and a company are the same value space: one name, one placeholder.
  const prefix = PREFIX[kind] || "INFO";
  let n = 0;
  for (const m of mapping) {
    const r = new RegExp("^\\[" + prefix + "_(\\d+)\\]$").exec(m.ph);
    if (r) n = Math.max(n, +r[1]);
  }
  return { ph: `[${prefix}_${n + 1}]`, isNew: true };
}

/**
 * Replace the findings by placeholders. `mapping` is read (and the new entries are
 * returned, not written into it).
 * @returns {{text:string, added:Array<{ph:string,value:string,kind:string}>}}
 */
export function redact(text, findings, mapping) {
  const all = [...mapping];
  const added = [];
  const parts = [];
  let at = 0;
  for (const f of [...findings].sort((a, b) => a.start - b.start)) {
    if (f.start < at) continue;
    const { ph, isNew } = placeholderFor(f.value, f.kind, all);
    if (isNew) { const e = { ph, value: f.value, kind: f.kind }; all.push(e); added.push(e); }
    parts.push(text.slice(at, f.start), ph);
    at = f.end;
  }
  parts.push(text.slice(at));
  // The same value again where nothing flagged it ("bei der Telekom … auch bei der
  // Telekom"): once anonymised, it is anonymised everywhere in the text.
  return { text: anonymizeKnown(parts.join(""), added), added };
}

/** Values the person already chose to anonymize are replaced without asking again. */
export function anonymizeKnown(text, mapping) {
  if (!text) return text;
  let out = text;
  for (const m of [...mapping].sort((a, b) => b.value.length - a.value.length)) {
    if (!m.value || m.value.length < 2) continue;
    const re = new RegExp("(?<![\\p{L}\\p{N}])" + esc(m.value) + "(?![\\p{L}\\p{N}])", "giu");
    out = out.replace(re, m.ph);
  }
  return out;
}

/** Put the real values back, for display on this device only. */
export function deanonymize(text, mapping) {
  if (!text || !mapping || !mapping.length) return text;
  let out = text;
  for (const m of mapping) {
    if (!m.ph) continue;
    out = out.split(m.ph).join(m.value);
  }
  return out;
}

/** The whole pass for text that leaves the device in "always" mode (or after "Anonymize"). */
export function anonymizeAll(text, mapping, opts = {}) {
  const known = anonymizeKnown(text, mapping);
  const r = redact(known, detectPII(known, opts), mapping);
  return r;
}
