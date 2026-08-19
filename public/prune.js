// ---------------------------------------------------------------------------
// prune.js — "smart pruning": send the model less without losing the answer.
//
// Users drift within a session — weather, then football, then a recipe. Sending
// the WHOLE transcript every turn burns tokens the new question does not need.
// Pruning happens HERE, client-side, before the history is handed to the
// backend, because the server bills exactly what it receives. It is the honest
// lever: fewer tokens leave the device.
//
// A hard rule shapes every choice: the topic-shift decision is LEXICAL, never a
// model call. Asking a model "is this a new topic?" would spend the very tokens
// we are trying to save. So we use cheap set-overlap heuristics and lean on the
// test harness (scripts/prune-eval.ts) to prove the quality holds.
//
// This is the SINGLE source of truth: the desktop UI imports it, and the eval
// script imports the same file, so what we measure is what ships.
// ---------------------------------------------------------------------------

export const PRUNE_DEFAULTS = Object.freeze({
  topicShift: true, // drop older turns unrelated to the new question
  whitespace: true, // collapse redundant whitespace (lossless)
  requestTrim: true, // strip leading/trailing filler from the user's prompt
  handover: true, // prune the same way when exporting a handover
});

// Stopwords in EN + DE — the app is German-facing but users mix languages. These
// carry no topic signal, so they must not count toward "relatedness".
const STOP = new Set(
  (
    "the a an and or but of to in on at for with is are was were be been being do does did " +
    "this that these those it its i you he she we they them my your our their me us him her " +
    "what which who whom whose when where why how not no yes can could would should will shall " +
    "have has had of off out up down over under again then than so as if about into your " +
    "der die das ein eine einen und oder aber von zu in im am für mit ist sind war waren sein " +
    "ich du er sie es wir ihr dies das diese jener was welche wer wann wo warum wie nicht kein " +
    "haben hat hatte werden wird man auch noch nur schon sehr mehr als wenn dann dass den dem"
  ).split(/\s+/),
);

/** Topic-bearing terms of a string: lowercased words ≥3 chars, minus stopwords. */
export function terms(text) {
  const out = new Set();
  for (const w of String(text).toLowerCase().match(/[a-zà-ÿ0-9]{3,}/gi) || []) {
    if (!STOP.has(w)) out.add(w);
  }
  return out;
}

/** Overlap of two term sets in [0,1]. 0 when either is empty. */
export function jaccard(a, b) {
  if (!a.size || !b.size) return 0;
  let inter = 0;
  for (const t of a) if (b.has(t)) inter++;
  return inter / (a.size + b.size - inter);
}

// Anaphora / continuation markers: a terse "and tomorrow?" refers BACK, so its
// low word-overlap with the prior turn is a follow-up, not a topic change. We
// keep recent context when we see these, erring toward answer quality.
const FOLLOWUP =
  /\b(it|its|that|this|these|those|they|them|he|she|him|her|there|then|instead|also|too|more|another|again|tomorrow|yesterday|previous|above|earlier|latter|former|same|its?|und|dazu|davon|stattdessen|auch|nochmal|weiter|dann|obige|selbe)\b/i;

/** Is the new prompt a follow-up that needs the recent turns to make sense? */
export function isFollowup(text) {
  const t = String(text).trim();
  const words = t.split(/\s+/).filter(Boolean).length;
  return words <= 4 || FOLLOWUP.test(t);
}

/** Collapse redundant whitespace. Lossless — never changes wording. */
export function normalizeWhitespace(s) {
  return String(s)
    .replace(/[ \t ]+/g, " ")
    .replace(/ *\n/g, "\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

// Leading politeness wrappers and trailing thanks. Conservative on purpose: it
// removes framing, never the actual request. Falls back to the original if it
// would empty the string.
const LEAD =
  /^\s*(please|kindly|pls|could you( please)?|would you( kindly| please)?|can you( please)?|i(?:'| a| wa)?m wondering if you (?:could|can)|i was wondering if you (?:could|can)|i(?:'| woul)d like (?:you )?to|i want you to|if you don'?t mind[, ]*|bitte|könntest du( bitte)?|kannst du( bitte)?|würdest du( bitte)?|ich möchte(,? dass du)?|ich hätte gerne)\b[,:]?\s*/i;
const TRAIL =
  /[\s,]*(please|thanks?( (a lot|in advance))?|thank you|thx|danke( im voraus| dir| sehr)?|vielen dank)\s*[.!]*\s*$/i;

/** Strip filler from a single user request. */
export function trimRequest(s) {
  let out = String(s);
  out = out.replace(LEAD, "");
  out = out.replace(TRAIL, "");
  out = out.trim();
  return out || String(s).trim();
}

/**
 * Prune a message array in place-safe fashion (returns a new array).
 *
 * `messages`: [{role:'user'|'assistant', content}], oldest first, the LAST entry
 * being the new prompt about to be sent. Attachments are not touched here.
 */
export function pruneMessages(messages, opts = PRUNE_DEFAULTS, cfg = {}) {
  const KEEP_RECENT_PAIRS = cfg.keepRecentPairs ?? 1; // last exchange as safety net
  const THRESHOLD = cfg.threshold ?? 0.08; // min overlap to keep an older turn
  if (!Array.isArray(messages) || messages.length === 0) return [];
  const msgs = messages.map((m) => ({ ...m, content: String(m.content ?? "") }));
  const last = msgs[msgs.length - 1];

  // 1. request trim (new prompt only) + whitespace (everything)
  if (opts.requestTrim && last.role === "user") last.content = trimRequest(last.content);
  if (opts.whitespace) for (const m of msgs) m.content = normalizeWhitespace(m.content);

  // 2. topic-shift pruning of the prior history
  if (!opts.topicShift || msgs.length <= 1) return msgs;

  const prompt = last;
  // Empty turns carry nothing: a send that errored leaves an answerless
  // assistant shell behind, which must not masquerade as a complete exchange.
  const prior = msgs.slice(0, -1).filter((m) => m.content.trim() !== "");

  // Pair user+assistant so an answer is never kept without its question.
  const pairs = [];
  for (let i = 0; i < prior.length; ) {
    if (prior[i].role === "user" && prior[i + 1]?.role === "assistant") {
      pairs.push([prior[i], prior[i + 1]]);
      i += 2;
    } else {
      pairs.push([prior[i]]);
      i += 1;
    }
  }

  const pterms = terms(prompt.content);

  // First pass: keep every prior turn that shares subject matter with the new
  // question (this also re-includes an EARLIER topic the user returns to).
  const related = [];
  for (const pair of pairs) {
    if (jaccard(terms(pair.map((m) => m.content).join(" ")), pterms) >= THRESHOLD) {
      related.push(...pair);
    }
  }

  // Anchor rule: NEVER send a prompt without a real exchange behind it while
  // one exists. Zero overlap is far more often a rephrased follow-up ("how
  // about the warranty?") than a true restart — lexical overlap can't tell
  // them apart — and a model that gets only the bare question visibly loses
  // the thread. The anchor must be a COMPLETE user+assistant pair: after a
  // failed send, the retried question matches its own orphaned duplicate at
  // 100% overlap, and that orphan carries no context at all. So whenever the
  // kept set holds no assistant answer, the most recent complete exchange
  // rides along; on a genuine topic change it costs one pair, on a follow-up
  // it saves the answer.
  let kept = related;
  if (!kept.some((m) => m.role === "assistant")) {
    const anchor = pairs.filter((p) => p.length === 2).slice(-KEEP_RECENT_PAIRS).flat();
    if (anchor.length) {
      const keep = new Set([...kept, ...anchor]);
      kept = prior.filter((m) => keep.has(m));
    }
  }

  return [...kept, prompt];
}

/**
 * Prune a whole session's message list for a handover EXPORT.
 *
 * Deliberately LOSSLESS: an export is the artefact itself, so dropping a turn
 * would lose content for good — exactly what the user said not to do. So here we
 * only tidy (collapse whitespace, strip request filler) and keep every turn.
 * Topic-shift dropping is reserved for the send path, where the local session
 * still holds the full history.
 */
export function pruneForHandover(messages, opts = PRUNE_DEFAULTS) {
  if (!Array.isArray(messages)) return [];
  if (!opts.handover) return messages.map((m) => ({ ...m }));
  return messages.map((m) => {
    let c = String(m.content ?? "");
    if (opts.whitespace) c = normalizeWhitespace(c);
    if (opts.requestTrim && m.role === "user") c = trimRequest(c);
    return { ...m, content: c };
  });
}
