// ---------------------------------------------------------------------------
// handover.md — the portable conversation format.
//
// A conversation is serialized to human-readable Markdown with a small YAML
// frontmatter header, so it can be archived, read, or moved between devices.
// serialize() and parse() are exact inverses for anything this app writes.
//
// This module is pure (no storage, no crypto, no DOM) so it runs identically in
// a browser, Tauri, or a Capacitor WebView — the mobile-first seam is that
// nothing here assumes a platform.
// ---------------------------------------------------------------------------

const SCHEMA = "scrambleai/handover@1";

/**
 * @param {{id?:string,title?:string,model?:string,created?:string,messages:Array<{role:string,text:string}>}} session
 * @returns {string} markdown
 */
export function serialize(session) {
  const created = session.created || new Date().toISOString();
  const fm = [
    "---",
    `schema: ${SCHEMA}`,
    `created: ${created}`,
    `model: ${session.model || "unknown"}`,
    session.title ? `title: ${escapeYaml(session.title)}` : null,
    "---",
    "",
  ]
    .filter((l) => l !== null)
    .join("\n");

  const body = session.messages
    .filter((m) => m.role === "you" || m.role === "ai")
    .map((m) => (m.role === "you" ? "## user\n\n" : "## assistant\n\n") + m.text.trim())
    .join("\n\n");

  return fm + body + "\n";
}

/**
 * @param {string} md
 * @returns {{title?:string,model?:string,created?:string,messages:Array<{role:string,text:string}>}}
 */
export function parse(md) {
  const text = md.replace(/\r\n/g, "\n");
  let meta = {};
  let bodyStart = 0;

  if (text.startsWith("---\n")) {
    const end = text.indexOf("\n---", 4);
    if (end !== -1) {
      const fm = text.slice(4, end);
      for (const line of fm.split("\n")) {
        const idx = line.indexOf(":");
        if (idx === -1) continue;
        const k = line.slice(0, idx).trim();
        const v = line.slice(idx + 1).trim();
        meta[k] = v;
      }
      bodyStart = end + 4; // past "\n---"
      // skip the newline right after the closing fence
      while (text[bodyStart] === "\n") bodyStart++;
    }
  }

  const body = text.slice(bodyStart);
  const messages = [];
  // split on header lines that are exactly "## user" / "## assistant"
  const re = /^## (user|assistant)\s*$/gm;
  let m;
  const marks = [];
  while ((m = re.exec(body)) !== null) marks.push({ role: m[1], start: m.index, headerEnd: re.lastIndex });
  for (let i = 0; i < marks.length; i++) {
    const cur = marks[i];
    const next = marks[i + 1];
    const chunk = body.slice(cur.headerEnd, next ? next.start : body.length).trim();
    if (chunk) messages.push({ role: cur.role === "user" ? "you" : "ai", text: chunk });
  }

  return {
    title: meta.title ? unescapeYaml(meta.title) : undefined,
    model: meta.model,
    created: meta.created,
    messages,
  };
}

// minimal YAML value escaping for the title line (quotes + colons)
function escapeYaml(s) {
  return /[:#"\n]/.test(s) ? JSON.stringify(s) : s;
}
function unescapeYaml(s) {
  if (s.startsWith('"') && s.endsWith('"')) {
    try { return JSON.parse(s); } catch { return s; }
  }
  return s;
}

export const Handover = { serialize, parse, SCHEMA };
