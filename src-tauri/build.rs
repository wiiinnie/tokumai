fn main() {
    // iOS: the StoreKit 2 bridge (gen/apple/.../StoreKitShim.swift) defines four C symbols
    // that only exist once Xcode links the app. Cargo also builds this crate as a cdylib,
    // which links on its own and would fail on them — so for that (unused on iOS) artifact
    // the linker is told these four may stay undefined. The staticlib Xcode consumes is
    // unaffected and resolves them against the Swift objects.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        for sym in ["tokumai_iap_products", "tokumai_iap_purchase", "tokumai_iap_unfinished", "tokumai_iap_finish"] {
            println!("cargo:rustc-link-arg-cdylib=-Wl,-U,_{sym}");
        }
    }
    tauri_build::build()
}
