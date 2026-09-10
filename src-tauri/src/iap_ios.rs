// ---------------------------------------------------------------------------
// iap_ios.rs — the Rust side of the StoreKit 2 bridge (gen/apple/.../StoreKitShim.swift).
//
// Each Swift function takes an opaque context pointer and a C callback and answers exactly
// once with a JSON string. Here the context is a boxed oneshot sender: the callback
// reclaims the box, copies the string and sends it; the awaiting command gets a Value.
// A callback that never comes (a StoreKit hang) is bounded by a timeout so the command
// returns; the box is then freed by the late callback, or leaks once — never double-freed.
// ---------------------------------------------------------------------------

use serde_json::Value;
use std::ffi::{c_char, c_void, CStr, CString};
use std::time::Duration;
use tokio::sync::oneshot;

type Cb = extern "C" fn(*mut c_void, *const c_char);

extern "C" {
    fn tokumai_iap_products(ids_json: *const c_char, ctx: *mut c_void, cb: Cb);
    fn tokumai_iap_purchase(product_id: *const c_char, ctx: *mut c_void, cb: Cb);
    fn tokumai_iap_unfinished(ctx: *mut c_void, cb: Cb);
    fn tokumai_iap_finish(transaction_id: *const c_char, ctx: *mut c_void, cb: Cb);
}

extern "C" fn on_done(ctx: *mut c_void, json: *const c_char) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: `ctx` is the Box<oneshot::Sender<String>> `call` leaked for exactly this
    // callback, which Swift invokes once; reclaiming it here is the matching from_raw.
    let tx: Box<oneshot::Sender<String>> = unsafe { Box::from_raw(ctx as *mut oneshot::Sender<String>) };
    let s = if json.is_null() {
        String::new()
    } else {
        // SAFETY: Swift hands a NUL-terminated C string that lives for the callback.
        unsafe { CStr::from_ptr(json) }.to_string_lossy().into_owned()
    };
    let _ = tx.send(s);
}

async fn call(timeout: Duration, f: impl FnOnce(*mut c_void, Cb)) -> Result<Value, String> {
    let (tx, rx) = oneshot::channel::<String>();
    let ctx = Box::into_raw(Box::new(tx)) as *mut c_void;
    f(ctx, on_done);
    let s = tokio::time::timeout(timeout, rx)
        .await
        .map_err(|_| "the App Store did not answer in time".to_string())?
        .map_err(|_| "the App Store call was dropped".to_string())?;
    let v: Value = serde_json::from_str(&s).map_err(|_| "unreadable App Store reply".to_string())?;
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    Ok(v)
}

fn c(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "invalid string for the App Store".to_string())
}

/// `{ products: [{id, displayName, displayPrice, price}] }` for the ids, in order.
pub async fn products(ids: &[String]) -> Result<Value, String> {
    let ids = c(&serde_json::to_string(ids).unwrap_or_else(|_| "[]".into()))?;
    // SAFETY: FFI into the Swift shim; the CString outlives the call (Swift copies it).
    call(Duration::from_secs(60), |ctx, cb| unsafe { tokumai_iap_products(ids.as_ptr(), ctx, cb) }).await
}

/// `{ status: ok|cancelled|pending, transactionId?, productId?, jws? }`. The Apple sheet
/// can sit open for a while (Face ID, a payment method to fix), so the budget is long.
pub async fn purchase(product_id: &str) -> Result<Value, String> {
    let id = c(product_id)?;
    // SAFETY: as above.
    call(Duration::from_secs(600), |ctx, cb| unsafe { tokumai_iap_purchase(id.as_ptr(), ctx, cb) }).await
}

/// `[{transactionId, productId, jws}]` — paid, not yet acknowledged to Apple.
pub async fn unfinished() -> Result<Vec<Value>, String> {
    // SAFETY: as above.
    let v = call(Duration::from_secs(60), |ctx, cb| unsafe { tokumai_iap_unfinished(ctx, cb) }).await?;
    Ok(v.get("transactions").and_then(|t| t.as_array()).cloned().unwrap_or_default())
}

/// Acknowledge to Apple — only after the server has credited it.
pub async fn finish(transaction_id: &str) -> Result<(), String> {
    let id = c(transaction_id)?;
    // SAFETY: as above.
    call(Duration::from_secs(60), |ctx, cb| unsafe { tokumai_iap_finish(id.as_ptr(), ctx, cb) }).await.map(|_| ())
}
