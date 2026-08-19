// prune.test.ts — the pruning ENGINE is correct and lossless where it claims to
// be. Quality-under-a-real-model is a separate, live proof: scripts/prune-eval.ts.
import assert from "node:assert";
// The UI and this test import the SAME module — what we verify is what ships.
import {
  pruneMessages,
  trimRequest,
  normalizeWhitespace,
  isFollowup,
  terms,
  jaccard,
  PRUNE_DEFAULTS,
} from "../public/prune.js";

type Msg = { role: "user" | "assistant"; content: string };
const u = (content: string): Msg => ({ role: "user", content });
const a = (content: string): Msg => ({ role: "assistant", content });
const roles = (ms: Msg[]) => ms.map((m) => m.content);

/* ---- request trimming keeps meaning ------------------------------------- */
assert.equal(trimRequest("Could you please summarise this text?"), "summarise this text?");
assert.equal(trimRequest("Bitte erkläre mir Quantenverschränkung, danke"), "erkläre mir Quantenverschränkung");
assert.equal(trimRequest("Explain recursion"), "Explain recursion", "nothing to trim → unchanged");
assert.ok(trimRequest("please").length > 0, "never returns empty");

/* ---- whitespace is lossless in wording ---------------------------------- */
assert.equal(normalizeWhitespace("a   b\t\tc"), "a b c");
assert.equal(normalizeWhitespace("line1\n\n\n\nline2   \n"), "line1\n\nline2");

/* ---- follow-up detection ------------------------------------------------- */
assert.ok(isFollowup("and what about tomorrow?"), "anaphora → follow-up");
assert.ok(isFollowup("why?"), "very short → follow-up");
assert.ok(!isFollowup("Who won the 2022 Champions League final in Paris?"), "self-contained new topic");

/* ---- topic-shift: older unrelated turns drop, the LAST pair anchors ------ */
// Anchor rule: a prompt is never sent naked while history exists — zero lexical
// overlap is more often a rephrased follow-up than a true restart, and the
// model must not visibly lose the thread. Only turns BEFORE the anchor drop.
{
  const convo: Msg[] = [
    u("Give me a recipe for carbonara."),
    a("Carbonara: guanciale, eggs, pecorino, pepper, spaghetti."),
    u("What's the weather in Berlin today?"),
    a("Berlin is 18°C with light rain expected this afternoon."),
    u("Who won the last Champions League final?"),
  ];
  const pruned = pruneMessages(convo, PRUNE_DEFAULTS) as Msg[];
  assert.ok(
    !roles(pruned).some((c) => /carbonara|guanciale/i.test(c)),
    "older unrelated turn should be pruned when the new question is football",
  );
  assert.ok(
    roles(pruned).some((c) => /weather|Berlin|rain/i.test(c)),
    "the most recent exchange must survive as the context anchor",
  );
  assert.equal(pruned[pruned.length - 1].content, "Who won the last Champions League final?");
}

/* ---- anchor rule: a retried question after an ERROR keeps real context --- */
// A failed send leaves the question orphaned (empty assistant shell). The retry
// matches its own duplicate at 100% overlap — but an orphan carries no answer,
// so the last COMPLETE exchange must still ride along as the anchor.
{
  const convo: Msg[] = [
    u("Which German handball club is the most successful right now?"),
    a("SC Magdeburg is the clear number one, ahead of Kiel."),
    u("Who do you think will be more successful long-term?"),
    a(""), // the 503 left no answer
    u("Who do you think will be more successful long-term?"),
  ];
  const pruned = pruneMessages(convo, PRUNE_DEFAULTS) as Msg[];
  assert.ok(
    roles(pruned).some((c) => /Magdeburg|Kiel|handball/i.test(c)),
    "after an errored send, the retry must keep the last complete exchange",
  );
  assert.ok(
    !pruned.some((m) => m.content.trim() === ""),
    "empty assistant shells never travel to the provider",
  );
}

/* ---- anchor rule: a marker-free rephrased follow-up keeps its context ---- */
{
  const convo: Msg[] = [
    u("Tell me about the Framework 13 laptop."),
    a("The Framework 13 is a modular, repairable notebook with swappable ports."),
    u("How does the warranty situation look in Germany?"),
  ];
  const pruned = pruneMessages(convo, PRUNE_DEFAULTS) as Msg[];
  assert.ok(
    roles(pruned).some((c) => /Framework|modular|notebook/i.test(c)),
    "a follow-up without anaphora markers must still keep the recent exchange",
  );
}

/* ---- topic-shift: a terse follow-up KEEPS the recent context ------------- */
{
  const convo: Msg[] = [
    u("What's the weather in Berlin today?"),
    a("Berlin is 18°C with light rain this afternoon."),
    u("and tomorrow?"),
  ];
  const pruned = pruneMessages(convo, PRUNE_DEFAULTS) as Msg[];
  assert.ok(
    roles(pruned).some((c) => /Berlin|weather|rain/i.test(c)),
    "a follow-up must keep the weather context it depends on",
  );
}

/* ---- topic-shift: returning to an earlier topic re-includes it ----------- */
{
  const convo: Msg[] = [
    u("Give me a recipe for carbonara."),
    a("Carbonara: guanciale, eggs, pecorino, pepper, spaghetti."),
    u("What's the capital of Peru?"),
    a("Lima."),
    u("Back to the carbonara — can I use bacon instead of guanciale?"),
  ];
  const pruned = pruneMessages(convo, PRUNE_DEFAULTS) as Msg[];
  assert.ok(roles(pruned).some((c) => /carbonara|guanciale/i.test(c)), "earlier related topic kept");
  assert.ok(!roles(pruned).some((c) => /Peru|Lima/i.test(c)), "unrelated Peru turn dropped");
}

/* ---- disabling topicShift leaves history intact (only whitespace/trim) --- */
{
  const convo: Msg[] = [u("weather?"), a("18°C"), u("football scores?")];
  const pruned = pruneMessages(convo, { ...PRUNE_DEFAULTS, topicShift: false }) as Msg[];
  assert.equal(pruned.length, 3, "no history pruning when the option is off");
}

/* ---- all options off is a pure passthrough of wording ------------------- */
{
  const convo: Msg[] = [u("  please tell me about  cats  "), a("Cats are mammals.")];
  const off = pruneMessages(convo, { topicShift: false, whitespace: false, requestTrim: false, handover: false }) as Msg[];
  assert.equal(off[0].content, "  please tell me about  cats  ", "no trimming when all off");
}

assert.ok(jaccard(terms("champions league final"), terms("who won the champions league")) > 0);

console.log("all pruning checks passed (trim, whitespace, topic-shift, follow-up, return-to-topic, off=passthrough)");
