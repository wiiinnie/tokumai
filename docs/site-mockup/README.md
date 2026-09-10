# Site mockups (2026-09-10)

- `clickdummy.html` — the whole site as a click-dummy with hash routes (`#/`, `#/how-it-works`,
  `#/pricing`, `#/download`, `#/compare`, `#/vs/duck-ai`), dark/light, desktop/mobile, and an
  SEO bar showing each route's title / description / keywords. Images are referenced as
  `{{IMG:name}}` → `server/site/img/name.jpg`; `build.sh` inlines them for the artifact.
- `vs-duck-ai.html` — the standalone comparison page mockup with the head/SEO notes.
- Artifacts: click-dummy https://claude.ai/code/artifact/… (see the session), comparison page
  https://claude.ai/code/artifact/22c67378-c1e5-43d2-a65c-f2ff1236d0db

Structure decided for SEO: one URL per topic instead of anchors on the home page —
`/how-it-works`, `/pricing`, `/download`, `/compare` + `/vs/<competitor>`. The home page keeps
teasers that link into them. "How it works" is rewritten around the who-axis (identity, IP,
payment, device) to match the comparison pages.
