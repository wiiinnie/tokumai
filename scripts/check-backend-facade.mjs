// Every method the dev (HTTP) and Tauri tables in public/backend.js define must also be
// exported through the `Backend` facade at the bottom — otherwise the call site gets
// `undefined`, the catch() swallows the TypeError, and a feature silently disappears on
// the phone (2026-09-10: "Phrase in Keychain" and the three-word check on iOS).
import { readFileSync } from "node:fs";
const s = readFileSync(new URL("../public/backend.js", import.meta.url), "utf8");
const names = [...new Set([...s.matchAll(/^  ([a-zA-Z]+): \(/gm)].map(m => m[1]))];
const facade = s.slice(s.indexOf("export const Backend"));
const missing = names.filter(n => !facade.includes(`  ${n}:`));
if (missing.length) { console.error("backend.js: missing from the Backend facade: " + missing.join(", ")); process.exit(1); }
console.log(`backend.js facade: ${names.length} methods, none missing`);
