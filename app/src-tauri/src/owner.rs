//! The person at the device proves they own it before the recovery phrase leaves the
//! keystore for the screen (audit M15, 2026-10-09). An unlocked laptop left for a minute,
//! a phone in another hand: three taps gave the 24 words, and with them the account.
//!
//! - macOS: Touch ID or the login password (LocalAuthentication, `DeviceOwnerAuthentication`).
//! - Android: biometrics or the device credential (`BiometricPrompt`, Owner.kt). A phone
//!   with no screen lock at all has nothing to prove with and is let through.
//! - iOS has its own path: the words are drawn by UIKit behind Face ID (`ios_native`).
//! - Windows and Linux: not yet (the system's own prompt is the next step; a password
//!   field in the page is not an option — it would hand the login password to the webview).

use tauri::AppHandle;

/// Ok when the owner was verified, Err with the reason otherwise. Never asks for a
/// password in the page.
pub async fn confirm(app: &AppHandle, reason: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        return macos::confirm(reason).await;
    }
    #[cfg(target_os = "android")]
    {
        let _ = app;
        return android::confirm(reason).await;
    }
    #[allow(unreachable_code)]
    {
        let _ = (app, reason);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_local_authentication::{LAContext, LAPolicy};

    /// On a thread of its own: the context and the block are not Send, and the reply
    /// comes on a private thread of the system's — nothing here needs the runtime.
    pub async fn confirm(reason: &str) -> Result<(), String> {
        let reason = reason.to_string();
        tokio::task::spawn_blocking(move || {
            let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let tx = std::sync::Mutex::new(Some(tx));
            let ctx = unsafe { LAContext::new() };
            let reason = NSString::from_str(&reason);
            let reply = RcBlock::new(move |ok: Bool, err: *mut NSError| {
                let outcome = if ok.as_bool() {
                    Ok(())
                } else {
                    let why = if err.is_null() { "not verified".to_string() } else { unsafe { (*err).localizedDescription().to_string() } };
                    Err(format!("Not verified: {why}"))
                };
                if let Ok(mut t) = tx.lock() {
                    if let Some(t) = t.take() {
                        let _ = t.send(outcome);
                    }
                }
            });
            unsafe { ctx.evaluatePolicy_localizedReason_reply(LAPolicy::DeviceOwnerAuthentication, &reason, &reply) };
            // The context stays alive until the reply: it is what the system is evaluating.
            let outcome = rx.recv_timeout(std::time::Duration::from_secs(180)).unwrap_or_else(|_| Err("Not verified: no answer".into()));
            drop(ctx);
            outcome
        })
        .await
        .map_err(|e| format!("owner check: {e}"))?
    }
}

#[cfg(target_os = "android")]
mod android {
    use jni::objects::JValue;
    use jni::signature::{MethodSignature, RuntimeMethodSignature};
    use jni::strings::JNIString;

    const OWNER: &str = "com/tokumai/app/Owner";

    pub async fn confirm(reason: &str) -> Result<(), String> {
        let vm = crate::keystore::android::vm()?;
        let activity = crate::keystore::android::activity()?;
        vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            let reason = env.new_string(reason)?;
            let sig = RuntimeMethodSignature::from_str("(Landroid/app/Activity;Ljava/lang/String;)V")?;
            env.call_static_method(JNIString::from(OWNER), JNIString::from("start"), MethodSignature::from(&sig), &[JValue::Object(activity.as_obj()), JValue::Object(&reason)])?;
            Ok(())
        })
        .map_err(|e: jni::errors::Error| format!("owner check: {e}"))?;
        // The prompt answers on the UI thread; this side looks in every so often. Two
        // minutes is longer than anyone takes to decide.
        let started = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let state = vm
                .attach_current_thread(|env| -> Result<i32, jni::errors::Error> {
                    let sig = RuntimeMethodSignature::from_str("()I")?;
                    env.call_static_method(JNIString::from(OWNER), JNIString::from("poll"), MethodSignature::from(&sig), &[])?.i()
                })
                .map_err(|e: jni::errors::Error| format!("owner check: {e}"))?;
            match state {
                1 => return Ok(()),
                2 => return Err("Not verified".into()),
                _ if started.elapsed() > std::time::Duration::from_secs(120) => return Err("Not verified: no answer".into()),
                _ => {}
            }
        }
    }
}
