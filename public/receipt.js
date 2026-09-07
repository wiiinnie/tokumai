// receipt.js — the purchase receipt, as a PDF, built on the device.
//
// Why by hand and not with a library: a receipt is a dozen lines of text in one font on
// one page. Every PDF library we could pull in is 50–300 kB of code that would have to be
// vendored into the app bundle (the webview loads nothing from a CDN, on purpose) to do
// what 60 lines of string building do here. The output is a PDF 1.4 file with one page,
// Helvetica and Helvetica-Bold, and a WinAnsi text encoding.
//
// Nothing here talks to the server. The receipt is assembled from the invoice the app
// already holds, so no document tied to a purchase is ever built on our side — which is
// the point: the receipt exists to be handed to the buyer, not to be kept by us.

// PDF text strings are latin-1 with three escapes. Anything outside that range (an em
// dash, a €) would be mojibake, so it is transliterated rather than silently mangled.
const SUBST = { "—": "-", "–": "-", "·": "-", "„": '"', "“": '"', "”": '"', "‚": "'", "‘": "'", "’": "'", "€": "EUR", "→": "->", "✓": "x" };
function pdfText(s) {
  let out = "";
  for (const ch of String(s == null ? "" : s)) {
    const c = SUBST[ch] !== undefined ? SUBST[ch] : ch;
    for (const d of c) {
      const code = d.codePointAt(0);
      if (d === "\\" || d === "(" || d === ")") out += "\\" + d;
      else if (code >= 32 && code <= 255) out += d;
      else out += "?";                       // outside WinAnsi: a visible placeholder, never garbage
    }
  }
  return out;
}

const PT = { left: 56, right: 539, top: 786 };   // A4 is 595 x 842 pt

/// One line of the content stream. `size` and `bold` pick the font.
function line(y, text, { size = 10.5, bold = false, x = PT.left } = {}) {
  return `BT /${bold ? "F2" : "F1"} ${size} Tf 1 0 0 1 ${x} ${y} Tm (${pdfText(text)}) Tj ET\n`;
}
/// Right-aligned, using Helvetica's own widths would be overkill — 0.5 em is close enough
/// for the value column and never overlaps the label at these lengths.
function lineRight(y, text, { size = 10.5, bold = false } = {}) {
  const w = String(text).length * size * 0.5;
  return line(y, text, { size, bold, x: Math.max(PT.left + 170, PT.right - w) });
}
function rule(y) {
  return `0.85 G 0.6 w ${PT.left} ${y} m ${PT.right} ${y} l S\n`;
}

/// `data` is what the app knows about one settled purchase — see buildReceipt() in the app.
/// Returns a Uint8Array holding a complete PDF file.
export function receiptPdf(d) {
  let y = PT.top;
  let s = "0 g\n";

  s += line(y, "tokumai", { size: 20, bold: true });
  y -= 16;
  s += line(y, "anonymous AI over the Nym mixnet", { size: 9 });
  y -= 26;
  s += line(y, "Receipt", { size: 15, bold: true });
  y -= 8;
  s += rule(y);
  y -= 22;

  const rows = [
    ["Receipt number", d.number],
    ["Date", d.date],
    ["Item", d.item],
    ["Amount paid", d.amount],
    ["Payment method", d.method],
    ["VAT", d.vat],
  ];
  for (const [k, v] of rows) {
    s += line(y, k, { size: 10.5 });
    s += lineRight(y, v, { size: 10.5, bold: true });
    y -= 17;
  }

  y -= 10;
  s += rule(y);
  y -= 22;
  s += line(y, "Seller", { size: 11, bold: true });
  y -= 16;
  for (const l of d.seller) { s += line(y, l, { size: 10 }); y -= 13; }

  y -= 14;
  s += line(y, "Right of withdrawal", { size: 11, bold: true });
  y -= 16;
  for (const l of d.withdrawal) { s += line(y, l, { size: 9.5 }); y -= 12.5; }

  y -= 12;
  s += line(y, "What you confirmed at checkout", { size: 11, bold: true });
  y -= 16;
  for (const l of d.consent) { s += line(y, l, { size: 9.5 }); y -= 12.5; }

  y -= 14;
  s += rule(y);
  y -= 16;
  for (const l of d.footer) { s += line(y, l, { size: 8.5 }); y -= 11; }

  return assemble(s);
}

/// Wrap a content stream into a one-page PDF with a proper xref table.
function assemble(content) {
  const objs = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] " +
      "/Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>",
    null, // the content stream, built below
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>",
  ];

  // WinAnsi is a ONE-byte encoding: a TextEncoder would write "ü" as two UTF-8 bytes and
  // the reader would render "Ã¼". pdfText() has already reduced everything to code points
  // <= 255, so the low byte IS the character.
  const enc = (str) => {
    const b = new Uint8Array(str.length);
    for (let i = 0; i < str.length; i++) b[i] = str.charCodeAt(i) & 0xff;
    return b;
  };
  const bytes = [];                       // chunks, concatenated at the end
  let len = 0;
  const push = (str) => { const b = enc(str); bytes.push(b); len += b.length; return b.length; };

  push("%PDF-1.4\n");
  const offsets = [0];
  for (let i = 0; i < objs.length; i++) {
    offsets.push(len);
    if (i === 3) {
      const body = enc(content);
      push(`4 0 obj\n<< /Length ${body.length} >>\nstream\n`);
      bytes.push(body); len += body.length;
      push("\nendstream\nendobj\n");
    } else {
      push(`${i + 1} 0 obj\n${objs[i]}\nendobj\n`);
    }
  }
  const xref = len;
  let x = `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
  for (let i = 1; i <= objs.length; i++) x += String(offsets[i]).padStart(10, "0") + " 00000 n \n";
  push(x);
  push(`trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`);

  const out = new Uint8Array(len);
  let at = 0;
  for (const b of bytes) { out.set(b, at); at += b.length; }
  return out;
}

/// Base64 for the bytes, because the bridge to the native save takes a string.
export function toBase64(bytes) {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(s);
}
