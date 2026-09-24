fn main() {
    // iOS: let the cdylib link with the StoreKit shim's symbols still open.
    //
    // `cargo build --lib` builds every crate type, the cdylib included, and that link step
    // cannot resolve `tokumai_iap_*`: those live in StoreKitShim.swift, which only Xcode
    // compiles. The app never uses the cdylib — Xcode links `libapp.a`, the staticlib, and
    // resolves the Swift symbols there — so the artefact nobody ships may link with them
    // left open.
    //
    // This belongs here rather than in .cargo/config.toml because `tauri ios build` sets
    // RUSTFLAGS itself, and an env RUSTFLAGS makes cargo ignore the config's rustflags
    // altogether — which is exactly how this failed the first time, working from a
    // terminal and breaking under Xcode. A build-script link arg is ADDED to whatever is
    // already there, so it survives.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {
        println!("cargo::rustc-link-arg-cdylib=-Wl,-undefined,dynamic_lookup");
    }
    tauri_build::build()
}
