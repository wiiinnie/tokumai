# Marketing copy — USPs for website and app stores

Status: draft, 2026-09-03. Source of truth for the public positioning text.
Keep the wording claims-safe: we say "cannot be traced to you", never
"nobody reads your prompts" (the model provider does read them).

## Positioning (internal)

Two axes: *what* is asked (content) and *who* asks (identity, IP, payment).
TEE products (nilGPT, Maple) protect the *what* with open-weight models; they
still know who paid and which IP asked. Proxy products (Duck.ai, Brave Leo,
Venice) promise to strip the IP but see it. tokumai protects the *who* by
design: no identity, no IP at the server (Nym mixnet), unlinkable blind-ecash
payments, hence no profile of a real person. Content is visible to us and the
upstream provider; say so plainly.

One-liner: *Others hide what you say. We make sure it can never be traced back
to you.*

Wording rules:
- "no real-world identity", not "no account" (a seed-derived account exists).
- "no profile of you as a real person", not "prompts cannot be linked"
  (a session counter exists server-side).
- App Store metadata: "leading AI models", no third-party trademarks.
  Website may name Gemini and GPT.

---

## Website (EN)

**Frontier AI. No identity attached.**

Ask Gemini or GPT anything. Nobody can tie the question to you. Not the model,
not the network, not even us.

**No real-world identity.** No email, no phone number, no login. Your account
is a recovery phrase that only you hold.

**Unlinkable payments.** Top up once, then spend blind tokens. Even if you pay
by card, nobody can connect your payment to a single prompt. That includes us.

**Untraceable prompts.** Every request travels through the Nym mixnet. Our
servers never see an IP address. No prompt can be traced back to a person.

**No profile of you.** Without an identity, an IP or a traceable payment,
there is nothing to build a profile on. Your questions never add up to a
picture of you as a real person.

Others hide what you say. We make sure it can never be traced back to you, and
never adds up to a profile of you.

*The model still reads your question. It just never learns who asked.*

## App Store (EN)

Subtitle (≤30 chars): **Private AI. No identity.**

Promotional text (≤170 chars):

> Chat with leading AI models without an email, a phone number or a traceable
> payment. Every prompt travels through the Nym mixnet. No one can tie a
> question to you.

Description opening:

> tokumai gives you frontier AI without giving up who you are.
>
> • No real-world identity: no email, no phone, no login. Your account is a
>   recovery phrase only you hold.
> • Unlinkable payments: top up once, spend blind tokens. Not even we can
>   connect a payment to a prompt.
> • Untraceable prompts: every request goes through the Nym mixnet. Our
>   servers never see your IP.
> • No profile of you: with no identity, no IP and no traceable payment, your
>   questions can never be assembled into a picture of you as a real person.
>
> Others hide what you say. We make sure it can never be traced back to you.

---

## Website (DE)

**Frontier-KI. Ohne Identität.**

Frag Gemini oder GPT, was du willst. Niemand kann die Frage dir zuordnen.
Nicht das Modell, nicht das Netz, nicht einmal wir.

**Keine echte Identität.** Keine E-Mail, keine Telefonnummer, kein Login.
Dein Konto ist eine Wiederherstellungsphrase, die nur du hast.

**Unverknüpfbare Zahlung.** Einmal aufladen, dann mit blinden Tokens
bezahlen. Selbst bei Kartenzahlung kann niemand deine Zahlung mit einem
Prompt verbinden. Auch wir nicht.

**Nicht zurückverfolgbare Prompts.** Jede Anfrage läuft durch das
Nym-Mixnet. Unsere Server sehen nie eine IP-Adresse. Kein Prompt lässt sich
zu einer Person zurückverfolgen.

**Kein Profil von dir.** Ohne Identität, IP und verfolgbare Zahlung gibt es
nichts, woraus sich ein Profil bauen ließe. Deine Fragen ergeben nie ein Bild
von dir als echter Person.

Andere verstecken, was du sagst. Wir sorgen dafür, dass es nie auf dich
zurückfällt und nie zu einem Profil von dir wird.

*Das Modell liest deine Frage. Es erfährt nur nie, wer sie gestellt hat.*

## App Store (DE)

Untertitel: **Private KI. Ohne Identität.**

Promo-Text:

> Chatte mit führenden KI-Modellen ohne E-Mail, Telefonnummer oder verfolgbare
> Zahlung. Jeder Prompt läuft durchs Nym-Mixnet. Niemand kann dir eine Frage
> zuordnen.

Beschreibung:

> tokumai gibt dir Frontier-KI, ohne dass du preisgibst, wer du bist.
>
> • Keine echte Identität: keine E-Mail, kein Telefon, kein Login. Dein Konto
>   ist eine Wiederherstellungsphrase, die nur du hast.
> • Unverknüpfbare Zahlung: einmal aufladen, blinde Tokens ausgeben. Nicht
>   einmal wir können eine Zahlung mit einem Prompt verbinden.
> • Nicht zurückverfolgbare Prompts: jede Anfrage läuft durchs Nym-Mixnet.
>   Unsere Server sehen nie deine IP.
> • Kein Profil von dir: ohne Identität, IP und verfolgbare Zahlung lassen
>   sich deine Fragen nie zu einem Bild von dir als echter Person
>   zusammensetzen.
>
> Andere verstecken, was du sagst. Wir sorgen dafür, dass es nie auf dich
> zurückfällt.

---

## Competitor snapshot (2026-09-03)

| Tool | Account | IP visible to operator | Payment linkable | Content readable by operator | Frontier models |
|---|---|---|---|---|---|
| tokumai | no (seed) | never (mixnet) | no (blind ecash) | yes (proxy) + Google/OpenAI | yes |
| nilGPT | yes (mail/wallet) | yes | yes | no (TEE) | no (Gemma 27B etc.) |
| Maple AI | yes | yes | partly (Bitcoin) | no (TEE) | no |
| Venice | optional | yes | pseudonymous (on-chain) | "we don't log" | yes, via proxy |
| Duck.ai / Brave Leo | no | yes at proxy | free / card | "we don't log" | yes |
| Lumo | yes (Proton) | yes | card | storage no, inference yes | no |
| NinjaChat | yes | yes | card | yes | yes |

Hardest comparison: Duck.ai over Tor (free, no account, no IP). Our answer:
mixnet beats Tor on timing correlation, image/thinking models, native app with
vault and guard, no rate limits, unlinkable paid tier.

Possible moat: an open-model tier inside a TEE reached over the mixnet would
cover both axes at once. Nobody offers that today.

---

## App Store Connect fields, as entered 2026-09-10 (version 1.0, build 0.6.2)

- Name: tokumai · Subtitle: Private AI. No identity.
- Category: Productivity (secondary Utilities) · Copyright: 2026 Matthias Winter, Hermes Blockchain Ventures
- URLs: support https://tokumai.com/ · marketing https://tokumai.com/ · privacy https://tokumai.com/privacy
- Keywords (≤100): private,anonymous,ai,chat,assistant,privacy,mixnet,nym,untraceable,no account,chatbot,llm
- Promotional text and description: the App Store (EN) block above, description extended with
  the feature list (models, images, on-device guard and vault, prepaid credit, recovery phrase)
  and the "model still reads your question" line.
- App Privacy: data collected = User Content (prompts, for app functionality, not linked to
  identity, no tracking) and Purchases (transaction fingerprint, app functionality, not linked);
  no analytics, no identifiers, no location, no contacts, no tracking.
- Screenshots: taken on the iPhone (6.1", 1179×2556) and scaled to 6.5" 1284×2778 with sips —
  same 19.5:9 ratio, Apple accepts the scaled set.
- IAPs credit.10/20/50 go in the same submission (first IAP needs an app version). Manual release.
