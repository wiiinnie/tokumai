// node --test app/tests — the Memory Palace's interface-side logic.
import { test } from "node:test";
import assert from "node:assert/strict";
import { buildIndex, toolResult, pendingTurns, worthExtracting, extractionMessages, parseExtraction, inverseOps, groupDiff, findLine, MAX_RESULT_BYTES, PLACEHOLDER_NOTE } from "../public/palace.js";
import { _tables } from "../public/i18n.js";

const palace = {
  files: [
    { id: "f_work", title: "Work", description: "Job, role, team size", neverSend: false,
      lines: [{ id: "l_1", text: "Works at Telekom as head of a VFX studio" }, { id: "l_2", text: "Salary 90k", neverSend: true }] },
    { id: "f_health", title: "Health", description: "Private", neverSend: true, lines: [{ id: "l_3", text: "Allergic to nuts" }] },
    { id: "f_empty", title: "Empty", description: "", lines: [{ id: "l_4", text: "only private", neverSend: true }] },
  ],
};
const anon = (s) => s.replace(/Telekom/g, "[EMPLOYER_1]");

test("the index lists titles only, never a file marked never-send, anonymised", () => {
  const ix = buildIndex(palace, anon);
  assert.deepEqual(ix, [{ id: "f_work", title: "Work", description: "Job, role, team size" }]);
  assert.ok(!JSON.stringify(ix).includes("VFX"));
});

test("read_memory answers without never-send lines, anonymised and within size", () => {
  const r = toolResult(palace, "f_work", anon);
  assert.equal(r.content, PLACEHOLDER_NOTE + "\n\n- Works at [EMPLOYER_1] as head of a VFX studio", "with the note that says what a placeholder is");
  assert.ok(!r.content.includes("Salary"));
  assert.equal(toolResult(palace, "f_health").found, false);
  assert.equal(toolResult(palace, "f_nope").found, false);
  const big = { files: [{ id: "f", title: "Big", lines: Array.from({ length: 2000 }, (_, i) => ({ id: "l" + i, text: "fact number " + i + " ".repeat(20) })) }] };
  assert.ok(new TextEncoder().encode(toolResult(big, "f").content).length <= MAX_RESULT_BYTES + 40);
});

test("only new turns are extracted, in batches worth the call", () => {
  const s = { palaceAt: 2, messages: [
    { role: "you", text: "old" }, { role: "ai", text: "old answer" },
    { role: "you", text: "I moved to Hamburg", sent: "I moved to Hamburg" }, { role: "ai", text: "Nice!" },
  ] };
  const turns = pendingTurns(s);
  assert.equal(turns.length, 2);
  assert.ok(worthExtracting(turns));
  assert.ok(!worthExtracting(turns.slice(1)), "one turn is not enough");
  assert.ok(!worthExtracting([{ role: "ai", text: "a" }, { role: "ai", text: "b" }]), "no user content");
  const msgs = extractionMessages(turns, buildIndex(palace));
  assert.equal(msgs[0].role, "system");
  assert.ok(msgs[1].content.includes("- f_work: Work — Job, role, team size"));
  assert.ok(msgs[1].content.includes("User: I moved to Hamburg"));
});

test("the model's JSON becomes changes, and nonsense is dropped", () => {
  const out = parseExtraction('```json\n[{"op":"add","fileId":"f_work","text":"Leads a team of 12"},' +
    '{"op":"add","newFileTitle":"Home","newFileDescription":"Where the user lives","text":"Lives in Hamburg"},' +
    '{"op":"update","fileId":"f_work","old":"works at telekom as head of a vfx studio","text":"Works at [EMPLOYER_1] as CTO"},' +
    '{"op":"delete","fileId":"f_nope","old":"x"},{"op":"add","fileId":"f_work"},"junk"]\n```', palace);
  assert.deepEqual(out, [
    { op: "add", fileId: "f_work", text: "Leads a team of 12" },
    { op: "add", newFileTitle: "Home", newFileDescription: "Where the user lives", text: "Lives in Hamburg" },
    { op: "update", fileId: "f_work", lineId: "l_1", text: "Works at [EMPLOYER_1] as CTO" },
  ]);
  assert.deepEqual(parseExtraction("Nothing to remember.", palace), []);
  assert.deepEqual(parseExtraction("[]", palace), []);
  assert.equal(findLine(palace.files[0], "Works at Telekom as the head of a VFX studio").id, "l_1");
  assert.equal(findLine(palace.files[0], "Plays tennis"), null);
});

test("undo reverses each change", () => {
  const applied = [
    { op: "add", fileId: "f", fileTitle: "Work", lineId: "l_9", text: "x" },
    { op: "delete", fileId: "f", fileTitle: "Work", lineId: "l_1", before: "old fact" },
    { op: "update", fileId: "f", fileTitle: "Home", lineId: "l_2", text: "new", before: "old" },
  ];
  assert.deepEqual(inverseOps(applied), [
    { op: "update", fileId: "f", lineId: "l_2", text: "old" },
    { op: "restore", fileId: "f", text: "old fact", neverSend: false },
    { op: "delete", fileId: "f", lineId: "l_9" },
  ]);
  assert.deepEqual(groupDiff(applied).map((g) => [g.title, g.changes.length]), [["Work", 2], ["Home", 1]]);
});

test("German and English have the same palace strings", () => {
  assert.deepEqual(Object.keys(_tables.de).sort(), Object.keys(_tables.en).sort());
});

test("a fact already kept, in any file and in other words around it, is not added again", () => {
  const p = { files: [
    { id: "f_p", title: "Persönliches", lines: [{ id: "l1", text: "Der Nutzer fährt einen Porsche 911." }, { id: "l2", text: "Ist 53 Jahre alt." }] },
    { id: "f_i", title: "Interessen", lines: [] },
  ] };
  const ops = parseExtraction(JSON.stringify([
    { op: "add", fileId: "f_i", text: "Fährt einen Porsche 911." },
    { op: "add", fileId: "f_p", text: "Ist 54 Jahre alt." },
    { op: "add", fileId: "f_i", text: "Segelt gern." },
    { op: "add", fileId: "f_p", text: "Der Nutzer segelt gern." },
  ]), p);
  assert.deepEqual(ops.map((o) => o.text), ["Ist 54 Jahre alt.", "Segelt gern."]);
});

test("a category named in another language is renamed, in English", () => {
  const p = { files: [{ id: "f_p", title: "Persönliches", description: "Persönliche Informationen", lines: [] }] };
  const ops = parseExtraction(JSON.stringify([
    { op: "rename", fileId: "f_p", title: "About me", description: "Name, age, family, home" },
    { op: "rename", fileId: "f_nope", title: "X" },
    { op: "rename", fileId: "f_p", title: "" },
  ]), p);
  assert.deepEqual(ops, [{ op: "file.update", fileId: "f_p", title: "About me", description: "Name, age, family, home" }]);
});
