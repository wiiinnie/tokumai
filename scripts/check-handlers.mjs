// Every data-click / data-change / data-input / data-keydown handler name in public/index.html
// must be reachable as window.<name>: the app script is an ES module, so a function is only
// global when it is listed in the Object.assign(window, {...}) registry — or defined in one of
// the classic (non-module) scripts. A missing name fails silently: the tap does nothing
// (2026-09-10: App Store tiles, the consent box, the trickle toggle, the identity picker).
import { readFileSync } from "node:fs";
const s = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
const i = s.indexOf("Object.assign(window,{");
const j = s.indexOf("});", i);
const reg = new Set(s.slice(i + "Object.assign(window,{".length, j).split(/[,\s]+/).filter(Boolean));
// Functions declared at top level of classic scripts are globals already.
const re = /<script(?![^>]*src=)[^>]*>([\s\S]*?)<\/script>/g;
let m;
while ((m = re.exec(s))) {
  if (/^\s*import /m.test(m[1])) continue;
  for (const f of m[1].matchAll(/^\s{0,2}(?:async\s+)?function\s+([A-Za-z_$][\w$]*)/gm)) reg.add(f[1]);
}
const used = new Set();
for (const h of s.matchAll(/data-(?:click|change|input|keydown|fn|close)=\\?"([A-Za-z_$][\w$]*)\\?"/g)) used.add(h[1]);
const missing = [...used].filter(n => !n.startsWith("__") && n !== "fn" && !reg.has(n)); // "fn" is the dispatcher comment
if (missing.length) { console.error("index.html: handlers not exposed on window: " + missing.join(", ")); process.exit(1); }
console.log(`index.html handlers: ${used.size} names, all exposed`);
