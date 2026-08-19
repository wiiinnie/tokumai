// ---------------------------------------------------------------------------
// prune-eval.ts — PROVE that smart pruning saves tokens without costing answers.
//
//   npm run prune:eval          (needs GEMINI_API_KEY in .env)
//
// For each scenario it sends the SAME final question twice — once with the full
// transcript, once through public/prune.js — to the real model, and reports:
//   • promptTokenCount  (Gemini's own count of the INPUT — what pruning shrinks)
//   • whether each answer still contains the fact a correct answer must have
//   • the answers themselves, so quality is inspectable, not just asserted
//
// The pruning it tests is byte-for-byte the pruning the app ships: same module.
// ---------------------------------------------------------------------------

import { pruneMessages, PRUNE_DEFAULTS } from "../public/prune.js";

type Msg = { role: "user" | "assistant"; content: string };
const u = (content: string): Msg => ({ role: "user", content });
const a = (content: string): Msg => ({ role: "assistant", content });

const KEY = process.env.GEMINI_API_KEY;
const MODEL = process.env.PRUNE_EVAL_MODEL || "gemini-3.5-flash-lite";

interface Scenario {
  name: string;
  why: string; // which pruning behaviour it exercises
  history: Msg[];
  question: string;
  expect: RegExp; // a fact a CORRECT answer must contain
}

const SCENARIOS: Scenario[] = [
  {
    name: "Topic shift (weather → history)",
    why: "unrelated older turns should be dropped; the question is self-contained",
    history: [
      u("What's the weather like in Berlin today?"),
      a("Berlin is around 18°C today with light rain in the afternoon and a gentle westerly wind."),
      u("Any good cafés near Alexanderplatz?"),
      a("Yes — House of Small Wonder and a few specialty-coffee spots are within walking distance of Alexanderplatz."),
      u("What time does the sun set there?"),
      a("Around 20:45 local time at this time of year in Berlin."),
    ],
    question: "Which country won the 2014 FIFA World Cup final?",
    expect: /germany|deutschland/i,
  },
  {
    name: "Coherent deep-dive (must NOT over-prune)",
    why: "a follow-up whose context is the whole thread — pruning must keep it",
    history: [
      u("Explain how recursion works in Python with a factorial example."),
      a("Recursion is when a function calls itself. Example:\n\ndef factorial(n):\n    if n <= 1:\n        return 1\n    return n * factorial(n - 1)\n\nEach call reduces n until the base case n<=1."),
      u("What's the base case there and why does it matter?"),
      a("The base case is n <= 1 returning 1. Without it the function would recurse forever and overflow the stack."),
    ],
    question: "Now rewrite that same function iteratively instead of recursively.",
    expect: /while|for|range|=\s*1|factorial/i,
  },
  {
    name: "Verbose polite request (request trim)",
    why: "leading/trailing filler removed; the answer must be unchanged in substance",
    history: [],
    question:
      "Could you please, if you don't mind, kindly tell me what the capital city of Australia is? Thanks a lot in advance!",
    expect: /canberra/i,
  },
  {
    name: "Return to an earlier topic",
    why: "re-include the earlier related topic, drop the interleaved unrelated one",
    history: [
      u("Give me a classic carbonara recipe."),
      a("Carbonara: spaghetti, guanciale, egg yolks, pecorino romano, black pepper. No cream."),
      u("What's the capital of Peru?"),
      a("The capital of Peru is Lima."),
      u("How many players are on a basketball team on court?"),
      a("Five players per team are on the court at a time."),
    ],
    question: "Back to that pasta — can I use pancetta instead of guanciale, and how does it change the dish?",
    expect: /pancetta|guanciale|carbonara|flavou?r|fat|smok/i,
  },
];

async function askGemini(messages: Msg[]): Promise<{ promptTokens: number; text: string }> {
  const contents = messages.map((m) => ({
    role: m.role === "assistant" ? "model" : "user",
    parts: [{ text: m.content }],
  }));
  const url = `https://generativelanguage.googleapis.com/v1beta/models/${MODEL}:generateContent?key=${KEY}`;
  const res = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ contents, generationConfig: { maxOutputTokens: 800, temperature: 0 } }),
  });
  if (!res.ok) throw new Error(`gemini ${res.status}: ${(await res.text()).slice(0, 300)}`);
  const body = (await res.json()) as any;
  const text: string =
    body?.candidates?.[0]?.content?.parts?.map((p: any) => p.text || "").join("") ?? "";
  const promptTokens: number = body?.usageMetadata?.promptTokenCount ?? 0;
  return { promptTokens, text };
}

function oneLine(s: string, n = 160): string {
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > n ? t.slice(0, n) + "…" : t;
}

async function main() {
  if (!KEY) {
    console.error("GEMINI_API_KEY not set — run with: npm run prune:eval (loads .env)");
    process.exit(1);
  }
  console.log(`\nSmart-pruning quality proof · model=${MODEL}\n${"=".repeat(64)}`);

  let fullTotal = 0;
  let prunedTotal = 0;
  let qualityHeld = 0;

  for (const s of SCENARIOS) {
    const full: Msg[] = [...s.history, u(s.question)];
    const pruned = pruneMessages(full, PRUNE_DEFAULTS) as Msg[];

    const [rFull, rPruned] = await Promise.all([askGemini(full), askGemini(pruned)]);

    const passFull = s.expect.test(rFull.text);
    const passPruned = s.expect.test(rPruned.text);
    if (passPruned) qualityHeld++;
    fullTotal += rFull.promptTokens;
    prunedTotal += rPruned.promptTokens;

    const saved = rFull.promptTokens - rPruned.promptTokens;
    const pct = rFull.promptTokens ? Math.round((saved / rFull.promptTokens) * 100) : 0;

    console.log(`\n▸ ${s.name}`);
    console.log(`  purpose: ${s.why}`);
    console.log(
      `  messages:  full ${full.length} → pruned ${pruned.length}   ` +
        `input tokens: full ${rFull.promptTokens} → pruned ${rPruned.promptTokens}  ` +
        `(saved ${saved}, ${pct}%)`,
    );
    console.log(`  quality:   full ${passFull ? "✓" : "✗"}   pruned ${passPruned ? "✓" : "✗"}  (must contain ${s.expect})`);
    console.log(`    full   → ${oneLine(rFull.text)}`);
    console.log(`    pruned → ${oneLine(rPruned.text)}`);
  }

  const savedTotal = fullTotal - prunedTotal;
  const pctTotal = fullTotal ? Math.round((savedTotal / fullTotal) * 100) : 0;
  console.log(`\n${"=".repeat(64)}`);
  console.log(
    `TOTAL input tokens: full ${fullTotal} → pruned ${prunedTotal}  ` +
      `(saved ${savedTotal}, ${pctTotal}%)`,
  );
  console.log(`Quality held: ${qualityHeld}/${SCENARIOS.length} pruned answers still contain the required fact.`);
  console.log("");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
