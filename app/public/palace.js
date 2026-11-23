// ---------------------------------------------------------------------------
// palace.js — the Memory Palace's logic on the interface side. Pure: no storage, no DOM,
// no network (the sealed copy, the queue and the sync are Rust's, `palace.rs`).
//
// - the index that goes with a question (title + description per file, never a file's
//   contents, never a file marked "never send"), anonymised;
// - the answer to a `read_memory` call (the file's lines minus the "never send" ones,
//   anonymised, cut to the size the enclave accepts);
// - the extraction: which turns are new since the last one, the prompt, and reading the
//   model's JSON back into changes the Rust side applies;
// - the "Memory updated" sheet: changes grouped by file, and their undo.
// ---------------------------------------------------------------------------

export const PALACE_DEFAULTS = Object.freeze({
  updates: "auto",      // "auto" | "ask" | "off"
  idleSecs: 60,         // after an answer, this long without input → look for new facts
});

/** What the enclave accepts per `read_memory` result, with room to spare. */
export const MAX_RESULT_BYTES = 15 * 1024;
/** `read_memory` calls per question, as the enclave counts them. */
export const MAX_READS = 3;
export const MAX_INDEX = 64;

const bytes = (s) => new TextEncoder().encode(s).length;

const PLACEHOLDER = /\[[A-Z]+_\d+\]/;
export const PLACEHOLDER_NOTE =
  "Values in square brackets such as [PERSON_1] are the user's real details, hidden from you on purpose. " +
  "Treat them as known: use them exactly as written wherever the real value belongs " +
  "(for example \"Your name is [PERSON_1]\"). The user's device shows the real value in their place.";

/**
 * The index sent with a question.
 * @param {{files:Array}} palace  the Rust side's view (live files and lines)
 * @param {(s:string)=>string} anon  the anonymiser for text leaving the device
 */
export function buildIndex(palace, anon = (s) => s) {
  if (!palace || !Array.isArray(palace.files)) return [];
  return palace.files
    .filter((f) => !f.neverSend && (f.lines || []).some((l) => !l.neverSend))
    .slice(0, MAX_INDEX)
    .map((f) => ({ id: f.id, title: anon(f.title || "").slice(0, 80), description: anon(f.description || "").slice(0, 240) }));
}

/**
 * What goes back for `read_memory(fileId)`: one fact per line. A file that is not there,
 * or must never be sent, answers that it is not available — the model is not told why.
 * @returns {{content:string, title:string, found:boolean}}
 */
export function toolResult(palace, fileId, anon = (s) => s) {
  const f = (palace && palace.files || []).find((x) => x.id === fileId);
  if (!f || f.neverSend) return { content: "This file is not available.", title: f ? f.title : "", found: false };
  const lines = (f.lines || []).filter((l) => !l.neverSend && l.text).map((l) => "- " + anon(l.text));
  let out = lines.join("\n");
  while (out && bytes(out) > MAX_RESULT_BYTES) {
    lines.pop();
    out = lines.join("\n") + "\n- (more facts left out)";
  }
  // Told why a value is in brackets, or the model takes "[PERSON_1]" for a name it does
  // not know and says so — when the person asked for exactly that name (2026-11-23).
  if (PLACEHOLDER.test(out)) out = PLACEHOLDER_NOTE + "\n\n" + out;
  return { content: out || "This file is empty.", title: f.title, found: true };
}

/**
 * The turns not yet looked at for memory: everything after `session.palaceAt` (an index
 * into the messages). Only questions and answers count.
 */
export function pendingTurns(session) {
  const msgs = (session && session.messages) || [];
  const from = Math.max(0, Number(session && session.palaceAt) || 0);
  return msgs.slice(from).filter((m) => (m.role === "you" || m.role === "ai") && (m.text || "").trim() && !m.streaming);
}

/** At most once per idle period, at least two new turns, at least one of them yours. */
export function worthExtracting(turns) {
  return turns.length >= 2 && turns.some((m) => m.role === "you" && (m.text || "").trim().length > 0);
}

const RULES = `You maintain the user's Memory Palace: short, durable facts about the user, filed by topic.
Read the conversation and decide what to remember. Rules:
- Only durable facts about the user: name, age, where they live, family, role and work, what they own or use (a car, a phone), preferences, ongoing projects, people and things that keep mattering. Every such fact the user states is worth keeping. No one-off details, no tasks, no small talk, nothing about the assistant.
- Only what the user said or clearly confirmed. Never infer or guess.
- One fact per line, short, in the third person ("Prefers short answers"). Write in the language the user writes in.
- Keep placeholders in square brackets such as [EMPLOYER_1] or [PERSON_1] exactly as they are.
- File each fact under an existing file id from the index when one fits. Each fact goes into exactly one file, never two.
- Files are broad. The usual ones: "About me" (name, age, where they live, family, what they own such as a car), "Work" (job, employer, role, colleagues), "Preferences" (how they like answers, tastes, likes and dislikes), "Health" (only what the user volunteers). A hobby, interest or project the user talks about in its own right gets a file of its own, named after it ("Aquariums", "Plant breeding", "House renovation"); a passing mention goes under "Preferences".
- A new file needs a title (one or two words) and a short description of what it holds, both in English, whatever language the facts are in.
- If the user corrects or withdraws a fact, use "update" (with "old": the old fact's words and "text": the new one) or "delete" (with "old").
- If there is nothing worth remembering, return [].
Answer with JSON only, no prose and no code fences: an array of
{"op":"add","fileId":"…","text":"…"} | {"op":"add","newFileTitleEnglish":"…","newFileDescriptionEnglish":"…","text":"…"} |
{"op":"update","fileId":"…","old":"…","text":"…"} | {"op":"delete","fileId":"…","old":"…"} |
{"op":"rename","fileId":"…","titleEnglish":"…","descriptionEnglish":"…"} — the last only for an existing file whose title or description is not in English, or no longer says what the file holds.
File titles and descriptions are ALWAYS English ("Gardening", not "Gärtnern"), even when the user writes German; only "text" keeps the user's language.`;

/**
 * The request that asks a model for new facts: the rules, the index, the new turns.
 * @param {Array<{role:string,text:string,sent?:string}>} turns  the text as it was SENT
 */
export function extractionMessages(turns, index) {
  const conv = turns.map((m) => (m.role === "you" ? "User: " : "Assistant: ") + (m.sent || m.text).trim()).join("\n\n");
  const idx = index.length ? index.map((e) => `- ${e.id}: ${e.title}${e.description ? " — " + e.description : ""}`).join("\n") : "(empty)";
  return [
    { role: "system", content: RULES },
    { role: "user", content: `Memory Palace index:\n${idx}\n\nConversation:\n${conv}\n\nReturn the JSON array now.` },
  ];
}

const norm = (s) => String(s || "").toLowerCase().replace(/\s+/g, " ").replace(/[.!;,]+$/, "").trim();

/** Words that say nothing about the fact itself ("Der Nutzer fährt …", "The user drives …"). */
const FILLER = new Set(["der", "die", "das", "nutzer", "nutzerin", "user", "the", "is", "ist", "a", "an", "ein", "eine", "einen"]);

function words(s) {
  return new Set(norm(s).split(/[^\p{L}\p{N}\[\]_]+/u).filter((w) => w.length > 1));
}

/** The line of a file that best matches `old` (same words, or most of them). */
export function findLine(file, old) {
  if (!file || !old) return null;
  const want = norm(old);
  const lines = (file.lines || []).filter((l) => !l.deleted);
  const exact = lines.find((l) => norm(l.text) === want);
  if (exact) return exact;
  const w = words(old);
  let best = null, score = 0;
  for (const l of lines) {
    const lw = words(l.text);
    const common = [...w].filter((x) => lw.has(x)).length;
    const s = common / Math.max(w.size, lw.size, 1);
    if (s > score) { best = l; score = s; }
  }
  return score >= 0.6 ? best : null;
}

/**
 * The model's answer → changes the Rust side applies (`palace_apply`). Anything that is
 * not well-formed, or names a file that does not exist, is left out.
 */
export function parseExtraction(text, palace) {
  let raw = String(text || "").trim();
  const fence = /```(?:json)?\s*([\s\S]*?)```/i.exec(raw);
  if (fence) raw = fence[1].trim();
  const a = raw.indexOf("["), b = raw.lastIndexOf("]");
  if (a < 0 || b <= a) return [];
  let arr;
  try { arr = JSON.parse(raw.slice(a, b + 1)); } catch (_) { return []; }
  if (!Array.isArray(arr)) return [];
  const files = (palace && palace.files) || [];
  const ops = [];
  // Every fact already kept, anywhere, and those added in this answer: a new one whose
  // words one of them already holds is the same fact again, often in another file
  // ("Fährt einen Porsche 911" beside "Der Nutzer fährt einen Porsche 911").
  const known = files.flatMap((f) => (f.lines || []).filter((l) => !l.deleted).map((l) => words(l.text)));
  const isKnown = (t) => {
    const w = [...words(t)].filter((x) => !FILLER.has(x));
    return w.length > 0 && known.some((k) => w.every((x) => k.has(x)));
  };
  for (const o of arr.slice(0, 40)) {
    if (!o || typeof o !== "object") continue;
    const text = typeof o.text === "string" ? o.text.trim().slice(0, 500) : "";
    const file = files.find((f) => f.id === o.fileId);
    if (o.op === "add" && text && isKnown(text)) continue;
    if (o.op === "add" && text) known.push(words(text));
    if (o.op === "add" && text) {
      if (file) ops.push({ op: "add", fileId: file.id, text });
      else {
        // The English field first; the plain name is still read, from older prompts.
        const title = String(o.newFileTitleEnglish || o.newFileTitle || "").trim();
        const description = String(o.newFileDescriptionEnglish || o.newFileDescription || "");
        if (title) ops.push({ op: "add", newFileTitle: title.slice(0, 80), newFileDescription: description.slice(0, 240), text });
      }
    } else if (o.op === "rename" && file && String(o.titleEnglish || o.title || "").trim()) {
      const title = String(o.titleEnglish || o.title).trim().slice(0, 80);
      const d = String(o.descriptionEnglish || o.description || "").trim();
      const description = d ? d.slice(0, 240) : file.description || "";
      if (title !== file.title || description !== (file.description || "")) ops.push({ op: "file.update", fileId: file.id, title, description });
    } else if ((o.op === "update" || o.op === "delete") && file) {
      const line = findLine(file, o.old || o.text);
      if (!line) continue;
      if (o.op === "update" && text) ops.push({ op: "update", fileId: file.id, lineId: line.id, text });
      if (o.op === "delete") ops.push({ op: "delete", fileId: file.id, lineId: line.id });
    }
  }
  return ops;
}

/** What `palace_apply` reported, grouped by file title for the sheet. */
export function groupDiff(applied) {
  const groups = new Map();
  for (const c of applied || []) {
    if (!["add", "update", "delete"].includes(c.op)) continue;
    const k = c.fileTitle || "—";
    if (!groups.has(k)) groups.set(k, []);
    groups.get(k).push(c);
  }
  return [...groups.entries()].map(([title, changes]) => ({ title, changes }));
}

/** The changes that undo `applied` (in reverse order). An undone add is a deletion, an
 * undone deletion comes back as a new line, an undone edit puts the old words back. */
export function inverseOps(applied) {
  const out = [];
  for (const c of [...(applied || [])].reverse()) {
    if (c.op === "add") out.push({ op: "delete", fileId: c.fileId, lineId: c.lineId });
    else if (c.op === "delete" && c.before) out.push({ op: "restore", fileId: c.fileId, text: c.before, neverSend: !!c.neverSend });
    else if (c.op === "update" && c.before) out.push({ op: "update", fileId: c.fileId, lineId: c.lineId, text: c.before });
  }
  return out;
}

/** "2 min ago" for the palace screen's header. */
export function ago(ms, now = Date.now(), never = "never") {
  if (!ms) return never;
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 60) return s + " s";
  if (s < 3600) return Math.round(s / 60) + " min";
  if (s < 86400) return Math.round(s / 3600) + " h";
  return Math.round(s / 86400) + " d";
}
