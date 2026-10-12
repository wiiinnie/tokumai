//! Speak instead of type (2026-10-12). The phone's own speech recogniser turns the words
//! into text, and the text goes to the enclave exactly as if it had been typed. The
//! audio never leaves the device and is never written down.
//!
//! The promise is enforced, not hoped for: iOS runs `SFSpeechRecognizer` with
//! `requiresOnDeviceRecognition` (Voice.swift), Android the on-device recogniser of
//! Android 12 (`createOnDeviceSpeechRecognizer`, Voice.kt). Where there is no on-device
//! model for the language, `voice_available` says so and the microphone is not shown —
//! the alternative would be a recogniser that quietly sends the voice to Apple or Google,
//! which is the one thing this feature must not do. (The published tauri-plugin-stt does
//! exactly that on Android, and falls back to it on iOS; hence this, 2026-10-12.)
//!
//! Desktop: not yet. macOS has the same Speech framework; Windows and Linux would need a
//! local model (whisper). Until then `voice_available` is false there.
//!
//! The interface polls: `voice_poll` a dozen times a second gives {state, text, level},
//! the level drives the waveform. States: idle, permission, listening, stopping, final,
//! error.

use serde_json::{json, Value};

#[cfg(target_os = "ios")]
mod ios {
    use serde_json::Value;
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::time::Duration;
    use tokio::sync::oneshot;

    type Cb = extern "C" fn(*mut c_void, *const c_char);

    extern "C" {
        fn tokumai_voice_available(lang: *const c_char, ctx: *mut c_void, cb: Cb);
        fn tokumai_voice_start(lang: *const c_char, ctx: *mut c_void, cb: Cb);
        fn tokumai_voice_poll(ctx: *mut c_void, cb: Cb);
        fn tokumai_voice_stop(ctx: *mut c_void, cb: Cb);
        fn tokumai_voice_cancel(ctx: *mut c_void, cb: Cb);
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
            .map_err(|_| "the speech recogniser did not answer in time".to_string())?
            .map_err(|_| "the speech call was dropped".to_string())?;
        let v: Value = serde_json::from_str(&s).map_err(|_| "unreadable speech reply".to_string())?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(e.to_string());
        }
        Ok(v)
    }

    fn c(s: &str) -> Result<CString, String> {
        CString::new(s).map_err(|_| "invalid language tag".to_string())
    }

    pub async fn available(lang: &str) -> Result<Value, String> {
        let l = c(lang)?;
        // SAFETY: FFI into the Swift shim; the CString outlives the call (Swift copies it).
        call(Duration::from_secs(5), |ctx, cb| unsafe { tokumai_voice_available(l.as_ptr(), ctx, cb) }).await
    }

    /// The two system prompts (speech recognition, microphone) may sit open a while.
    pub async fn start(lang: &str) -> Result<Value, String> {
        let l = c(lang)?;
        // SAFETY: as above.
        call(Duration::from_secs(120), |ctx, cb| unsafe { tokumai_voice_start(l.as_ptr(), ctx, cb) }).await
    }

    pub async fn poll() -> Result<Value, String> {
        // SAFETY: as above; `poll` answers synchronously.
        call(Duration::from_secs(5), |ctx, cb| unsafe { tokumai_voice_poll(ctx, cb) }).await
    }

    pub async fn stop() -> Result<Value, String> {
        // SAFETY: as above.
        call(Duration::from_secs(5), |ctx, cb| unsafe { tokumai_voice_stop(ctx, cb) }).await
    }

    pub async fn cancel() -> Result<Value, String> {
        // SAFETY: as above.
        call(Duration::from_secs(5), |ctx, cb| unsafe { tokumai_voice_cancel(ctx, cb) }).await
    }
}

#[cfg(target_os = "android")]
mod android {
    use jni::objects::JValue;
    use jni::signature::{MethodSignature, RuntimeMethodSignature};
    use jni::strings::JNIString;
    use serde_json::Value;

    const VOICE: &str = "com/tokumai/app/Voice";

    /// A static method of Voice.kt that answers with UTF-8 JSON as a byte array.
    fn ask(method: &str, sig: &str, with_activity: bool, text: Option<&str>) -> Result<Value, String> {
        let vm = crate::keystore::android::vm()?;
        let activity = if with_activity { Some(crate::keystore::android::activity()?) } else { None };
        let bytes = vm
            .attach_current_thread(|env| -> Result<Vec<u8>, jni::errors::Error> {
                let sig = RuntimeMethodSignature::from_str(sig)?;
                let mut args: Vec<JValue> = Vec::new();
                if let Some(a) = &activity {
                    args.push(JValue::Object(a.as_obj()));
                }
                let s = match text {
                    Some(t) => Some(env.new_string(t)?),
                    None => None,
                };
                if let Some(s) = &s {
                    args.push(JValue::Object(s));
                }
                let out = env.call_static_method(JNIString::from(VOICE), JNIString::from(method), MethodSignature::from(&sig), &args)?.l()?;
                let out = env.cast_local::<jni::objects::JByteArray>(out)?;
                env.convert_byte_array(&out)
            })
            .map_err(|e| format!("voice ({method}): {e}"))?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| "unreadable speech reply".to_string())?;
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return Err(e.to_string());
        }
        Ok(v)
    }

    /// A static method with nothing to say back.
    fn tell(method: &str) -> Result<(), String> {
        let vm = crate::keystore::android::vm()?;
        vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
            let sig = RuntimeMethodSignature::from_str("()V")?;
            env.call_static_method(JNIString::from(VOICE), JNIString::from(method), MethodSignature::from(&sig), &[])?;
            Ok(())
        })
        .map_err(|e| format!("voice ({method}): {e}"))
    }

    pub async fn available(_lang: &str) -> Result<Value, String> {
        ask("available", "(Landroid/app/Activity;)[B", true, None)
    }

    pub async fn start(lang: &str) -> Result<Value, String> {
        ask("start", "(Landroid/app/Activity;Ljava/lang/String;)[B", true, Some(lang))
    }

    pub async fn poll() -> Result<Value, String> {
        let v = ask("poll", "()[B", false, None)?;
        // An error state is an error answer, as on iOS: the interface sees one shape.
        if v["state"] == "error" {
            return Err(v["error"].as_str().unwrap_or("could not listen").to_string());
        }
        Ok(v)
    }

    pub async fn stop() -> Result<Value, String> {
        tell("stop").map(|()| serde_json::json!({ "ok": true }))
    }

    pub async fn cancel() -> Result<Value, String> {
        tell("cancel").map(|()| serde_json::json!({ "ok": true }))
    }
}

#[cfg(target_os = "ios")]
use ios as native;
#[cfg(target_os = "android")]
use android as native;

/// `{ available, lang?, why? }` — asked once at start; the microphone is shown only on a
/// yes. `lang` empty means the device's own language.
#[tauri::command]
pub async fn voice_available() -> Result<Value, String> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        return native::available("").await;
    }
    #[allow(unreachable_code)]
    Ok(json!({ "available": false, "why": "speaking a question comes to the desktop with a later version" }))
}

#[tauri::command]
pub async fn voice_start(lang: Option<String>) -> Result<Value, String> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        return native::start(lang.as_deref().unwrap_or("")).await;
    }
    #[allow(unreachable_code)]
    {
        let _ = lang;
        Err("speaking a question is not available on this computer yet".into())
    }
}

/// `{ state, text, level }`; an error state comes back as the error itself.
#[tauri::command]
pub async fn voice_poll() -> Result<Value, String> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        return native::poll().await;
    }
    #[allow(unreachable_code)]
    Err("not listening".into())
}

#[tauri::command]
pub async fn voice_stop() -> Result<Value, String> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        return native::stop().await;
    }
    #[allow(unreachable_code)]
    Ok(json!({ "ok": true }))
}

#[tauri::command]
pub async fn voice_cancel() -> Result<Value, String> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        return native::cancel().await;
    }
    #[allow(unreachable_code)]
    Ok(json!({ "ok": true }))
}
