# Pitch build (EN/DE web artifact + PDFs)

- `template.html` — single source: both languages, murmur design tokens, lightbox w/ zoom.
  Placeholders `{{FONT:*}}` / `{{IMG:*}}` / `{{SVG_DIAGRAM_*}}` are filled by build.py.
- `build.py` — base64-embeds assets/ (screenshots + woff2 fonts), generates the two
  architecture SVGs, writes `scrambleai-pitch.html` (publish this as the artifact).
- `build_pdf.py` — derives one print HTML per language (dark, A4 borderless, language
  chip on the cover as marker, chapter-per-page), then render with:
  chrome --headless=new --no-pdf-header-footer --virtual-time-budget=15000 --print-to-pdf=...
- Screenshots are real captures of the dev UI (localhost:8787); regenerate the .b64
  files with `base64 -i x.jpg -o x.b64` next to the jpg before building.
- Published artifact (keep this URL): https://claude.ai/code/artifact/e3b7b92c-55f0-44b1-b72f-0732b0ac6595
