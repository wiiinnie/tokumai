// ---------------------------------------------------------------------------
// smoke-ui.mjs — open the app in a real browser and click through the screens that
// only exist at runtime.
//
// `node --check` proves the file parses; it says nothing about a function that reads a
// variable somebody deleted. That is exactly how "Account & recovery" stopped opening on
// 2026-09-15: the session cut removed the redeem state, three lines still read it, and
// every check in the repo stayed green.
//
// Run: node scripts/smoke-ui.mjs        (needs Chrome; nothing is installed)
// ---------------------------------------------------------------------------
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, resolve } from "node:path";
import puppeteer from "puppeteer-core";

const ROOT = resolve(import.meta.dirname, "..", "public");
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".svg": "image/svg+xml" };

const server = createServer(async (req, res) => {
  // "/" has no extension, and serving the app as application/octet-stream makes Chrome
  // download it instead of rendering it — the navigation then fails as ERR_ABORTED.
  const path = req.url.split("?")[0] === "/" ? "/index.html" : req.url.split("?")[0];
  try {
    const body = await readFile(join(ROOT, path));
    res.writeHead(200, { "content-type": TYPES[extname(path)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("no");
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = `http://127.0.0.1:${server.address().port}/`;

const browser = await puppeteer.launch({ executablePath: CHROME, headless: "new", args: ["--no-sandbox"] });
const page = await browser.newPage();

// Every uncaught error and console error is a failure — the point of the exercise.
const problems = [];
page.on("pageerror", (e) => problems.push(`uncaught: ${e.message}`));
page.on("console", (m) => {
  // A missing icon or model file is not what this test is about; an exception in the
  // app's own code is.
  const t = m.text();
  if (m.type() === "error" && !t.includes("Failed to load resource")) problems.push(`console: ${t}`);
});

// networkidle0 waits for requests this page never makes (no backend here); "load" is the
// event that actually fires, and the app boots from it.
// Chrome reports ERR_ABORTED for this page even as it loads it fine (the app rewrites
// the document early). What matters is whether the app booted, which the next line asks.
await page.goto(url, { waitUntil: "load", timeout: 30_000 }).catch(() => {});
await new Promise((r) => setTimeout(r, 800));
const booted = await page.evaluate(() => typeof window.openAccount === "function").catch(() => false);
if (!booted) {
  const seen = await page.evaluate(() => ({
    url: location.href,
    title: document.title,
    scripts: document.querySelectorAll("script").length,
    hasApp: !!document.getElementById("main"),
    keys: Object.keys(window).filter((k) => /open|render|chat/i.test(k)).slice(0, 12),
  })).catch((e) => ({ err: String(e) }));
  console.error("  page:", JSON.stringify(seen));
}
if (!booted) {
  console.error("smoke-ui: the app did not boot — nothing else can be said");
  for (const p of problems) console.error(`  ${p}`);
  await browser.close();
  server.close();
  process.exit(1);
}

// The dev bridge has no backend behind it here, so calls reject; what is being tested is
// that the screens RENDER — a missing variable throws before any of that matters.
const screens = [
  ["account", "openAccount"],
  ["buy credit", "openBuy"],
  ["settings", "openSettings"],
];
for (const [name, fn] of screens) {
  problems.length = problems.length; // keep prior errors
  const before = problems.length;
  await page.evaluate((f) => {
    try {
      window[f]();
    } catch (e) {
      throw new Error(`${f} threw: ${e && e.message}`);
    }
  }, fn).catch((e) => problems.push(`${name}: ${e.message}`));
  await new Promise((r) => setTimeout(r, 250));
  const opened = await page.evaluate(() => !!document.querySelector(".scrim.open"));
  if (!opened) problems.push(`${name}: nothing opened`);
  // Close it the way the app closes it: the close path resets state of its own, and that
  // is where a variable somebody deleted hides just as well as in the open path.
  await page
    .evaluate((f) => {
      const close = { openAccount: "closeAccount", openBuy: "closeBuy", openSettings: "closeSettings" }[f];
      if (typeof window[close] === "function") window[close]();
      else document.querySelectorAll(".scrim.open").forEach((s) => s.classList.remove("open"));
    }, fn)
    .catch((e) => problems.push(`${name} close: ${e.message}`));
  await new Promise((r) => setTimeout(r, 120));
  console.log(problems.length > before ? `  ✗ ${name}` : `  ✓ ${name}`);
}

// The account modal's sub-pages: each is a render function of its own.
const pages = ["server", "phrase", "restore", "giveback", "migrate", "danger"];
await page.evaluate(() => window.openAccount());
for (const p of pages) {
  const before = problems.length;
  await page
    .evaluate((name) => {
      const el = document.createElement("div");
      el.dataset.page = name;
      window.acctNav(el);
    }, p)
    .catch((e) => problems.push(`page ${p}: ${e.message}`));
  await new Promise((r) => setTimeout(r, 120));
  console.log(problems.length > before ? `  ✗ account → ${p}` : `  ✓ account → ${p}`);
}

await browser.close();
server.close();

if (problems.length) {
  console.error(`\nsmoke-ui: ${problems.length} problem(s)`);
  for (const p of problems) console.error(`  ${p}`);
  process.exit(1);
}
console.log("\nsmoke-ui: every screen rendered");
