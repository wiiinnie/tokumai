// tokumai shared core — the byte-identical crypto/protocol used by both the
// desktop client (src-tauri) and the server binary (server).
//
// - coconut:    threshold ecash — client/authority/verifier/quorum crypto roles
// - quorum:     double-spend detection store + graduated blacklist
// - federation: the wire protocol + authority-side request handlers
pub mod auth;
pub mod billing;
pub mod coconut;
pub mod federation;
pub mod gateway;
pub mod ledger;
pub mod pricing;
pub mod purse;
pub mod quorum;
pub mod session;
