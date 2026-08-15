// Tiny static file server for the Tauri dev frontend.
//
// Tauri v2 does not serve a static frontendDist itself in `tauri dev` — it needs
// a devUrl. This serves public/ with module-friendly MIME types so the webview
// loads the UI. It is DEV-ONLY tooling (no app logic; the app talks to the Rust
// core via invoke). In `tauri build` the assets are embedded via custom-protocol
// and this server is not involved.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname } from "node:path";

const ROOT = new URL("../public/", import.meta.url);
const PORT = Number(process.env.TAURI_FRONTEND_PORT ?? 1421);

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
};

createServer(async (req, res) => {
  let path = decodeURIComponent((req.url || "/").split("?")[0]);
  if (path === "/" || path.endsWith("/")) path += "index.html";
  // Confine to public/: strip leading slashes and any traversal.
  const rel = path.replace(/^\/+/, "").replace(/\.\.(\/|\\|$)/g, "");
  try {
    const buf = await readFile(new URL(rel, ROOT));
    res.writeHead(200, { "content-type": MIME[extname(path)] || "application/octet-stream", "cache-control": "no-store" });
    res.end(buf);
  } catch {
    res.writeHead(404).end();
  }
}).listen(PORT, "127.0.0.1", () => console.log(`[tauri-frontend] serving public/ on http://127.0.0.1:${PORT}`));
