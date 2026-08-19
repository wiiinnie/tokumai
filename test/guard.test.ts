// guard.test.ts — the sensitive-data detectors fire on real secrets and stay
// quiet on ordinary text. Same module the UI ships.
import assert from "node:assert";
import { scanText } from "../public/guard.js";

const types = (t: string) => scanText(t).map((f) => f.type).sort();
const has = (t: string, type: string) => scanText(t).some((f) => f.type === type);

// ---- true positives ------------------------------------------------------
assert.ok(has("my card is 4111 1111 1111 1111", "card"), "Luhn-valid Visa test number");
assert.ok(has("pay to GB82 WEST 1234 5698 7654 32", "iban"), "valid IBAN");
assert.ok(has("reach me at jane.doe@example.com", "email"));
assert.ok(has("SSN 123-45-6789 on file", "ssn"));
assert.ok(has("server at 192.168.10.254 today", "ip"));
assert.ok(has("key sk-ABCDEFGHIJKLMNOPQRSTUVWX", "apikey"));
assert.ok(has("password: hunter2secret", "password"));
assert.ok(
  has("abandon ability able about above absent absorb abstract absurd abuse access accident", "mnemonic"),
  "12 BIP39 words",
);
assert.ok(has("call +49 30 1234 5678 tomorrow", "phone"));
assert.ok(has("ship to 221 Baker Street", "address"));

// ---- true negatives (no over-warning) ------------------------------------
assert.deepEqual(types("What is the capital of Peru and how tall is Everest?"), [], "clean prose");
assert.deepEqual(types("I scored 42 points in 3 games, my rating went to 1850"), [], "plain numbers");
assert.ok(!has("the year 2024 was warmer than 2019", "card"), "short numbers aren't cards");
assert.ok(!has("meet at 3pm to discuss abandon ship plans", "mnemonic"), "a few wordlist words aren't a seed");

// ---- OCR mode is more lenient (a misread digit breaks Luhn) --------------
assert.ok(!scanText("4578 4562 5643 9172").some((f) => f.type === "card"), "non-Luhn ignored in normal mode");
assert.ok(scanText("4578 4562 5643 9172", { ocr: true }).some((f) => f.type === "card"), "non-Luhn 16-digit flagged in OCR mode");
assert.ok(scanText("VALID THRU 05/28\nCREDIT CARD", { ocr: true }).some((f) => f.type === "doc"), "card wording flagged in OCR mode");
assert.ok(!scanText("VALID THRU 05/28 CREDIT CARD").some((f) => f.type === "doc"), "doc cue only in OCR mode");

// ---- a card number must not be mis-read as a phone (substring) -----------
{
  const f = scanText("4152 9912 3456 7890", { ocr: true }).map((x) => x.type);
  assert.ok(f.includes("card"), "16-digit read as card in OCR mode");
  assert.ok(!f.includes("phone"), "not also flagged as a phone");
}
assert.ok(scanText("call +49 30 1234 5678 tomorrow").some((f) => f.type === "phone"), "real phone still detected");
// ---- a bare date must NOT be mis-read as a phone (dates cover invoices/letters) ----
for (const d of ["2026-08-15", "15.08.2026", "15/08/26", "2026/8/5"])
  assert.ok(!scanText(d, { ocr: true }).some((f) => f.type === "phone"), `date ${d} not flagged as phone`);
assert.ok(scanText("01-23-45-67-89", { ocr: true }).some((f) => f.type === "phone"), "grouped phone still detected next to date rule");
// A column of small table numbers must NOT be glued into a fake card (OCR mode)
assert.ok(!scanText("651 652 653 654 200 fem reg 200 100", { ocr: true }).some((f) => f.type === "card"), "table numbers are not a card");
assert.ok(scanText("4152 9912 3456 7890", { ocr: true }).some((f) => f.type === "card"), "a real 4-4-4-4 card still detected in OCR mode");
// ---- MRZ / document cues (OCR) -------------------------------------------
assert.ok(scanText("VCNLDJOJA<<JOYCE<<<<<<<<<<<<<<<<", { ocr: true }).some((f) => f.type === "mrz"), "passport MRZ detected");
assert.ok(scanText("SCHENGEN VISA · NUMBER OF PASSPORT", { ocr: true }).some((f) => f.type === "doc"), "visa/passport wording detected");

console.log("all guard checks passed (card, iban, email, ssn, ip, apikey, password, mnemonic, phone, address, ocr-mode, mrz, card-not-phone; no false positives)");
