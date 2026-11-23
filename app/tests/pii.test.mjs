// node --test app/tests — the on-device personal-information check.
import { test } from "node:test";
import assert from "node:assert/strict";
import { detectPII, redact, deanonymize, anonymizeKnown, placeholderFor, anonymizeAll } from "../public/pii.js";

const kinds = (t, o) => detectPII(t, o).map((f) => [f.kind, f.value]);

test("an employer named after a trigger phrase is found, in German and English", () => {
  assert.deepEqual(kinds("Ich arbeite bei der Telekom als Entwickler."), [["employer", "Telekom"]]);
  assert.deepEqual(kinds("ich arbeite für Siemens"), [["employer", "Siemens"]]);
  assert.deepEqual(kinds("I work at Acme Robotics Inc. in Berlin"), [["employer", "Acme Robotics Inc"]]);
  assert.deepEqual(kinds("Mein Chef bei Bosch ist streng"), [["employer", "Bosch"]]);
  assert.deepEqual(kinds("We met at Google last year"), [["company", "Google"]], "a known company without a trigger");
});

test("people, email, phone, IBAN, card, address and birth date", () => {
  assert.deepEqual(kinds("Meine Frau Anna hat Geburtstag"), [["person", "Anna"]]);
  assert.deepEqual(kinds("my boss is called Peter Smith"), [["person", "Peter Smith"]]);
  assert.deepEqual(kinds("write to max.muster@example.org please"), [["email", "max.muster@example.org"]]);
  assert.deepEqual(kinds("ruf an: +49 170 1234567"), [["phone", "+49 170 1234567"]]);
  assert.deepEqual(kinds("IBAN DE89 3704 0044 0532 0130 00"), [["iban", "DE89 3704 0044 0532 0130 00"]]);
  assert.deepEqual(kinds("card 4111 1111 1111 1111"), [["card", "4111 1111 1111 1111"]]);
  assert.deepEqual(kinds("Ich wohne in der Hauptstraße 5, 10115 Berlin"), [["address", "Hauptstraße 5, 10115 Berlin"]]);
  assert.deepEqual(kinds("ich bin geboren am 12.03.1985"), [["birthdate", "12.03.1985"]]);
  assert.deepEqual(kinds("born on March 12, 1985"), [["birthdate", "March 12, 1985"]]);
});

test("ordinary text is left alone", () => {
  for (const t of [
    "Ich arbeite bei schönem Wetter lieber draußen",
    "I work at home on Fridays",
    "Meeting am 12.03.2026 um 14:00",
    "The answer is 4111 and 1111",
    "Order 1234567890 shipped",
    "Was ist die Hauptstadt von Frankreich?",
    "my friend is happy",
    "apple pie recipe",
  ]) assert.deepEqual(kinds(t), [], t);
});

test("a value the person never wants to be asked about again is not reported", () => {
  assert.deepEqual(kinds("Ich arbeite bei der Telekom", { ignore: ["telekom"] }), []);
});

test("placeholders are stable and the round trip puts the values back", () => {
  const mapping = [];
  const text = "Ich arbeite bei der Telekom, meine Frau Anna auch bei der Telekom.";
  const r = anonymizeAll(text, mapping);
  assert.equal(r.text, "Ich arbeite bei der [EMPLOYER_1], meine Frau [PERSON_1] auch bei der [EMPLOYER_1].");
  assert.equal(r.added.length, 2);
  mapping.push(...r.added);
  // The model answers with the placeholders; the display puts the real words back.
  const answer = "Bei [EMPLOYER_1] sind Sie und [PERSON_1] gut aufgehoben.";
  assert.equal(deanonymize(answer, mapping), "Bei Telekom sind Sie und Anna gut aufgehoben.");
  // Known values are replaced without asking again; new ones get the next number.
  assert.equal(anonymizeKnown("Telekom zahlt gut", mapping), "[EMPLOYER_1] zahlt gut");
  assert.equal(placeholderFor("Siemens", "employer", mapping).ph, "[EMPLOYER_2]");
  assert.equal(placeholderFor("telekom", "company", mapping).ph, "[EMPLOYER_1]");
  const again = redact("I work for Siemens", detectPII("I work for Siemens"), mapping);
  assert.equal(again.text, "I work for [EMPLOYER_2]");
});
