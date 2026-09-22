//! The commercial and safety constants the enclave enforces. They are compiled into the
//! image, so they are part of what the attestation covers: the operator cannot quietly raise
//! a price or loosen a limit without shipping a new, published measurement. The price list
//! itself is `pricing.json` at the repository root, embedded the same way.
//!
//! (The first server read the margin and most limits from its environment, which the
//! operator controls — the reason "a signed price list" was on its to-do list. Attestation
//! is that signature.)

/// What we charge over the provider's price. One figure for every model and every rail.
pub const MARGIN: f64 = 1.15;
/// A request that reached a provider costs at least this much (TOKU).
pub const MIN_CHARGE_TOKU: u64 = 1;
/// The price list, as published with this image.
pub const PRICING_JSON: &str = include_str!("../../../pricing.json");

/// Visible-answer budget when the app names none, and the hard cap on what it may name.
pub const DEFAULT_MAX_TOKENS: u64 = 4096;
pub const MAX_OUTPUT_TOKENS: u64 = 131_072;
/// Thinking budget when the app names none, and the cap (reached by OpenAI's "high").
pub const DEFAULT_THINKING: u64 = 2048;
pub const MAX_THINKING: u64 = 16_384;

/// Bounds on one chat request: bytes (inline images are base64) and messages.
pub const MAX_REQUEST_BYTES: usize = 48 * 1024 * 1024;
pub const MAX_MESSAGES: usize = 2_000;
/// Attachments the providers actually read; anything else is refused before it is sent.
/// (Audit 2026-09-21, M4: Gemini forwarded any type inline, audio and video included, and
/// a 4,096-token guess per attachment reserved a fraction of what they cost.)
pub const ATTACHMENT_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "application/pdf"];
/// Largest single attachment, decoded.
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
/// Input tokens an image can cost at most (a high-detail picture on either provider).
pub const IMAGE_INPUT_TOKENS: u64 = 1_600;

/// Web search while answering ("live"): at most this many queries per turn are reserved,
/// each at the provider's price.
pub const MAX_SEARCH_QUERIES: u64 = 10;
pub const GEMINI_USD_PER_QUERY: f64 = 0.014;
pub const OPENAI_USD_PER_QUERY: f64 = 0.01;

/// Declines (moderation or the provider's own policy) an account may collect per UTC day at
/// one provider before that provider refuses it until tomorrow.
pub const STRIKES_PER_DAY: u32 = 3;

/// The App Store: the app's bundle id and product ids. Compiled in, like everything that
/// decides what a purchase is.
pub const APPLE_BUNDLE_ID: &str = "com.tokumai.app";
/// Plans: `<prefix><euro>`, `.year` appended for the yearly version.
pub const APPLE_PLAN_PREFIX: &str = "com.tokumai.app.plan.";
/// Prepaid credit: `<prefix><amount>`.
pub const APPLE_CREDIT_PREFIX: &str = "com.tokumai.app.credit.";
/// Prepaid tiles on the App Store. Amounts to be settled with the prepaid prices (3 years).
pub const APPLE_CREDIT_TILES: &[u32] = &[10, 20, 50];
/// Sandbox purchases (TestFlight, Xcode) count only in a development build, or an image
/// built with the `apple-sandbox` feature — which has its own, published measurement. A
/// production image can never be talked into accepting them.
pub const APPLE_SANDBOX: bool = cfg!(any(debug_assertions, feature = "apple-sandbox"));
