# Memory Palace

A memory that follows the person across chats and devices, like the memory of the big
assistants — end-to-end encrypted, so the operator can neither read it nor tie it to an
account or a payment. Built 2026-11-23. It replaces the handover.md feature (a summary
exported at the end of a chat and pasted back in), which is gone without migration.

The name is the memory technique's (method of loci); code says `palace`.

## What it is made of

| | where |
|---|---|
| keys, blob names, sealing, request signatures, the plaintext shapes, merging | `crates/core/src/palace.rs` |
| the store: opaque blobs per namespace, TOFU key, replay window, quota | `crates/enclave/src/palace.rs`, tables `palace_ns` / `palace` in `ledger.rs` |
| `read_memory`: the index with a question, the tool, the provider mapping | `crates/enclave/src/memory.rs`, `openai.rs`, `gemini.rs` |
| the device's copy, the upload queue, the sync | `app/src-tauri/src/palace.rs`, section "the Memory Palace" in `lib.rs` |
| index, tool results, extraction, undo | `app/public/palace.js` |
| the personal-information check | `app/public/pii.js` |
| the words, German and English | `app/public/i18n.js` (`palace.*`) |
| the screens: indicator, palace sheet, opt-in, toast, diff | `app/public/index.html`, "the Memory Palace" |

## Keys

Everything comes from the BIP39 seed of the 24 words, by HKDF-SHA256 under labels of its
own: `tokumai/palace/v1/enc` (AES-256-GCM key), `tokumai/palace/v1/sign` (Ed25519 seed),
`tokumai/palace/v1/ns` (namespace salt). The account key is `sha256("scrai/account/v1" ‖
seed)` and the note seed comes from the account key: the palace's keys and the account's
share nothing that can be followed from one to the other without the seed (test:
`the_palace_and_the_account_share_nothing_that_can_be_followed_without_the_seed`).

- `namespaceId = sha256(signPub ‖ nsSalt)`.
- `blobId = HMAC-SHA256(encKey, "tokumai/palace/v1/blob:" ‖ fileId)`: the enclave never
  sees a file id or a title.
- A blob is `nonce(12) ‖ AES-256-GCM(plaintext)` with AAD `namespaceId ‖ blobId ‖
  version(u64 BE)`.
- The device's own cache (`palace.bin`) is sealed under the same key with its own label.

## On the wire

`palace.list`, `palace.get`, `palace.put` (with `expected`; a stale one answers
`palace.conflict` with the version there is — the "409"), `palace.delete`, `palace.wipe`.
Each body carries `ns`, `pub`, `ts`, `nonce` and an Ed25519 signature over
`tokumai/palace/v1|ns|op|blobId|version|sha256(ct)|ts|nonce`. The key is registered on the
first write (trust on first use); the timestamp must be within five minutes and the nonce
new.

The envelope around it is the usual sealed request, but signed by an account made for that
one request and thrown away — **never the account's** — and it travels over a mixnet client
of the palace's own (one per burst, dropped after it), never the account's client. The
enclave does not count it as an account seen.

What the enclave stores per blob: namespace, blob id, version, ciphertext, size, written
at. Nothing else. Quota: 256 KB a blob, 2 MB and 512 blobs a namespace, 1 GB in all (the
book lives on a 4 GiB volume). The operator's console shows totals only: namespaces,
blobs, bytes.

## Sync

Local first. A change is merged into the device's copy and sealed to disk before anything
else; from then on it counts as saved. The blobs it touched go into a queue kept in the same
file (it survives a restart) and are uploaded at the NEXT session — when the app starts, or
comes back after an hour or more in the background, or on "Sync now" (`palace::UPLOAD_WINDOW`)
— after a random 5–120 s, and never within 90 s of a blind note being minted or redeemed
(`palace::SPEND_GAP_MS`). Hours or days lie between a conversation and the upload it caused,
so the namespace's writes cannot be lined up with an account's questions by their timing;
the extraction itself is the account's own request like any question and needs no delay.
Until then the indicator says "Saved on this device · uploads next session". Failures back off (10 s
doubling to 30 min, with jitter); after five in a row the indicator says "failed".

At start (and on "Sync now"): `list`, then `get` for what changed — the index first, then
the files — and merge. Merging (`core::palace::merge_*`): files and lines are united by id;
for the same id the later edit wins and a deletion always wins (tombstones carry the hash of
the words, not the words); a live line whose words a tombstone of the same age or newer
carries is gone; duplicates by normalised words keep the smallest id, so every device
converges on the same result. An undone deletion comes back as a new line. A conflict on
upload: fetch, merge, seal again, retry.

A new device: the phrase, the opt-in, a full sync. Nothing to export or import.

## The model and the palace

With each question goes the index (title and description of each file that has something
sendable), anonymised. The enclave tells the model what the palace is and offers one tool,
`read_memory(file_id)`. A call comes back to the app in the answer (`toolCalls`); the app
reads the file from its own copy, leaves out every line and file marked "never send",
anonymises, cuts it to 15 KB, and asks again with the call and the result appended. Three
reads per question at most: after that the tool stays declared but may not be called
(`tool_choice: none` / `mode: NONE`). Every round is an ordinary chat request, held and
billed as such; nothing of it is kept in the enclave.

Providers: OpenAI (`function_call` / `function_call_output`) and Gemini (`functionCall`
with its thought signature handed back, `functionResponse`). Not beside Gemini's search
(they do not share a request) and not for picture models. Grok is not a provider here.

## Writing memory

After an answer and 60 s without input (configurable), when the chat is left, when the app
goes to the background, or by hand ("Save to Memory Palace" on the chat in the rail), the
app sends the turns since the last extraction (at least two, at least one of them the
person's) and the index to the cheapest text model of the provider the chat already uses,
with a prompt that asks for JSON only: durable facts, no inferences, placeholders kept.
The answer becomes changes (`palace.js::parseExtraction`; a correction names the old fact
by its words, so the lines themselves are never sent). The personal-information check runs
on them before they are kept and anonymises what it finds without asking (saving runs in
the background and must not interrupt; turning the check off is the way to keep values as
they are). Then: "Memory updated · View · Undo" — or, with "Ask before saving", the sheet
with checkboxes first. The cost shows under the answer as "Memory update".

Off, paused for the chat (incognito), or no palace: nothing runs.

## The personal-information check

On the device, before anything leaves it: the prompt, the index, a file the model asked
for. Employers and people's names after a trigger phrase ("ich arbeite bei …", "my wife …";
a company or brand merely mentioned is not personal), email, phone, postal address, IBAN and card numbers (with their
checksums), birth dates after a trigger. Modes: ask (default) / always anonymise / off.
"Anonymize" puts a stable placeholder in place (`[EMPLOYER_1]`); the mapping is kept in the
chat (vault) and in the palace (sealed, so another device can read lines back) and never
goes to a model; the answer is shown with the values put back. "Don't ask again for …" is
kept on the device. Memory content leaving the device is always anonymised unless the check
is off.

## Threat model: what changes

| Aspect | Before (stateless) | With the Memory Palace | Mitigation |
|---|---|---|---|
| Content at the server | Nothing stored | Ciphertext only; the key never leaves the devices | AES-256-GCM, keys from the phrase |
| Linkability at the server | Every request unlinkable | **A long-lived pseudonym (the namespace)** with an activity pattern: sync times, sizes, how often it changes | Mixnet, its own mixnet client, throwaway envelope keys, no IP anywhere, random upload delays; padding later |
| Palace ↔ payment | n/a | None by design | Separate derivation, separate requests and clients, 90 s away from any note |
| Linkability at the model provider | Within one chat | **By content, across chats**, when memory is read | Index only plus `read_memory`, anonymisation, "never send", incognito chats |
| Value of the phrase | Access and credit | **Access and the whole memory** | Said at the opt-in; the phrase is already behind the device's owner check |
| Data at rest, legal requests | None | Ciphertext only, unreadable without the phrase | "Delete entire Memory Palace" removes the server copy and the device's; the book's snapshots are kept 14 days (`probe.sh backups`), so a deleted blob is gone from every copy after that |

Opt-in at first start with the trade-off said plainly; "This device only" keeps no server
copy at all.

## Not yet (Tokumai 2.0)

Retrieval inside the enclave, on-device embeddings, padding blobs to size buckets and dummy
reads, a storage quota paid with a separate note. Known limits now: a wipe on one device
does not reach the copies other devices hold (they keep theirs until they wipe too, and a
change made there uploads again); two devices that give the same placeholder number to two
different values at the same moment keep the older one; the status lines are the app's own,
not frames from the enclave (the enclave answers whole, it does not stream).
