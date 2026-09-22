// Every Backend.<name> the interface calls must exist in public/backend.js — otherwise the
// call site gets `undefined`, a catch() swallows the TypeError, and a feature silently
// disappears (2026-09-10, in the first app: the three-word check on iOS).
import { readFileSync, readdirSync } from "node:fs";
const pub = new URL("../public/", import.meta.url);
const backend = readFileSync(new URL("backend.js", pub), "utf8");
const defined = new Set([...backend.matchAll(/^  ([a-zA-Z]+): /gm)].map((m) => m[1]));
const files = readdirSync(pub).filter((f) => (f.endsWith(".js") || f.endsWith(".html")) && f !== "backend.js");
const missing = [];
for (const f of files) {
  const s = readFileSync(new URL(f, pub), "utf8");
  for (const m of s.matchAll(/Backend\.([a-zA-Z]+)/g)) if (!defined.has(m[1])) missing.push(`${f}: Backend.${m[1]}`);
}
if (missing.length) { console.error("calls to Backend methods that do not exist:\n  " + [...new Set(missing)].join("\n  ")); process.exit(1); }
console.log(`backend.js: ${defined.size} methods, every call in the interface resolves`);
