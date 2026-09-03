# Abuse policy (for provider inquiries)

tokumai relays chat requests from anonymous users to AI providers over the Nym mixnet.
We cannot identify a person: purchases are unlinkable from usage by design (blind-signed
ecash), and usage is keyed by a per-session pseudonym the user can rotate.

What we do enforce, automatically, on every server:

1. **Moderation prefilter** (OpenAI-routed chats): the user's latest turn is checked with
   the provider's moderation endpoint before the model call; flagged input is declined.
2. **Strikes**: every policy decline — by a provider or by the prefilter — counts against
   the session. After `ABUSE_STRIKES_PER_DAY` declines (default 3) in a UTC day, the session
   is refused for the rest of that day; its remaining credit stays unusable meanwhile.
3. **Per-user identifiers to OpenAI**: a daily-rotating hash of the session, so the
   provider can act on one user's abuse instead of the whole service. It never carries an
   account, name, address or payment detail.

On an inquiry naming a safety identifier and a date we can confirm: whether it exists in
our logs for that day, how many strikes it collected, and that the session was paused.
We cannot name, contact or permanently ban the person behind it; the cost of an offense
is the rest of their current credit chunk and a day of service.
