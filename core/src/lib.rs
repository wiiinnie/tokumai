// tokumai shared core — the byte-identical crypto/protocol used by both the
// desktop client (src-tauri) and the server binary (server).
//
// - coconut:    threshold ecash — client/authority/verifier/quorum crypto roles
// - quorum:     double-spend detection store + graduated blacklist
// - federation: the wire protocol + authority-side request handlers
/// What a ticketbook size costs, measured. Tests only — see the module header.
#[cfg(test)]
mod bench_books;
pub mod auth;
pub mod billing;
pub mod coconut;
pub mod federation;
pub mod gateway;
pub mod pricing;
pub mod purse;
pub mod quorum;
pub mod subscription;
pub mod tender;
