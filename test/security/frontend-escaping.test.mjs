// ---------------------------------------------------------------------------
// Regressionstests für H1 + H-fe-1 (docs/security/audit-2026-08-20.md).
// Repliziert die Escaping-Helfer aus public/index.html (esc/escAttr/safeHttpUrl)
// und den Handover-Preview-Pfad. GRÜN = sicher; bricht ein Fix, schlägt der Test um.
//   node test/security/frontend-escaping.test.mjs
// ---------------------------------------------------------------------------
import assert from "node:assert";

// --- verbatim aus public/index.html (bei Fix dort mit-anpassen) --------------
const esc = s => (s||"").replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));
const escAttr = s => (s||"").replace(/[&<>"'`]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;','`':'&#96;'}[c]));
const safeHttpUrl = u => /^https?:\/\//i.test(u||"") ? u : "#";
// ----------------------------------------------------------------------------

let failed = 0;
const check = (name, fn) => { try { fn(); console.log("✅ " + name); } catch (e) { failed++; console.log("❌ " + name + " — " + e.message); } };

// H-fe-1: escAttr must neutralise a double-quoted attribute breakout — the payload's
// own `"` can no longer close the attribute, so the trailing onerror stays INSIDE the
// value (inert) instead of becoming a live attribute.
check("escAttr blocks the double-quote attribute breakout", () => {
  const evil = `x" onerror="__TAURI__.core.invoke('account_reveal')`;
  const encoded = escAttr(evil);
  assert.ok(!encoded.includes('"'), "no raw double-quote may survive to close the attribute");
  assert.ok(encoded.includes("&quot;"), "the quote must be entity-encoded");
});

// H-fe-1: escAttr also protects a single-quoted attribute context.
check("escAttr blocks the single-quote breakout", () => {
  const evil = `x' onmouseover='alert(1)`;
  const encoded = escAttr(evil);
  assert.ok(!encoded.includes("'"), "no raw single-quote may survive to close the attribute");
  assert.ok(encoded.includes("&#39;"), "the single-quote must be entity-encoded");
});

// H-fe-1: only http(s) URLs may become an href.
check("safeHttpUrl blocks javascript:/data: link injection", () => {
  assert.equal(safeHttpUrl("javascript:alert(1)"), "#");
  assert.equal(safeHttpUrl("data:text/html,<script>x</script>"), "#");
  assert.equal(safeHttpUrl("https://btcpay.example/i/abc"), "https://btcpay.example/i/abc");
});

// H1: the handover preview escapes raw model text BEFORE the cosmetic highlights.
check("handover preview renders injected model HTML as inert text", () => {
  const md = `schema: handover\n---\nhi <img src=x onerror=steal()>\n`;
  const html = esc(md)
    .replace(/^---$/gm,'<span class="kc">---</span>')
    .replace(/^(schema|created|model|title):/gm,'<span class="k">$1</span>:');
  assert.ok(!/<img/.test(html), "an <img> in the answer must not render as a live tag");
  assert.ok(html.includes("&lt;img"), "it must be escaped to inert text");
  assert.ok(html.includes('<span class="k">schema</span>:'), "cosmetic highlight still works");
});

console.log(failed ? `\n${failed} FAILED` : "\nall passed");
process.exit(failed ? 1 : 0);
