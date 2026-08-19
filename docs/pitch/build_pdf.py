import re, os

sp = os.path.dirname(os.path.abspath(__file__))
html = open(os.path.join(sp, "scrambleai-pitch.html")).read()

PRINT_CSS = """
<style>
@page{size:A4;margin:0}
@media print{
  html,body{background:var(--bg)!important}
  body{-webkit-print-color-adjust:exact;print-color-adjust:exact}
  .page{max-width:none;padding:11mm 13mm 14mm}
  .hero{margin-top:2mm;padding:40px 42px 36px}
  .hero h1{font-size:46px}
  /* the language chip stays visible as the document's language marker —
     absolute (not fixed) so it prints once, on the first page only */
  .langbar{display:flex!important;position:absolute;top:9mm;right:12mm;backdrop-filter:none;background:var(--panel-2)}
  .langbar button{cursor:default}
  #lightbox{display:none!important}
  /* every chapter starts its own page; the hero stands alone as the cover.
     .chapter.flow needs its own rule — the template's flow exception would
     otherwise win on specificity. */
  .chapter,.chapter.flow{break-before:page;margin-top:0;padding-top:9mm}
  figure.shot,.diagram,.steps li,.facts li,.kpi{break-inside:avoid}
  figure.shot .frame{box-shadow:none;cursor:default}
  figure.shot .frame:hover{transform:none}
  h3{break-after:avoid}
}
</style>
"""

def make(lang):
    out = html
    # keep only the requested language's page div (cut at the marker comments)
    if lang == "en":
        out = re.sub(r'<div class="page" id="lang-de"[\s\S]*?</div><!-- /lang-de -->', "", out)
    else:
        out = re.sub(r'<div class="page" id="lang-en"[\s\S]*?</div><!-- /lang-en -->', "", out)
        out = out.replace('<div class="page" id="lang-de" hidden>', '<div class="page" id="lang-de">')
        out = out.replace('<button id="btn-en" class="on" type="button">EN</button>',
                          '<button id="btn-en" type="button">EN</button>')
        out = out.replace('<button id="btn-de" type="button">DE</button>',
                          '<button id="btn-de" class="on" type="button">DE</button>')
    # pin the language instead of restoring it from localStorage; the missing
    # other-language div would make setLang throw, so guard it
    out = out.replace(
        'setLang((()=>{ try { return localStorage.getItem("scrai.pitch.lang") || "en"; } catch(e){ return "en"; } })());',
        f'try{{setLang("{lang}")}}catch(e){{}}')
    body = out + PRINT_CSS
    doc = (f'<!doctype html><html lang="{lang}" data-theme="dark"><head><meta charset="utf-8">'
           f'<title>ScrambleAI</title></head><body>{body}</body></html>')
    path = os.path.join(sp, f"scrambleai-pitch-print-{lang}.html")
    open(path, "w").write(doc)
    print(lang, len(doc), "bytes ->", path)

make("en")
make("de")
