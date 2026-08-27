# scrambleai — Angriffs-/Testszenarien (Hardening)

Reproduzierbare Tests gegen den eigenen Code, gemappt auf die Findings in
[`audit-2026-08-20.md`](./audit-2026-08-20.md). Jedes Szenario: **Setup → Schritt →
Erwartung heute → Erwartung nach Fix.** Konvention für die automatisierten Tests:
**Exit ≠ 0 / GRÜN = Lücke besteht** (CI-Alarm); nach dem Fix schlägt der Test um.

Zentrale Rolle spielt ein **Rogue-Server-Harness**: ein bösartiger scrai-server ist
im Threat-Model der Standardfall (das anonyme Bearer-Ecash existiert, damit man dem
Operator *nicht* vertrauen muss). Die meisten Client-Findings werden getriggert,
indem der Server eine manipulierte JSON-Antwort liefert.

---

> **Status:** C1, C2, C3 sind gefixt (siehe Fix-Status im Audit-Report). Die
> Szenarien A/B/D unten dienen jetzt als **Regressionstests**: sie sind grün und
> müssen es bleiben. H-Findings sind offen.

## Szenario A — Bösartiger Server stiehlt den Seed (C1, H7)  · AUTOMATISIERT · ✅ GEFIXT

**Setup:** keiner — reiner Rendering-Pfad.
**Schritt:** `node test/security/xss-image-payload.test.mjs` → jetzt **exit 0 (SAFE)**:
`im.data` wird als Base64 validiert, der Payload zu leerem Base64 neutralisiert.
Der Test speist die exakte `imgSrc`/`imagesHtml`-Logik aus `public/index.html:1018-1035`
mit einer Chat-Antwort, deren `images[].data` aus dem `src`-Attribut ausbricht:

```json
{"images":[{"mimeType":"image/png",
  "data":"x\" onerror=\"window.__TAURI__.core.invoke('account_reveal').then(exfil)\" data-x=\""}]}
```

**Erwartung heute:** Exit 1 — das erzeugte Markup enthält einen injizierten
`onerror="…__TAURI__…account_reveal…"`. Im Webview (mit `csp:null` +
`withGlobalTauri:true`) feuert der Handler und exfiltriert die 24-Wort-Mnemonic.
**Nach Fix:** `im.data` wird als Base64 validiert / `src` per DOM-Property gesetzt +
striktes CSP → kein Ausbruch → Test schlägt fehl (SAFE).

**End-to-End-Variante (manuell, höchste Aussagekraft):** siehe Rogue-Server-Harness
unten; dort liefert ein echter Nym-Service-Provider die Payload über das Mixnet an
die laufende Tauri-App. Bestätigt die volle Kette Server→Webview→nativ.

---

## Szenario B — Absturz löscht Bearer-Coins permanent (C2, H6)  · RUST-REGRESSIONSTEST · ✅ GEFIXT

**Jetzt verankert** als `cargo test -p scrambleai wallet::tests` (3 grüne Tests):
`round_trips_and_keeps_bearer_coins`, `corrupt_wallet_is_backed_up_never_silently_discarded`,
`saved_wallet_is_owner_only`. Der Fix: atomarer Write (tmp+fsync+rename) → ein
Absturz lässt das ALTE Wallet intakt statt einer abgeschnittenen Datei; Korruption
wird in `wallet.corrupt.<ts>.json` gesichert statt still verworfen; Modus `0600`.

<details><summary>Ursprüngliche Repro-Skizze (historisch)</summary>

**Setup:** `#[cfg(test)]`-Modul in `src-tauri/src/wallet.rs` (unten). Kein Angreifer.
**Schritt:** eine `wallet.json` mit gültigem Wallet + Purses schreiben, sie dann
(wie ein abgebrochener `fs::write`) **truncaten**, `load()` aufrufen.
**Erwartung heute:** `load()` schluckt den Parse-Fehler (`unwrap_or_default()`,
`wallet.rs:44`) → leeres Wallet; Mnemonic + alle `coconut_purses` weg. Ein
folgendes `save()` zementiert den Verlust.
**Nach Fix:** `load()` schlägt bei Korruption laut fehl / legt ein Backup an; der
atomare `save()` (tmp+fsync+rename) hinterlässt nie eine halbe Datei.

```rust
// in src-tauri/src/wallet.rs
#[cfg(test)]
mod hardening_tests {
    use super::*;
    use std::fs;

    #[test]
    fn truncated_wallet_must_not_silently_reset() {
        let dir = std::env::temp_dir().join(format!("scrai-wtest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // gültiges Wallet mit Bearer-Geld
        let w = Wallet { mnemonic: Some("abandon ... art".into()),
                         coconut_purses: vec![r#"{"fake":"purse-json"}"#.into()],
                         ..Default::default() };
        save(&dir, &w).unwrap();
        // simuliere einen abgebrochenen Write (Absturz/Stromausfall/volle Platte):
        let full = fs::read_to_string(wallet_path(&dir)).unwrap();
        fs::write(wallet_path(&dir), &full[..full.len()/2]).unwrap(); // truncate

        let loaded = load(&dir);
        // HEUTE grün (Lücke): loaded ist Default → assert schlägt fehl → Test ROT? Nein:
        // wir behaupten die SICHERE Eigenschaft, die es NOCH NICHT gibt:
        assert!(loaded.mnemonic.is_some() || loaded.coconut_purses.is_empty()==false,
            "SILENT DATA LOSS: truncated wallet load()-te zu einem leeren Wallet — \
             Bearer-Coins unwiederbringlich weg (Finding C2)");
        fs::remove_dir_all(&dir).ok();
    }
}
```
Ausführen: `cargo test -p scrambleai hardening_tests -- --nocapture`
(Der Test ist heute **rot** = die unsichere Eigenschaft ist noch da; nach dem
Fix wird er grün. Das ist die Ausnahme von der „grün=Lücke"-Konvention — hier
behaupten wir bewusst die Ziel-Eigenschaft.)

</details>

---

## Szenario C — Fehlgeschlagener Spend verbrennt Coins (H4)  · MANUELL + DROP-IN

**Kern:** `pay_info` wird pro Spend frisch erzeugt, aber **nie persistiert**
(`lib.rs:288-290`), obwohl `purse.rs:63-64` einen Retry mit *demselben* `pay_info`
verlangt.
**Setup:** laufender scrai-server; Client mit einer Purse (`coconut_withdraw`).
**Schritt:** einen `coconut_spend` auslösen und die Reply-Zustellung droppen
(Server nach dem Empfang, vor `send_reply`, killen — oder `TIMEOUT_MS` künstlich
auf 1 ms setzen). Danach `coconut_held_scrai` / `state` prüfen und erneut spenden.
**Erwartung heute:** die Coins des ersten Versuchs sind weg (Zähler avanciert +
persistiert, `pay_info` verworfen); der zweite Versuch nimmt die *nächsten* Coins
mit *neuem* `pay_info`. Netto-Verlust ohne Angreifer, nur durch Mixnet-Paketverlust.
**Nach Fix:** ein persistierter Pending-Record `{purse_idx, pay_info, coins,
spend_date}` wird beim Retry mit demselben `pay_info` wiederverwendet →
`detect()` klassifiziert als `Replay` (kein Verlust, kein Ban).

Unit-nah nachstellbar in `core` (ohne Mixnet), da `Purse::spend` + `QuorumStore`
dort testbar sind (vgl. `purse.rs`-Tests): (1) spend → persist → „Send schlägt
fehl" → mit demselben `pay_info` erneut spenden → Quorum `Replay` (SAFE);
(2) im Ist-Verhalten wird der zweite Spend mit *neuem* `pay_info` ausgeführt →
frische Coins verbraucht → beweist den Verlust.

---

## Szenario D — Unehrlicher Operator überteuert unbemerkt (C3)  · ✅ CLIENT-GUARD GEFIXT

**Gefixt:** der Client rechnet den fairen Preis jetzt unabhängig nach
(`fair_price_estimate` in `lib.rs`, gleiche `scrai-core`-Mathematik, aus dem EIGENEN
Token-Estimate + der mitgelieferten Preisliste), flaggt Ladungen > fair × 4 als
`priceWarning` und verweigert weiteres Auto-Redeem in einen geflaggten Server.
Regressionstests: `cargo test -p scrambleai c3_tests` (3 grün). Fängt sowohl
`MARGIN`-Inflation als auch Token-Inflation, weil der Client den Server-Token-Count
nicht glaubt. **Verbleibend (Föderations-Milestone):** signierte, versionierte Liste
mit gepinntem Pubkey, damit auch ein FREMDER Operator die Liste nicht fälschen kann.

<details><summary>Ursprüngliche manuelle Repro (weiterhin gültig, um den Guard zu prüfen)</summary>

**Kern:** Preis, Marge und `usage` waren rein serverseitig; der Client rechnete nie
nach (`billing.js:8-9`, `index.html:1658` `balance = evt.balance`).
**Setup:** eigenen scrai-server mit `MARGIN=1000` starten (oder `pricing.json`
aufblasen, oder in `chat.rs` die zurückgegebene `usage.outputTokens` ×50).
**Schritt:** in der App eine normale Chat-Nachricht senden.
**Erwartung heute:** die App zeigt den überhöhten Preis als Fakt an und zieht ihn
von der Balance ab — **keine Warnung, keine Client-Gegenrechnung.** Über mehrere
Nachrichten drainiert der Operator das gesamte Guthaben.
**Nach Fix:** der Client rechnet den fairen Preis aus einer *signierten* Retail-Liste
(gepinnter Pubkey) gegen seinen eigenen Token-Count nach und lehnt/flaggt jede
Ladung darüber; Marge ist in die signierte Liste gefaltet → `MARGIN`-env wirkungslos.

**Automatisierbar** als Metering-Konsistenztest, sobald die Client-Nachrechnung
existiert: „charge ≤ signed_rate × own_token_count" gegen eine Reihe von Antworten.
Heute nicht automatisierbar, weil die zu testende Prüf-Logik fehlt — das ist selbst
der Befund.

</details>

---

## Szenario E — Operator nimmt Coins/Entitlement ohne Gegenwert (H2, H3)  · HARNESS

**H2 (redeem):** Rogue-Server nimmt den `redeem`-Serial ins Quorum auf, gibt aber
**keine** `sessions.credit`. **Schritt:** Client redeemt eine Purse gegen den
Rogue-Server. **Erwartung heute:** Purse-Coins verbrannt (irreversibel, Re-Send =
`Replay` ohne Gutschrift), Session-Balance 0, kein Beweis für den Client.
**Nach Fix:** Gutschrift atomar mit dem Serial-Burn; optional signierte Quittung,
die der Client verifiziert, bevor er die Coins als „ausgegeben" abschreibt.

**H3 (withdraw):** Rogue-Server verifiziert die Kontosignatur, **konsumiert das
Entitlement-Buch**, liefert aber eine Müll-`BlindedSignature`. **Schritt:** Client
`coconut_withdraw` gegen den Rogue-Server. **Erwartung heute:** `issue_verify`
lehnt ab → kein nutzbares Wallet, aber das mit Echtgeld gekaufte Buch ist futsch.
**Nach Fix:** verify-then-consume / idempotente Ausgabe → kein Verbrauch ohne
verifizierte Ausgabe.

---

## Szenario F — DoS: ein Request legt den Server lahm (H8)  · MANUELL

**Setup:** laufender scrai-server.
**Schritt:** einen Free-Tier-Chat an ein Modell senden, dessen Upstream hängt
(z. B. eine Firewall-Regel, die ausgehend zu Groq/Gemini blackholet), oder schlicht
viele `{"kind":"models"}` fluten.
**Erwartung heute:** der serielle Loop (`main.rs:159-262`) blockiert im
untimed `send().await`; **alle** anderen Clients werden nicht mehr bedient.
**Nach Fix:** `.timeout()` auf jedem Provider-Call + `tokio::spawn` pro Nachricht →
ein hängender Upstream betrifft nur diesen einen Request.

---

## Szenario G — DoS: Client-Nachricht paniked/erschöpft den Server (M-srv-2, L-srv-1/2/3)

- **Upload-Bombe:** ein `upload.chunk` mit riesigem `data` (Base64) wird **vor** dem
  Size-Check dekodiert (`uploads.rs:73`). → Ceiling **vor** Decode erzwingen.
- **Overflow-Gate:** `upload.begin` mit `totalBytes ≈ 2^64-staged+k` umgeht den
  Kapazitäts-Check (`uploads.rs:56`). → `checked_add`.
- **`maxTokens`-Overflow:** `chat` mit `maxTokens: 18446744073709551615`
  (`chat.rs:72`). → auf Context-Window klemmen.
- **UTF-8-Panic:** MITM auf dem **Klartext-HTTP-LCD** (`nyx.rs:241`) liefert einen
  Fehler-String, dessen Byte 200 ein Multibyte-Zeichen teilt. → `chars().take(200)`.

---

## Rogue-Server-Harness (für A E2E, D, E)

Der schärfste Test ist ein **bösartiger scrai-server**, gegen den die echte
Tauri-App über das Mixnet spricht. Aufbau (klein, wiederverwendbar):

1. Kopie von `server/` als `rogue-server/` (oder ein Feature-Flag `SCRAI_ROGUE=1`),
   das gezielt eine der folgenden Bosheiten einschaltet:
   - **A:** in die `chat`-Antwort ein `images:[{mimeType:"image/png", data:"<payload>"}]`
     mit dem C1-Ausbruch einschleusen.
   - **D:** `MARGIN` ignorieren und `cost`/`usage` frei setzen (z. B. ×50).
   - **E-H2:** in `gateway::redeem_reply` das `sessions.credit` überspringen, den
     Serial aber ins Quorum schreiben.
   - **E-H3:** in `federation`/`main.rs` das Entitlement konsumieren, aber eine
     zufällige `BlindedSignature` zurückgeben.
2. `cargo run -p rogue-server` → Nym-Adresse notieren.
3. In der App-DevTools `invoke('set_server',{address:'<rogue addr>'})`
   (dass das **ohne Validierung** geht, ist selbst Finding H5).
4. Normale Aktionen ausführen (Chat / withdraw / redeem) und beobachten:
   Seed-Exfiltration (A), stille Überteuerung (D), verbrannte Coins ohne
   Gegenwert (E).

> Der Harness ist bewusst noch nicht eingecheckt (er ist scharfe Munition). Diese
> Beschreibung genügt, um ihn bei Bedarf in <1 h aus `server/` abzuleiten. Wenn ihr
> wollt, lege ich `rogue-server/` als deaktiviertes, klar gekennzeichnetes Test-Target
> an.

---

## Was NICHT getestet werden muss (bestätigt sicher)

- Coin-Fälschung, Double-Spend-Framing eines ehrlichen Nutzers, Rückführung eines
  Spends auf das Kaufkonto → kryptografisch verhindert (Abschnitt 6 des Audits).
- Path-Traversal (Uploads in-memory), SQL-Injection (parametrisiert),
  Secret-Leak in Antworten, OCR/PII-Abfluss ans Netz, Handover-Export enthält
  keinen Seed. Alle als „sauber" verifiziert.
