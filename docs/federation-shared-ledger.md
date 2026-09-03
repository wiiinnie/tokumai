# Multi-Server — das „gemeinsame Gästebuch" (Shared Ledger, alle Server vom selben Betreiber)

Status: **Design, Ledger-Logik gebaut, nicht verdrahtet** (2026-08-21; Betriebsmodell
festgezurrt 2026-09-03). Das Zielmodell für mehrere Server, die **alle vom selben
Betreiber** laufen. Ziel: **jede** Wertform funktioniert über alle Server hinweg UND über
Geräte­wechsel — ohne dass Guthaben irgendwo strandet.

Voraussetzung / Trust-Modell: **Du betreibst jeden Server und die geteilte Datenbank
selbst.** Dieses Design ist sicher **genau so lange, wie das gilt** (siehe §6 — die Grenze).

> **Entscheidung 2026-09-03: es gibt KEIN Fremdbetreiber-Modell.** Ein früherer Plan sah
> eine „trustless Föderation" mit fremden Operatoren vor (Bearer-Ecash + t-of-n-DKG über
> fremde Authorities + Betreiber-Clearing). Der ist gestrichen — nicht aus technischen,
> sondern aus regulatorischen Gründen: sobald Kundengeld über die Authority an fremde
> Betreiber fließt (Settlement/Clearing), ist das ein Zahlungsdienst bzw. E-Geld →
> BaFin-Erlaubnis → GwG-Verpflichteter → **Identifizierung der Kunden**, was das anonyme
> Modell beendet. Fremde Node-Betreiber partizipieren stattdessen auf der Nym-Ebene: als
> **Entry-Gateways der scrai-Server** (`SCRAI_GATEWAY_MASTER`/`SCRAI_GATEWAY_FALLBACK`),
> vergütet über Nyms Bandbreiten-Ökonomie (heute frei, später bezahlte
> Bandbreiten-Credentials). Dabei fließt kein Geld von uns an sie und kein Kundengeld über
> sie. Begründung und Rest der Roadmap: `docs/ROADMAP.md` „Operating model".

> **Implementierungs-Stand (2026-08-21).** Die Naht + die Ledger-Logik sind **gebaut &
> getestet**:
> - `core/src/ledger.rs` — die async `Ledger`-Trait-Naht (nur die seltenen Ops
>   `credit`/`status`/`balance`; Billing bleibt lokal). `SessionStore` implementiert sie
>   (heutiger Einzelserver).
> - **Crate `ledger/` (`scrai-ledger`)** — das „Gästebuch" real: `store` (gehärtete lokale
>   SQLite, 0600 + WAL + FULL, §6c), `proto` (signiertes Wire-Format + Member-Key-Krypto-ACL
>   + Nonce-Replay-Schutz, §6b/§6c), `service` (verify→dispatch→reply-Handler), `mixnet`
>   (`MixnetLedger`-Client über eine `LedgerTransport`-Abstraktion). **11 Unit-Tests grün**
>   inkl. End-to-End Client→Service→Store über einen Loopback-Transport (nur der physische
>   Mixnet-Hop fehlt).
> - **Noch INFRA-abhängig (braucht laufendes Mixnet + ≥2 Server zum Validieren):** die
>   konkrete Nym-`LedgerTransport`-Impl im Server, der Ledger-Service als Nym-SP-Loop
>   (spiegelt `server/src/main.rs`), das Deployment, und das **Lease** (§6b/§6d) für
>   Multi-Server-Billing-Kohärenz. Diese Teile sind bewusst noch nicht verdrahtet.

---

## 1. Das Problem, das gelöst wird

Zwei Wertformen, jede nur zur Hälfte portabel (Details: Session-Erklärung im Audit-Thread):

| Form | Über Gerät (via Seed)? | Über Server hinweg? |
|---|---|---|
| Held Ecash (Coconut-Purses) | ❌ Bearer, nicht seed-ableitbar | ✅ via geteiltem VK |
| Session-Credit (redeemed) + Entitlement | ✅ seed-abgeleitete IDs | ❌ liegt heute in **einer** Server-SQLite |

Die Klemme: das Seed-recoverable Guthaben (Session + Entitlement) ist heute **server-lokal**,
weil es in der lokalen SQLite *eines* Servers liegt (`server/src/store.rs`). Wechselt der
Nutzer den Server, kennt der neue Server die seed-abgeleitete `sessionId`/`accountId` nicht.

**Kernidee:** Die seed-abgeleiteten Guthaben in **einen geteilten Store** legen, den *jeder*
deiner Server liest/schreibt. Die `sessionId`/`accountId` sind bereits seed-abgeleitet und
**server-agnostisch** — dieselbe `sessionId` trifft bei Server A und B **dieselbe Zeile**.
Damit „findet der Seed alles überall".

---

## 2. Die drei Bausteine (zusammen ergeben sie „alles überall")

Das Gästebuch allein reicht nicht — es braucht drei Teile, die zusammenspielen:

1. **Gemeinsamer Authority-Key → geteilter VK.** Damit **Held Ecash** bei jedem deiner
   Server gilt, müssen alle Server denselben aggregierten Verification-Key kennen. Bei
   eigenen Servern der einfachste Weg: **eine** Authority-Identität, die alle Server teilen
   (oder t-of-n unter deinen eigenen Servern). Dann ist ein Coin von irgendwo bei jedem
   deiner Server verifizierbar — offline, ohne Rückfrage. *(t-of-n über die eigenen
   Server schützt gegen eine einzelne kompromittierte Box, siehe §6.)*
2. **Shared Ledger („Gästebuch") — der Fokus dieses Docs.** Ein geteilter, transaktionaler
   Store für **Session-Balances**, **Entitlement** und den **Spent-Serial-Set** (Double-
   Spend). Alle Server lesen/schreiben denselben.
3. **Signiertes Directory.** Damit Clients deine Server-Endpunkte + den VK authentisch
   erfahren (kein Gossip nötig — Tor-Consensus-Modell, siehe `federation-params.md`).

Held Ecash wird durch (1) portabel, Session/Entitlement durch (2). Beides zusammen: jede
Form funktioniert über alle deine Server + jedes Gerät mit dem Seed.

---

## 3. Was ins Gästebuch gehört

Heute liegt all das **pro Server lokal** (`store.rs` SQLite, `session.rs SessionStore`,
`pay.rs` Entitlement). Es wandert in **einen** geteilten Store, unverändert gekeyed:

| Datum | Key | heute | im Shared Ledger |
|---|---|---|---|
| Session-Balance (SCRAI) | `sessionId` (seed-abgeleitet) | lokale SQLite | geteilte Zeile, alle Server |
| Entitlement (bezahlt, nicht abgehoben) | `accountId` (seed-abgeleitet) | lokal | geteilt |
| Spent-Serials / Quorum | Coin-Serial | lokal | geteilt (Double-Spend über alle Server erkannt/verhindert) |
| Invoice-/Paywall-State | invoiceId / accountId | lokal | geteilt (kein Doppel-Settle) |

Wichtig: **Keine Key-Änderung nötig.** Die IDs sind schon seed-abgeleitet und
server-neutral. Nur der *Ort* des Stores ändert sich (lokal → geteilt).

---

## 4. Architektur (für eigene Server)

**Empfehlung: ein gemeinsamer, verwalteter Postgres.** Alle Server verbinden sich dagegen;
er ist die autoritative Quelle. Stark konsistent, transaktional, ein Ort für Row-Locks.
SPOF = die DB → mit managed HA/Replikation abfedern.

Alternativen, bewusst *nicht* der Startpunkt:
- **Ledger-Service:** ein Server „besitzt" das Gästebuch, die anderen rufen ihn per RPC für
  Credit/Debit. Ist im Grunde die Postgres-Variante mit einer Service-Schicht davor —
  unnötige Extra-Schicht für den Start.
- **Raft/Multi-Primary:** höhere Verfügbarkeit, deutlich mehr Komplexität. Overkill für eine
  Handvoll eigener Server.
- **SQLite über Netz / Litestream:** SQLite ist kein guter Multi-Writer über Netzwerk —
  vermeiden, sobald >1 schreibender Server existiert.

Mapping auf den Code: `store.rs` zeigt statt auf lokale SQLite auf den geteilten Postgres;
die Reserve/Settle/Credit/Debit-Operationen laufen transaktional dagegen (dieselbe
Atomaritäts-Disziplin wie H2's `save_many`, nur gegen den geteilten Store).

---

## 5. Der EINE echte technische Knackpunkt: Nebenläufigkeit (kein Trust-Problem)

Solange du alles betreibst, ist **niemand unehrlich** — das Restrisiko ist rein technisch:
**Races**. Zwei parallele Anfragen (z. B. zwei Geräte mit demselben Seed, oder derselbe
Nutzer an Server A und B gleichzeitig) dürfen dieselbe Session-Balance nicht doppelt
reservieren.

Regeln, die das sauber lösen (Standard-DB-Handwerk):
- **Atomarer bedingter Debit:** `UPDATE ... SET balance = balance - x WHERE balance >= x`.
  Schlägt die Bedingung fehl → keine Über-Reservierung möglich, nie negativer Stand.
- **Row-Lock pro `sessionId`** für das Reserve→Settle-Fenster einer Nachricht (euer
  reserve/settle in `session.rs` existiert schon — es muss nur gegen den geteilten Store und
  mit Lock laufen, statt gegen die lokale In-Process-Map).
- **Credit (Redeem) ist additiv** → mit einer Transaktion von Natur aus race-frei.
- **Spent-Serial: `INSERT`, der bei Duplikat fehlschlägt** (unique constraint) → dieselbe
  Münze kann an keinen zwei Servern gleichzeitig „durchrutschen" (**Prävention**, nicht nur
  Detektion — das ist der Vorteil, den nur eigene, vertraute Infra erlaubt).

Damit ist Double-Spend über deine Server hinweg **verhindert**, nicht bloß nachträglich
bestraft — genau weil der Spent-Set geteilt und autoritativ ist.

---

## 6. Die Sicherheits-Grenze (WANN dieses Modell bricht)

**Dieses Design ist sicher genau dann, wenn du jeden Server UND die geteilte DB
kontrollierst.** Der Grund: das Gästebuch ist eine *vertraute* autoritative Datenbank. Es
gibt keine Kryptografie, die einen schreibenden Server daran hindert zu lügen — die
Sicherheit kommt allein daraus, dass alle Schreiber *du* bist.

Der Moment, in dem es bricht — und was dann gilt:
- **Ein fremder Operator darf ins Gästebuch schreiben** → er fälscht Balances, löscht
  Spent-Serials, doppel-settlet Invoices. Der ganze Punkt des Ecash (Operator *nicht*
  vertrauen müssen) wäre verletzt.
- **Ein fremder Operator hält einen Authority-Share** (bei geteiltem Single-Key sogar den
  ganzen Key) → er kann **Geld minten**.

Deshalb die harte Regel:

> **Onboarde NIE einen 3rd-Party-Operator auf das Gästebuch oder den geteilten Authority-Key.**
> Es gibt dafür auch kein Ersatzmodell mehr (Entscheidung 2026-09-03, siehe oben): ein
> Fremdbetreiber, der an unserem Umsatz teilhat, macht uns zum Zahlungsdienstleister.
> Fremde beteiligen sich nur auf der Nym-Ebene (Entry-Gateways), nie an Authority, Ledger
> oder Guthaben.

Was aus dem alten „trustless"-Katalog trotzdem bleibt, hat einen anderen Gegner — die
**kompromittierte eigene Box** statt des unehrlichen Fremden: **t-of-n-DKG** über die
eigenen Server (ein gehackter Server mintet nicht allein), die **signierte Preisliste** (ein
gehackter Server kann nicht überteuern), Client-`verify_share` + `flag_server`. Details:
`federation-params.md` „Security roadmap".

Das Gästebuch ist damit das **Zielmodell**, kein Zwischenschritt — maximal einfach, weil
alle Schreiber derselbe Betreiber sind.

---

## 7. Rollout (jeder Schritt für sich sicher)

1. **Heute:** ein Server, lokale SQLite.
2. **Shared DB hochziehen**, den bestehenden Server darauf umstellen, aktuelle Stände
   migrieren. Auch mit nur einem Server schon ein sicheres No-Op — nichts ändert sich fürs
   Verhalten, nur der Speicherort.
3. **Server #2 gegen dieselbe DB + denselben Authority-Key/VK** hochziehen. Balances „gehen
   einfach", weil dieselben seed-abgeleiteten Zeilen; Ecash gilt via geteiltem VK.
4. **Signiertes Directory** ausliefern, damit Clients beide Endpunkte + den VK kennen.
5. **Client:** muss nur wissen, dass die Server eine Föderation sind (gleicher VK) und beide
   Endpunkte kennen. Die Migrations-UI braucht dann den Absichts-Split „neues Gerät" vs
   „Server wechseln" (letzterer wird durch das Gästebuch aber weitgehend zum Nicht-Problem,
   weil das Guthaben ohnehin überall sichtbar ist).

---

## 6b. Zugriff auf den Ledger: über den MIXNET, nicht über einen Clearnet-Port

Entscheidung (2026-08-21): der Shared Ledger wird **nicht** als Postgres-Port ins Netz
gestellt, sondern als **eigener Nym-Service-Provider** mit Mixnet-Adresse. Die echte
SQL-DB liegt **lokal** hinter diesem Service (nur Unix-Socket/localhost, **kein**
Netzwerk-Port). Die anderen Server sind **Clients** über den Mixnet und schicken
*semantische* Ops (`credit`, `redeem`, `entitlement`), **nie** rohes SQL.

Warum: **kein Clearnet-Port = keine Portscan-/DDoS-/Postgres-CVE-Angriffsfläche.** Die
Zugriffskontrolle wird kryptografisch (nur Server mit Föderations-Key dürfen schreiben)
statt per Netzwerk-ACL. Metadaten-Privacy gratis dazu (der Mixnet verbirgt sogar, dass
Server A mit dem Ledger spricht).

Das fügt sich exakt in die `Ledger`-Trait-Naht:
- `MemLedger` / `LocalLedger` — In-Process gegen die lokale DB. **Der Ledger-Service
  selbst nutzt den.** Ein All-in-one-Einzelserver auch.
- `MixnetLedger` (später) — dünner Client-Stub, der dieselben Ops als Mixnet-Requests an
  den Ledger-Service schickt. Reiner Impl-Tausch, kein Refactor der Call-Sites.

**Kritische Grenze — der Hot-Path bleibt LOKAL:** das Chat-Billing (`reserve`→`settle`
PRO NACHRICHT) darf NICHT über den Mixnet-Ledger laufen — das würde jede Chat-Antwort um
~2 Mixnet-Roundtrips verlangsamen. Nur die **seltenen** Ops (Kauf neuer Credits, Redeem)
gehen an den Shared Ledger; deren Latenz ist egal. Das Per-Nachricht-Billing läuft weiter
gegen den lokalen `SessionStore`.
- **Konsequenz fürs Sichtbar-Machen der Balance über Server:** die spätere, saubere
  Zusammenführung ist das **Lease-Modell** — ein Server checkt die Session-Balance einmal
  aus dem Shared Ledger aus (1 Mixnet-Roundtrip), billt lokal schnell, gibt den Rest bei
  Idle/Session-Ende zurück (selten). Nur **ein** Server hält das Lease gleichzeitig
  (Lock im Ledger) → kein Doppel-Verbrauch über Server.
- **Status: NICE-TO-HAVE, geparkt** (2026-08-21). Erst beim Performance-Tuning bauen.
  Bis dahin gilt: Billing lokal, Redeem/Kauf können über den Mixnet-Ledger gehen; die
  volle server-übergreifende Live-Sichtbarkeit der *laufenden* Balance kommt mit dem Lease.

## 6c. Härtung der Datenbank selbst

Auch ohne Clearnet-Port gehärtet (Defense-in-depth):
- **At-Rest verschlüsselt:** Volume-Verschlüsselung (LUKS) als Basis; bei SQLite zusätzlich
  **SQLCipher** (transparentes AES über die ganze Datei). Key im OS-Keychain / 0600-File,
  konsistent mit dem Wallet-at-rest-Ansatz (H6).
- **Dateirechte:** DB-Datei `0600`, Eigentümer ein dedizierter unprivilegierter Service-User
  (wie `scrai`). Kein `world/group`-Read.
- **Kein Netzwerk-Listener:** Postgres `listen_addresses=''` (nur Unix-Socket) bzw. SQLite
  ohnehin dateibasiert. `pg_hba.conf` nur local + `scram-sha-256`; dedizierte Rolle mit
  Least-Privilege (nur die Ledger-Tabellen).
- **Version/Patching:** unterstützte Major-Version pinnen, Security-Auto-Updates,
  minimale Extensions.
- **Die eigentliche Zugriffskontrolle liegt am Mixnet-Service:** der Ledger-Service
  akzeptiert nur Requests, die mit einem **Föderations-Member-Key** signiert sind
  (Allowlist der Member-Pubkeys), integritätsgeschützt + nonce-replay-geschützt. Das
  ersetzt Netzwerk-ACLs durch Krypto-ACLs.

## 6e. TEMPORÄR: hard-codierter Default-Server im Client — VOR Federation ENTFERNEN

Stand 2026-08-23: der Client hat eine **hard-codierte Liste `KNOWN_SERVERS`** (`public/index.html`)
mit dem einen offiziellen scrai-server als Dropdown-Default (plus „Custom / manual…" für freie
Eingabe). Grund: bequemes Onboarding, solange es genau **einen** Operator gibt (dich).

**Ist das ein Risiko?** Kein Secret-Leak — eine Nym-Adresse ist öffentlich (so erreichen Clients
den Server). Die realen Punkte:
- **Zentralisierung:** alle Clients defaulten auf deinen Server (aktuell ohnehin die Realität).
- **Update-Fragilität:** rotiert die Server-Nym-Identität, brechen hard-codierte Clients bis zum
  neuen Build → **Server-Identität stabil halten** (nym-client-Key auf dem VPS nicht neu erzeugen).
- **Kurzfristig leicht SICHERER** als Freitext: ein Angreifer kann den Nutzer schwerer auf einen
  bösen Server locken (H5). Aber es **pinnt Trust auf einen Operator**.
- **Konflikt mit dem Directory:** widerspricht dem signierten Directory-Modell (§4/§2 Baustein 3).

**TODO (ersetzen, sobald es mehr als einen Server gibt):** `KNOWN_SERVERS` wird zur
**signierten, versionierten Server-Directory** (wir signieren die Liste, Client verifiziert
gegen gepinnten Governance-Key — Tor-Consensus-Modell). Der hard-codierte Default fliegt dann
raus bzw. wird zum signierten Directory-Eintrag. Zwischenstand (0.4.6): der Katalog trägt
bereits `identities` (die K Adressen eines Servers) als Fallback-Liste im Client.

## 6d. Zwei Geräte / gleichzeitiger Betrieb — Lease-TTL + Fencing

Teil des geparkten Lease-Modells (§6b). Kontext: beide Geräte haben denselben Seed →
dieselbe `sessionId` → **dasselbe** Guthaben. Es ist **kein Angreifer**, sondern derselbe
User — die Stakes sind niedrig (eigenes, kleines Guthaben). Ziel: kein Doppel-Verbrauch,
keine Aussperrung, vernünftige UX.

- **Zwei Geräte am SELBEN Server → kein Problem, geht heute schon.** Ein Lease, eine lokale
  Balance, der Server serialisiert beide Verbindungen. Kein Lease-Protokoll nötig.
- **Zwei Geräte an VERSCHIEDENEN Servern gleichzeitig → der einzige knifflige Fall.** Beide
  wollen dasselbe `sessionId`-Guthaben; der Ledger vergibt das Lease exklusiv an einen.

**Gewählter Ansatz (erstes Multi-Server-Release): exklusives Lease + kurze TTL + Fencing.**
- **Exklusiv:** ist Gerät 1 aktiv (Lease bei Server A), bekommt Gerät 2 an einem anderen
  Server ein **„Session gerade auf deinem anderen Gerät aktiv"** — für einen seltenen Fall
  völlig okay. Wird Gerät 1 idle, übernimmt Gerät 2 nach **TTL-Ablauf**.
- **TTL + Heartbeat (warum):** jedes Lease läuft nach z. B. 60 s ab und muss vom Halter
  erneuert werden. Ohne TTL würde ein **abgestürzter/offline** Server das Lease für immer
  halten → der User wäre aus dem eigenen Guthaben ausgesperrt. Erneuert der Halter nicht
  (weg/idle), verfällt das Lease und der Ledger darf es neu vergeben („per Timeout
  gestohlen").
- **Fencing-Token (Epoche) — verhindert Split-Brain:** war der alte Halter nur langsam statt
  tot, würden nach dem Steal **beide** billen → Doppel-Verbrauch. Fix: jede Lease-Vergabe
  trägt eine **monoton steigende Epoche**; der Ledger akzeptiert einen Reconcile/Rückschreib
  **nur mit der aktuellen Epoche**. Ein verspäteter Rückschreib des alten Halters (Epoche N <
  aktuelle N+1) wird **verworfen** → kein Doppel-Verbrauch; schlimmstenfalls gehen die
  *unbestätigten* Abbuchungen des alten Halters aus dem abgelaufenen Fenster verloren (nicht
  verdoppelt).
- **Fail-closed:** sobald ein Server seinen Lease-Heartbeat zum Ledger nicht mehr durchbringt,
  **stoppt er das Billing dieser Session**, statt optimistisch weiterzumachen → minimiert den
  Verlust im abgelaufenen Fenster.

**Alternative (nur falls echter Parallelbetrieb ein Bedürfnis wird): Balance-Splitting** — der
Ledger verleast jedem Gerät/Server einen **Chunk** (z. B. $3 an A, $2 an B), beide billen
unabhängig, holen bei Bedarf nach. Kein Stehlen, aber fragmentiertes Guthaben mit
gelegentlichem Rebalancing. Nicht der Startpunkt.

**Heute (ein Server):** komplett moot — zwei Geräte treffen denselben Server, ein Lease, eine
lokale Balance, Serialisierung. TTL/Steal/Fencing entstehen erst mit mehreren Servern UND
echtem Gleichzeitig-Betrieb.

---

## 8. Warum das die Migrations-Frage auflöst

Mit dem Gästebuch verschwindet das „redeem-all bindet an einen Server"-Problem für **deine**
Server: die Session-Balance liegt in der geteilten DB, also sieht *jeder* deiner Server sie
unter derselben `sessionId`. Held Ecash gilt via geteiltem VK ohnehin überall. Ergebnis:

- **Neues Gerät:** Seed rein → Entitlement + Session-Balance da (geteilte DB), Ecash am alten
  Gerät ggf. vorher redeemen (bleibt der einzige Bearer-Sonderfall).
- **Server wechseln:** kein Stranden mehr — dieselbe DB, dieselben Stände.

Der einzige Rest-Bearer-Fall (Held Ecash über Gerätewechsel) bleibt bestehen; für eigene
Server genügt „am alten Gerät vor dem Wechsel redeemen" (kleine `REDEEM_CHUNK_COINS` halten
den möglichen Verlust bei $1). Eine seed-verschlüsselte Backup-Box wäre reiner Komfort.

---

Related: `federation-params.md` (Parameter + Security-Roadmap für den Multi-Server-Betrieb),
`security/audit-2026-08-20.md` (H9 DKG, H3 Fair-Exchange, C3 Preisliste).
