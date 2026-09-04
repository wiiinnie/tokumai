# OpenAI as a provider

Built 2026-09-03 (`server/src/openai.rs`). Verified against developers.openai.com the same
day: Responses API shapes, moderation endpoint, prices.

## What is different from Gemini

| | Gemini | OpenAI |
|---|---|---|
| free tier | yes (operator key's daily allowance → `free-tier` models, reduced price) | none — every token paid, account **prepaid** (set a monthly budget in the OpenAI dashboard) |
| API | native generateContent | Responses API (`/v1/responses`); one adapter, no Chat Completions |
| reasoning | `thinkingBudget` tokens | `reasoning.effort` low/medium/high, mapped from the app's slider (≤1k → low, ≤8k → medium, else high); reasoning tokens are billed as **output** and count against `max_output_tokens` (= maxTokens + budget) |
| latency | seconds | reasoning answers can take 1–2 min → catalog `timeoutMs` 180 000, the app waits that long for these models |
| caching | explicit | automatic; `input_tokens_details.cached_tokens` billed at `cached_in` |
| web search | grounding, 5 000 free queries/month, then $14/1k | `web_search` tool, **every call billed** ($10/1k for our reasoning models; `SCRAI_OPENAI_SEARCH_USD`), reserved at 10 calls/turn like grounding |
| training | paid tier: no | API: no (business terms) |
| retention | brief abuse logging | ~30 days abuse monitoring unless the org has Zero Data Retention → badge shows `retentionDays` (`SCRAI_OPENAI_RETENTION_DAYS`) |
| storage | — | `store: false` on every request (nothing kept in OpenAI's response store) |
| attachments | inlineData (images, PDFs, text) | `input_image` (images), `input_file` (PDF); other types are named but not sent |
| model list | live from the provider ∩ pricing.json | fixed allowlist (`OPENAI_MODELS` in catalog.rs) ∩ pricing.json — `/v1/models` mixes in embeddings/TTS/fine-tunes |
| concurrency | `SCRAI_MAX_INFLIGHT_CHATS` | own pool `SCRAI_MAX_INFLIGHT_OPENAI` (rate limits are per org tier; get the account to tier ≥ 2 before launch) |
| decline text | "Declined by Google (…)" | "Declined by OpenAI (policy / content filter / moderation: …)" |

## Anonymous users behind one org key

OpenAI expects platforms to (a) send a per-end-user `safety_identifier`, (b) run their own
safeguards, (c) respond to abuse reports. What we do:

- **Identifier**: `sha256(salt | UTC day | session id)[..24]` — stable within a day so OpenAI
  can act on ONE user's abuse instead of throttling our whole key; different tomorrow; never
  the raw session id, never an account. Salt from `SCRAI_ABUSE_SALT` (else random per boot).
- **Prefilter**: `MODERATION_PREFILTER=1` runs the free `omni-moderation-latest` on the
  user's latest turn (text + images) before the model call; flagged input is declined by us.
  Only OpenAI-routed chats — the prompt goes to OpenAI anyway, so no extra data flow. A
  moderation outage fails open.
- **Strikes**: every "Declined by …" (any provider, or the prefilter) is a strike against
  the session; at `ABUSE_STRIKES_PER_DAY` (default 3) the session is refused until the next
  UTC day. Its balance stays (unusable for the day). That is the only sanction an
  anonymous, unlinkable session can carry: the user can rotate sessions with their
  remaining coins — the cost of an offense is the rest of the current $1 chunk and a day.
- **On an abuse report** we can answer: identifier X on day Y → session paused, strike
  count N, balance stranded; no person identifiable by design (docs/abuse-policy.md).

## The privacy page names the providers — keep it in sync

`server/site/privacy.html` (§3 and §5) names every model provider that receives request
content. That is a GDPR duty (Art. 13 (1) (e)), not documentation: it has to be right
**before** a provider serves its first request, and the page is also one of the ones
Mollie reviews. It names Google and OpenAI today.

**Adding or removing any provider means editing that page in the same change** — in both
worktrees, since main and tokumai each ship a site — plus `VENDOR_TERMS` in
`public/index.html`. The file carries a checklist comment above §3.

## Not yet

Image generation (`gpt-image-*`, per-token image billing like Nano Banana — needs the
Responses `image_generation` tool + our chunked picture path), audio, structured output.
