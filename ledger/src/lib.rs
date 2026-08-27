// ---------------------------------------------------------------------------
// scrai-ledger — the shared "guest book": the seed-recoverable, cross-server value
// layer (redeemed session credit), reachable over the MIXNET, never a network port.
//
//   store   — the hardened local SQLite database (the guest book itself)
//   proto   — the signed request/reply wire format + the member-key crypto ACL
//   service — the request handler a Nym service-provider loop calls
//   mixnet  — MixnetLedger: a chat server's client stub for a remote ledger-service
//
// Only the INFREQUENT operations live here (credit/status/balance). Per-message chat
// billing stays LOCAL to the chat server and must never pay a mixnet round-trip.
// See docs/federation-shared-ledger.md.
// ---------------------------------------------------------------------------

pub mod mixnet;
pub mod proto;
pub mod service;
pub mod store;
