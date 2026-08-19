import re, os

sp = os.path.dirname(os.path.abspath(__file__))

def svg(lang):
    L = {
        "en": dict(core="shared Rust core: Coconut (BLS12-381) · billing · auth · sessions — one code for app and server",
                   app=["App (Tauri 2)", "Rust core · Nym SDK", "guard · OCR · GLiNER", "wallet: Coconut books"],
                   mix="Nym mixnet · Sphinx · SURBs",
                   srv=["scrai-server (Rust)", "reachable only via mixnet", "Coconut authority · paywall", "reserve → settle per chat"],
                   f1="sees: the content, never the route", f2="sees: the question, never the sender", f3="see: the server"),
        "de": dict(core="geteilter Rust-Kern: Coconut (BLS12-381) · Billing · Auth · Sessions — ein Code für App und Server",
                   app=["App (Tauri 2)", "Rust-Kern · Nym SDK", "Guard · OCR · GLiNER", "Wallet: Coconut-Books"],
                   mix="Nym-Mixnet · Sphinx · SURBs",
                   srv=["scrai-server (Rust)", "nur übers Mixnet erreichbar", "Coconut-Authority · Paywall", "Reserve → Settle je Chat"],
                   f1="sieht: den Inhalt, nie die Route", f2="sieht: die Frage, nie den Absender", f3="sehen: den Server"),
    }[lang]
    return f'''<svg class="dg" viewBox="0 0 920 224" role="img" aria-label="Architecture">
      <rect class="bx" x="8" y="8" width="702" height="32" rx="8"/>
      <text class="lbl" x="24" y="29">scrai-core</text>
      <text class="sub" x="116" y="28">{L["core"]}</text>
      <rect class="bx bxd" x="8" y="64" width="180" height="96" rx="10"/>
      <text class="lbld" x="24" y="92">{L["app"][0]}</text>
      <text class="subd" x="24" y="112">{L["app"][1]}</text>
      <text class="subd" x="24" y="128">{L["app"][2]}</text>
      <text class="subd" x="24" y="144">{L["app"][3]}</text>
      <g>
        <line class="edge" x1="188" y1="112" x2="242" y2="112"/><circle class="dot" cx="250" cy="112" r="5"/>
        <line class="edge" x1="258" y1="112" x2="292" y2="112"/><circle class="dot" cx="300" cy="112" r="5"/>
        <line class="edge" x1="308" y1="112" x2="342" y2="112"/><circle class="dot" cx="350" cy="112" r="5"/>
        <line class="edge" x1="358" y1="112" x2="392" y2="112"/><circle class="dot" cx="400" cy="112" r="5"/>
        <line class="edge" x1="408" y1="112" x2="442" y2="112"/><circle class="dot" cx="450" cy="112" r="5"/>
        <line class="edge" x1="458" y1="112" x2="510" y2="112"/>
        <text class="note" x="236" y="92">entry</text>
        <text class="note" x="308" y="92">mix · mix · mix</text>
        <text class="note" x="438" y="92">exit</text>
        <text class="sub" x="240" y="140">{L["mix"]}</text>
      </g>
      <rect class="bx bxd" x="510" y="64" width="200" height="96" rx="10"/>
      <text class="lbld" x="526" y="92">{L["srv"][0]}</text>
      <text class="subd" x="526" y="112">{L["srv"][1]}</text>
      <text class="subd" x="526" y="128">{L["srv"][2]}</text>
      <text class="subd" x="526" y="144">{L["srv"][3]}</text>
      <line class="edge" x1="710" y1="96" x2="768" y2="82"/>
      <line class="edge" x1="710" y1="128" x2="768" y2="142"/>
      <rect class="bx" x="770" y="62" width="140" height="42" rx="8"/>
      <text class="lbl" x="786" y="88">Gemini</text>
      <rect class="bx" x="770" y="118" width="140" height="42" rx="8"/>
      <text class="lbl" x="786" y="144">Groq</text>
      <text class="sub" x="10" y="196">{L["f1"]}</text>
      <text class="sub" x="512" y="196">{L["f2"]}</text>
      <text class="sub" x="770" y="196">{L["f3"]}</text>
    </svg>'''

html = open(os.path.join(sp, "template.html")).read()
html = html.replace("{{SVG_DIAGRAM_EN}}", svg("en")).replace("{{SVG_DIAGRAM_DE}}", svg("de"))
html = re.sub(r"\{\{FONT:(\w+)\}\}", lambda m: open(os.path.join(sp, "fonts", m.group(1) + ".b64")).read().strip(), html)
html = re.sub(r"\{\{IMG:([\w-]+)\}\}", lambda m: open(os.path.join(sp, "web", m.group(1) + ".b64")).read().strip(), html)
missing = re.findall(r"\{\{[^}]*\}\}", html)
open(os.path.join(sp, "scrambleai-pitch.html"), "w").write(html)
print("bytes:", len(html), "missing:", missing)
