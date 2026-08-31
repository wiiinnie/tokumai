// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
  // AppImage runs bundle their own (older) libs against the host's WebKitGTK, and on
  // VMs / older graphics stacks WebKit's DMA-BUF renderer then fails to get an EGL
  // display — the window stays grey (2026-08-31 Kali report). Default those runs to
  // the safe render path; a user-set value always wins.
  #[cfg(target_os = "linux")]
  if std::env::var_os("APPIMAGE").is_some() {
    for key in ["WEBKIT_DISABLE_DMABUF_RENDERER", "WEBKIT_DISABLE_COMPOSITING_MODE"] {
      if std::env::var_os(key).is_none() {
        std::env::set_var(key, "1");
      }
    }
  }
  app_lib::run();
}
