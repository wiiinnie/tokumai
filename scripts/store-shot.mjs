// ---------------------------------------------------------------------------
// store-shot.mjs — screenshots of the app's own screens, at phone size, for the
// App Store.
//
// App Store Connect wants a "review screenshot" per in-app purchase: a picture of where
// in the app the thing is bought. Taking it off a device is a chicken-and-egg problem —
// StoreKit hands the app no products until the products are Ready to Submit, and they
// cannot be submitted without the screenshot.
//
// So this renders the REAL app — the same index.html that ships, the same CSS, the same
// render functions — in Chrome at iPhone size, with the backend stubbed out. What it is
// NOT is a mockup: every pixel comes from the code under review. Only the numbers behind
// it are supplied here, because on a device they would come from StoreKit.
//
// Run:  node scripts/store-shot.mjs [outdir]        (needs Chrome; nothing is installed)
// Out:  plans-monthly.png, plans-yearly.png         1170 x 2532 (390 x 844 @3x)
// ---------------------------------------------------------------------------
import { createServer } from "node:http";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { extname, join, resolve } from "node:path";
import puppeteer from "puppeteer-core";

const ROOT = resolve(import.meta.dirname, "..", "public");
const OUT = resolve(process.argv[2] || join(import.meta.dirname, "..", "dist", "store-shots"));
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".svg": "image/svg+xml" };

// The prices are Apple's, as set in App Store Connect. Kept here rather than invented in
// the page so this file is the one place to correct when a price point moves.
const PRICES = {
  "com.tokumai.app.plan.10": ["€10.00", "10.00"],
  "com.tokumai.app.plan.20": ["€20.00", "20.00"],
  "com.tokumai.app.plan.50": ["€50.00", "50.00"],
  "com.tokumai.app.plan.10.year": ["€114.99", "114.99"],
  "com.tokumai.app.plan.20.year": ["€224.99", "224.99"],
  "com.tokumai.app.plan.50.year": ["€549.99", "549.99"],
};

const server = createServer(async (req, res) => {
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
await page.setViewport({ width: 390, height: 844, deviceScaleFactor: 3, isMobile: true, hasTouch: true });
await page.setUserAgent(
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148",
);

// The app decides it is the iPhone build by finding Tauri's invoke bridge and an iPhone
// user agent (index.html: isIOSNative). Standing in for the bridge is what makes the iOS
// code paths — the App Store sheet among them — the ones that render.
await page.evaluateOnNewDocument((prices) => {
  const plans = Object.keys(prices);
  const state = {
    appVersion: "0.6.6",
    serverVersion: "0.6.6",
    account: { fingerprint: "a4f2 91c8 77de", phraseVerified: true },
    balance: 0,
    held: 0,
    entitlement: 0,
    plan: null,
    bookToku: 100000,
    coinToku: 100,
    tiers: [5, 10, 20, 50],
    testnet: false,
    // Rates as the SERVER publishes them: TOKU per million tokens, margin already in
    // (server/src/catalog.rs → billing::retail). Feeding dollars in here instead would
    // make the "answers a month" line on each tier read three times too generous.
    models: [
      { id: "gemini-3.1-flash-lite", label: "Gemini 3.1 Flash-Lite", kind: "text", rate: { in: 32500, out: 195000 } },
      { id: "gemini-3.5-flash", label: "Gemini 3.5 Flash", kind: "text", rate: { in: 195000, out: 1170000 } },
      { id: "gemini-3.1-flash-image", label: "Nano Banana 2", kind: "image", rate: { in: 65000, image: 7800000 } },
    ],
    server: "4LjM6dbZ9Pu4hS7hg3AHAK4YLkRfsdH8U5Bi1E9CPBA7.BLpmR82Up6HiucSBLGJQRys6ZHVNZF2doZLPoZFSwTpC@38zcSsvjXsAX7C28ko2H3Lt55X4TYxfZYkPADxKXZHUj",
    iapProducts: [],
    iapPlans: plans,
    plans: [
      { tier: 0, toku: 700000, cents: 1000, yearlyCents: 10800, savesCents: 0 },
      { tier: 1, toku: 1500000, cents: 2000, yearlyCents: 21600, savesCents: 142 },
      { tier: 2, toku: 4000000, cents: 5000, yearlyCents: 54000, savesCents: 714 },
    ],
  };
  const answers = {
    state,
    localState: state,
    iap_plans: {
      products: plans.map((id) => ({ id, displayName: id, displayPrice: prices[id][0], price: prices[id][1] })),
      ladder: state.plans,
    },
    iap_products: { products: [] },
    mixnetStatus: { connected: true },
  };
  window.__TAURI_INTERNALS__ = {
    invoke: async (cmd) => {
      if (cmd in answers) return answers[cmd];
      // Anything not answered here is something this screen does not need. Returning null
      // rather than throwing keeps a stray call from aborting the render.
      return null;
    },
  };
}, PRICES);

await page.goto(url, { waitUntil: "load", timeout: 30_000 }).catch(() => {});
await new Promise((r) => setTimeout(r, 1200));

const booted = await page.evaluate(() => typeof window.openPlans === "function").catch(() => false);
if (!booted) {
  console.error("store-shot: the app did not boot — nothing to photograph");
  await browser.close();
  server.close();
  process.exit(1);
}

await mkdir(OUT, { recursive: true });
for (const [name, yearly] of [["plans-monthly", false], ["plans-yearly", true]]) {
  await page.evaluate((y) => {
    window.openPlans();
    window.planPeriod(y);
  }, yearly);
  // The sheet loads its prices through the stub, then renders; a beat is enough.
  await new Promise((r) => setTimeout(r, 700));
  const shown = await page.evaluate(() => {
    const t = document.querySelectorAll("#planBody .plantier").length;
    return { tiers: t, text: (document.getElementById("planBody") || {}).innerText || "" };
  });
  if (!shown.tiers) {
    console.error(`store-shot: ${name} — no plans rendered. The sheet said:\n${shown.text.slice(0, 300)}`);
    await browser.close();
    server.close();
    process.exit(1);
  }
  const file = join(OUT, `${name}.png`);
  await writeFile(file, await page.screenshot({ type: "png" }));
  console.log(`  ✓ ${file}  (${shown.tiers} plans)`);
  await page.evaluate(() => window.closePlans());
  await new Promise((r) => setTimeout(r, 200));
}

await browser.close();
server.close();
console.log("store-shot: done");
