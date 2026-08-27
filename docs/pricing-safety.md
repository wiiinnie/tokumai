# Pricing safety — „ein Prompt darf mich nie mehr kosten als dem User berechnet"

Ziel: für **jeden** Prompt gilt `berechnet (SCRAI) ≥ Provider-Kosten × Marge` (Marge ≥ 1,10,
im Zweifel aufgerundet). Stand 2026-08-23.

## Warum es per Konstruktion hält (Code)

`core/src/billing.rs` + `server/src/chat.rs`:
- **Abgerechnet wird auf denselben Token, auf die Google abrechnet.** `gemini_usage()` zählt
  `output = candidatesTokenCount + thoughtsTokenCount` → **Thinking-Tokens werden als Output
  berechnet** (mit Test abgesichert). Input inkl. `toolUsePromptTokenCount`; Cached fällt im
  Zweifel auf den vollen Input-Preis (überberechnet nie unter).
- **Überall `ceil` (aufrunden)**, Marge `MARGIN` (clamp ≥ 1), `MIN_CHARGE`-Floor: `price_scrai =
  ceil(cost_scrai × margin) ≥ cost_scrai × margin`. Reserve zieht die Worst-Case-Ceiling ab,
  Settle refundet nur den ungenutzten Teil.
- **Edge case geschlossen (2026-08-23):** liefert Gemini KEINE `usageMetadata`, es kam aber eine
  Antwort → Fallback auf Zeichen-Schätzung (~4 ch/Token), nie 0 berechnen während Google
  abrechnet.

Damit hängt „nie verlieren" nur noch an **zwei Stellschrauben**:
1. **`MARGIN` ≥ 1,10** (Empfehlung **1,15** als Puffer — +10 % ist dünn).
2. **`pricing.json` ≥ Googles echte Preise.** Ist die Tabelle ≥ Google, ist die berechnete
   „Kosten"-Zahl ≥ die echte Rechnung → Garantie hält.

## Zeigt AI Studio den Preis pro Prompt? Nein — aber die API schon

Der Playground hat keinen Live-Kosten-Zähler. Aber die **API-Antwort** liefert `usageMetadata`
(prompt/candidates/thoughts/cachedContent-Token) = exakt Googles Abrechnungsbasis. × Googles
veröffentlichte Preise = **exakte Kosten pro Prompt** (rechnet der Server bereits).
Google Cloud → Billing zeigt die (verzögerte) Gesamtrechnung für den Monats-Abgleich.

## Dev-Cost-Audit-Panel (Nachweis, live)

**Erreichen: Wordmark „ScrambleAI" oben links ~0,65 s gedrückt halten** → togglet das Panel
(bleibt über App-Starts erhalten, in `localStorage`). Unter jeder Antwort erscheint dann:

> ⚙ provider $0.001380 · charged $0.001518 · +10.0 % ✓

Rot + ✗, falls berechnet < Kosten × 1,10 (darf per Code nie passieren — der Live-Check beruhigt).
Daten kommen aus dem Billing-Frame der Antwort (`costScrai` + `priceScrai`), der ohnehin schon
mitgeschickt wird.

## Sinnvoller Prüf-Modus für `pricing.json` (gegen Google-Preisänderungen)

**Primär (autoritativ): monatlicher Rechnungs-Abgleich.** Der Server loggt pro Prompt die
Provider-Kosten. Summe pro Monat vs. **Google-Cloud-Rechnung**:
- Rechnung ≈ berechnete Kosten → `pricing.json` ist korrekt, Garantie bewiesen.
- Rechnung **>** berechnete Kosten → Google hat einen Preis erhöht → `pricing.json` anheben.

Das fängt **jede** Google-Preisänderung automatisch (die Rechnung lügt nicht), ohne Scraping.

**Puffer:** `MARGIN = 1.15` absorbiert eine kleine Erhöhung, bis der Monats-Abgleich sie zeigt.

**Vor dem Mainnet-Key (einmalig):** `pricing.json` gegen die aktuelle Google-Preisliste
(https://ai.google.dev/gemini-api/docs/pricing) verifizieren.

**Optional (nice-to-have):** ein Skript, das die Google-Preisseite monatlich zieht und gegen
`pricing.json` difft und bei Abweichung alarmiert. Brittle (HTML-Scraping), daher nur Ergänzung
zum Rechnungs-Abgleich, nicht Ersatz.

**Hygiene:** `pricing.json` bekommt ein `checked: "YYYY-MM"`-Feld; Review-Reminder monatlich.
