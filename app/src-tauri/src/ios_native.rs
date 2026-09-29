//! The parts of the app that are native on a phone, because a webview cannot do them —
//! or, in one case, because it must not.
//!
//! Ported from the first generation (08_scrambleai-tokumai), where each was learned
//! against a real device:
//!
//! * **the picker**, because `<input type="file">` in a WKWebView will not reopen after a
//!   cancel, so the "+" button silently dies on the second try;
//! * **the photo library and the share sheet**, which have no desktop counterpart at all;
//! * **opening a link**, because the opener plugin's Swift package is never linked into
//!   the generated Xcode project, so its command fails at runtime on iOS and nowhere else.

pub mod share {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{msg_send, AnyThread, MainThreadMarker};
    use objc2_foundation::{NSArray, NSData, NSError, NSString, NSURL};
    use objc2_photos::{PHAssetCreationRequest, PHAssetResourceCreationOptions, PHAssetResourceType, PHPhotoLibrary};
    use objc2_ui_kit::{UIActivityViewController, UIApplication, UIImage};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot::Sender;

    /// The error in words a person can act on. Photos refuses with code 3311 (PHPhotos,
    /// "access denied") or -3311 (the older ALAssetsLibrary domain) when the app has no
    /// permission to add — the case that used to look like success (2026-09-29).
    fn explain(e: &NSError) -> String {
        let code = e.code();
        let text = e.localizedDescription().to_string();
        if code == 3311 || code == -3311 || text.to_lowercase().contains("denied") {
            "Photos access is off for tokumai. Allow “Add Photos Only” under Settings › tokumai › Photos, then save again.".to_string()
        } else {
            format!("Photos did not take the picture: {text} ({} {code})", e.domain())
        }
    }

    /// How Photos is told what the bytes are.
    fn uti(mime: &str) -> &'static str {
        match mime {
            "image/webp" => "org.webmproject.webp",
            "image/png" => "public.png",
            "image/gif" => "com.compuserve.gif",
            _ => "public.jpeg",
        }
    }

    /// The picture as bytes Photos always accepts, for when it will not take the original:
    /// PNG when every pixel matters, JPEG otherwise. Copied out at once — the NSData UIKit
    /// hands back is autoreleased.
    unsafe fn reencode(bytes: &[u8], lossless: bool) -> Result<Vec<u8>, String> {
        extern "C-unwind" {
            fn UIImageJPEGRepresentation(image: &UIImage, quality: f64) -> *mut AnyObject;
            fn UIImagePNGRepresentation(image: &UIImage) -> *mut AnyObject;
        }
        let data = NSData::with_bytes(bytes);
        let img = UIImage::initWithData(UIImage::alloc(), &data).ok_or("could not decode the image data")?;
        let data: *mut AnyObject = if lossless { UIImagePNGRepresentation(&img) } else { UIImageJPEGRepresentation(&img, 0.95) };
        if data.is_null() {
            return Err("could not re-encode the picture for Photos".into());
        }
        let len: usize = msg_send![data, length];
        let ptr: *const u8 = msg_send![data, bytes];
        if ptr.is_null() || len == 0 {
            return Err("the picture re-encoded to nothing".into());
        }
        Ok(std::slice::from_raw_parts(ptr, len).to_vec())
    }

    /// One creation request: the bytes as a photo resource, typed. `done` hears Photos.
    unsafe fn submit(bytes: Vec<u8>, uti: &'static str, done: RcBlock<dyn Fn(Bool, *mut NSError)>) {
        let library = PHPhotoLibrary::sharedPhotoLibrary();
        let change = RcBlock::new(move || unsafe {
            let resource = NSData::with_bytes(&bytes);
            let options = PHAssetResourceCreationOptions::new();
            options.setUniformTypeIdentifier(Some(&NSString::from_str(uti)));
            // The request registers itself with the library on creation; nothing to keep.
            let req = PHAssetCreationRequest::creationRequestForAsset();
            req.addResourceWithType_data_options(PHAssetResourceType::Photo, &resource, Some(&options));
        });
        let _: () = msg_send![&*library, performChanges: &*change, completionHandler: &*done];
    }

    /// Hand the picture to Photos and report what Photos said.
    ///
    /// Through PhotoKit, not `UIImageWriteToSavedPhotosAlbum`: the old function answers
    /// "Unknown error" for a picture decoded from WebP under an "Add Photos Only"
    /// permission (2026-09-29, iOS 26), and a change request made from that UIImage
    /// answers 3302, "invalid". So the bytes go in **as delivered** — the WebP the enclave
    /// packed for the mixnet, typed as such; Photos keeps WebP — and only if Photos will
    /// not take them are they decoded and handed over again as PNG or JPEG. No picture is
    /// converted twice unless the first way failed. The completion handler carries Photos'
    /// own verdict; before it was heard, a picture that never arrived showed as saved.
    pub fn save_image_to_photos(bytes: &[u8], mime: &str, lossless: bool, tx: Sender<Result<(), String>>) {
        let tx = Arc::new(Mutex::new(Some(tx)));
        let answer = move |tx: &Arc<Mutex<Option<Sender<Result<(), String>>>>>, v: Result<(), String>| {
            if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                let _ = tx.send(v);
            }
        };
        let verdict = |ok: Bool, err: *mut NSError| -> Result<(), String> {
            if ok.as_bool() {
                Ok(())
            } else {
                Err(unsafe { err.as_ref() }.map(explain).unwrap_or_else(|| "Photos did not take the picture, and did not say why".into()))
            }
        };
        // The second try's handler: whatever Photos says now is the answer.
        let finish = {
            let tx = tx.clone();
            RcBlock::new(move |ok: Bool, err: *mut NSError| answer(&tx, verdict(ok, err)))
        };
        // The first try's handler: taken as delivered, or once more re-encoded.
        let original = bytes.to_vec();
        let first = {
            let tx = tx.clone();
            let original = original.clone();
            RcBlock::new(move |ok: Bool, err: *mut NSError| {
                let first = verdict(ok, err);
                if first.is_ok() {
                    answer(&tx, Ok(()));
                    return;
                }
                match unsafe { reencode(&original, lossless) } {
                    Ok(plain) => unsafe { submit(plain, if lossless { "public.png" } else { "public.jpeg" }, finish.clone()) },
                    Err(e) => answer(&tx, Err(format!("{} — and re-encoding failed: {e}", first.unwrap_err()))),
                }
            })
        };
        unsafe { submit(original, uti(mime), first) };
    }

    // keyWindow is deprecated for multi-scene apps; this app is single-scene.
    #[allow(deprecated)]
    pub fn present_share_sheet(path: &str) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let obj: Retained<AnyObject> = Retained::into_super(Retained::into_super(url));
            let items = NSArray::from_retained_slice(&[obj]);
            let avc = UIActivityViewController::initWithActivityItems_applicationActivities(
                mtm.alloc(),
                &items,
                None,
            );
            let app = UIApplication::sharedApplication(mtm);
            let window = app.keyWindow().ok_or("no key window")?;
            let root = window.rootViewController().ok_or("no root view controller")?;
            // iPad presents this as a popover and needs an anchor; iPhone ignores it.
            if let Some(pop) = avc.popoverPresentationController() {
                pop.setSourceView(Some(&window));
            }
            root.presentViewController_animated_completion(&avc, true, None);
        }
        Ok(())
    }
}

pub mod picker {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObjectProtocol};
    use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSObject, NSString};
    use objc2_ui_kit::{
        UIApplication, UIImagePickerController, UIImagePickerControllerDelegate,
        UIImagePickerControllerSourceType, UINavigationControllerDelegate,
    };
    use std::cell::RefCell;
    use tokio::sync::oneshot::Sender;

    // UIKit only weakly references the picker's delegate, so keep the last one alive here
    // (main thread) until the next present() replaces it. Never cleared from inside a
    // delegate callback — that could dealloc `self` mid-method.
    thread_local! {
        static KEEP: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
    }

    pub struct Ivars {
        tx: RefCell<Option<Sender<Result<Option<Vec<u8>>, String>>>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "TokumaiImagePickerDelegate"]
        #[ivars = Ivars]
        struct PickerDelegate;

        unsafe impl NSObjectProtocol for PickerDelegate {}
        unsafe impl UINavigationControllerDelegate for PickerDelegate {}

        unsafe impl UIImagePickerControllerDelegate for PickerDelegate {
            #[unsafe(method(imagePickerController:didFinishPickingMediaWithInfo:))]
            fn did_finish(&self, picker: &UIImagePickerController, info: &AnyObject) {
                let bytes = unsafe { extract_jpeg(info) };
                self.reply(bytes);
                picker.dismissViewControllerAnimated_completion(true, None);
            }

            #[unsafe(method(imagePickerControllerDidCancel:))]
            fn did_cancel(&self, picker: &UIImagePickerController) {
                self.reply(Ok(None));
                picker.dismissViewControllerAnimated_completion(true, None);
            }
        }
    );

    impl PickerDelegate {
        fn new(mtm: MainThreadMarker, tx: Sender<Result<Option<Vec<u8>>, String>>) -> Retained<Self> {
            let this = mtm.alloc::<Self>().set_ivars(Ivars { tx: RefCell::new(Some(tx)) });
            unsafe { msg_send![super(this), init] }
        }
        fn reply(&self, v: Result<Option<Vec<u8>>, String>) {
            if let Some(tx) = self.ivars().tx.borrow_mut().take() {
                let _ = tx.send(v);
            }
        }
    }

    // Pull the original UIImage out of the info dict and JPEG-encode it. Copy the bytes
    // immediately — the returned NSData is autoreleased and only valid during this call.
    unsafe fn extract_jpeg(info: &AnyObject) -> Result<Option<Vec<u8>>, String> {
        let key = NSString::from_str("UIImagePickerControllerOriginalImage");
        let image: *mut AnyObject = msg_send![info, objectForKey: &*key];
        if image.is_null() {
            return Ok(None);
        }
        extern "C-unwind" {
            fn UIImageJPEGRepresentation(image: *mut AnyObject, quality: f64) -> *mut AnyObject;
        }
        let data: *mut AnyObject = UIImageJPEGRepresentation(image, 0.85);
        if data.is_null() {
            return Err("could not JPEG-encode the picked image".into());
        }
        let len: usize = msg_send![data, length];
        let ptr: *const u8 = msg_send![data, bytes];
        if ptr.is_null() || len == 0 {
            return Err("picked image encoded to zero bytes".into());
        }
        Ok(Some(std::slice::from_raw_parts(ptr, len).to_vec()))
    }

    #[allow(deprecated)]
    pub fn present(source: &str, tx: Sender<Result<Option<Vec<u8>>, String>>) {
        let mtm = match MainThreadMarker::new() {
            Some(m) => m,
            None => {
                let _ = tx.send(Err("picker must run on the main thread".into()));
                return;
            }
        };
        let want_camera = source == "camera";
        let src_type = if want_camera {
            UIImagePickerControllerSourceType::Camera
        } else {
            UIImagePickerControllerSourceType::PhotoLibrary
        };
        if !unsafe { UIImagePickerController::isSourceTypeAvailable(src_type, mtm) } {
            let _ = tx.send(Err(if want_camera {
                "no camera available on this device".into()
            } else {
                "photo library unavailable".into()
            }));
            return;
        }
        let app = UIApplication::sharedApplication(mtm);
        let Some(window) = app.keyWindow() else {
            let _ = tx.send(Err("no key window".into()));
            return;
        };
        let Some(root) = window.rootViewController() else {
            let _ = tx.send(Err("no root view controller".into()));
            return;
        };
        let picker = unsafe { UIImagePickerController::new(mtm) };
        unsafe { picker.setSourceType(src_type) };
        let delegate = PickerDelegate::new(mtm, tx);
        // `delegate` is untyped `id` on UIImagePickerController; msg_send passes it directly.
        let _: () = unsafe { msg_send![&*picker, setDelegate: &*delegate] };
        KEEP.with(|k| *k.borrow_mut() = Some(delegate));
        unsafe { root.presentViewController_animated_completion(&picker, true, None) };
    }
}

/// Open a link through UIKit. The opener plugin cannot do this here: its Swift package is
/// not linked into the generated Xcode project, so on iOS — and only on iOS — the plugin's
/// command fails at runtime while every other platform works.
pub fn open_url(url: &str) {
    use objc2::MainThreadMarker;
    use objc2_foundation::{NSDictionary, NSString, NSURL};
    use objc2_ui_kit::UIApplication;
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("[open] not on the main thread — url not opened");
        return;
    };
    let s = NSString::from_str(url);
    let Some(nsurl) = (unsafe { NSURL::URLWithString(&s) }) else {
        log::warn!("[open] UIKit rejected the url");
        return;
    };
    let options = NSDictionary::new();
    unsafe { UIApplication::sharedApplication(mtm).openURL_options_completionHandler(&nsurl, &options, None) };
}

/// The recovery phrase, shown and copied by native code.
///
/// This one is not about what a webview can do — it is about what it must never hold. The
/// 24 words are a bearer secret: whoever reads them owns the account and its balance. A
/// script that got into the page can read anything the page has ever held, so the page is
/// never given them. The sheet below is drawn by UIKit, gated by Face ID or the passcode,
/// and its Copy button writes to the pasteboard from here — local to the device, and with
/// an expiry, so the words do not follow the person around.

/// The recovery phrase, shown and copied by native code.
///
/// This one is not about what a webview can do — it is about what it must never hold. The
/// 24 words are a bearer secret: whoever reads them owns the account and its balance. A
/// script that got into the page can read anything the page has ever held, so the page is
/// never given them. The sheet below is drawn by UIKit, gated by Face ID or the passcode,
/// and its Copy button writes to the pasteboard from here — local to the device, and with
/// an expiry, so the words do not follow the person around.
pub mod secure {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{msg_send, MainThreadMarker};
    use objc2_foundation::{NSArray, NSDate, NSDictionary, NSError, NSNumber, NSString};
    use objc2_local_authentication::{LAContext, LAPolicy};
    use objc2_ui_kit::{
        UIAlertAction, UIAlertActionStyle, UIAlertController, UIAlertControllerStyle, UIApplication,
        UIPasteboard, UIPasteboardOptionExpirationDate, UIPasteboardOptionLocalOnly, UIViewController,
    };
    use std::path::PathBuf;
    use tauri::AppHandle;

    #[allow(deprecated)]
    fn present(mtm: MainThreadMarker, vc: &UIViewController) {
        let app = UIApplication::sharedApplication(mtm);
        if let Some(window) = app.keyWindow() {
            if let Some(root) = window.rootViewController() {
                unsafe { root.presentViewController_animated_completion(vc, true, None) };
            }
        }
    }

    // A plain native alert with a single nil-handler "Done" button.
    fn alert(mtm: MainThreadMarker, title: &str, message: &str) {
        let a = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str(title)),
            Some(&NSString::from_str(message)),
            UIAlertControllerStyle::Alert,
            mtm,
        );
        let done = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Done")),
            UIAlertActionStyle::Default,
            None,
            mtm,
        );
        a.addAction(&done);
        present(mtm, &a);
    }

    /// How long the copied phrase stays on the pasteboard. Long enough to switch to a
    /// password manager and paste, short enough that it is not still there tomorrow.
    const PASTEBOARD_SECS: f64 = 60.0;

    /// Put the phrase on the pasteboard from NATIVE code: the words go from the Rust wallet
    /// to UIPasteboard without ever entering the webview, so the H1 boundary holds while the
    /// user still gets to paste into 1Password. Two options make that copy less dangerous
    /// than a plain one:
    ///   · localOnly      — no Universal Clipboard, so the seed does not hop to the Mac or iPad
    ///   · expirationDate — iOS clears it after a minute, without us having to
    fn copy_phrase(text: &str) {
        let value = NSString::from_str(text);
        let utf8 = NSString::from_str("public.utf8-plain-text");
        let v: &AnyObject = &value;
        let item = NSDictionary::<NSString, AnyObject>::from_slices(&[&*utf8], &[v]);
        let items = NSArray::from_retained_slice(&[item]);

        let local = NSNumber::numberWithBool(true);
        let until = NSDate::dateWithTimeIntervalSinceNow(PASTEBOARD_SECS);
        let (l, u): (&AnyObject, &AnyObject) = (&local, &until);
        let opts = unsafe {
            NSDictionary::<NSString, AnyObject>::from_slices(
                &[UIPasteboardOptionLocalOnly, UIPasteboardOptionExpirationDate],
                &[l, u],
            )
        };
        let pb = unsafe { UIPasteboard::generalPasteboard() };
        unsafe { pb.setItems_options(&items, &opts) };
    }

    // The phrase alert, which unlike `alert` carries a Copy button. The 24 words are not
    // selectable in a UIAlertController — a tester pointed out that reading them off the
    // screen and typing them into a password manager is the whole interaction (2026-09-06)
    // — and the answer is a native copy, not moving the phrase back into the webview.
    fn alert_phrase(mtm: MainThreadMarker, title: &str, phrase: &str) {
        let a = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str(title)),
            Some(&NSString::from_str(phrase)),
            UIAlertControllerStyle::Alert,
            mtm,
        );
        let text = phrase.to_string();
        let handler = RcBlock::new(move |_a: core::ptr::NonNull<UIAlertAction>| copy_phrase(&text));
        let copy = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Copy")),
            UIAlertActionStyle::Default,
            Some(&handler),
            mtm,
        );
        let done = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Done")),
            UIAlertActionStyle::Cancel,
            None,
            mtm,
        );
        a.addAction(&copy);
        a.addAction(&done);
        present(mtm, &a);
    }

    // Biometric-gate (Face ID / Touch ID / passcode), then show the phrase natively. Must be
    // called on the main thread. The LAContext reply lands on a private thread, so we hop
    // back to main (via the AppHandle) to touch UIKit + read the wallet.
    pub fn reveal_phrase(app: AppHandle, dir: PathBuf, title: &'static str) {
        let ctx = unsafe { LAContext::new() };
        let reason = NSString::from_str("Show your recovery phrase");
        let reply = RcBlock::new(move |ok: Bool, _err: *mut NSError| {
            let (app, dir) = (app.clone(), dir.clone());
            let ok = ok.as_bool();
            let _ = app.run_on_main_thread(move || {
                let Some(mtm) = MainThreadMarker::new() else { return };
                if !ok {
                    alert(mtm, "Not verified", "Face ID / passcode was cancelled or failed.");
                    return;
                }
                match crate::profile::load(&dir).mnemonic {
                    Some(m) => alert_phrase(mtm, title, &m),
                    None => alert(mtm, "No account", "No recovery phrase on this device."),
                }
            });
        });
        unsafe {
            let _: () = msg_send![
                &ctx,
                evaluatePolicy: LAPolicy::DeviceOwnerAuthentication,
                localizedReason: &*reason,
                reply: &*reply,
            ];
        }
    }

    // Present the "Account security" action sheet. Its "Reveal recovery phrase" action is a
    // NATIVE button: webview JS can present this sheet but cannot tap the action, so it can
    // neither trigger the biometric prompt nor read the phrase.
    #[allow(deprecated)]
    pub fn open_account_security(app: AppHandle, dir: PathBuf) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let sheet = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str("Account security")),
            Some(&NSString::from_str(
                "Your 24-word recovery phrase is the only way back to your balance. It is shown only on this device.",
            )),
            UIAlertControllerStyle::ActionSheet,
            mtm,
        );
        let handler = RcBlock::new(move |_a: core::ptr::NonNull<UIAlertAction>| {
            reveal_phrase(app.clone(), dir.clone(), "Recovery phrase");
        });
        let reveal = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Reveal recovery phrase")),
            UIAlertActionStyle::Default,
            Some(&handler),
            mtm,
        );
        let cancel = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("Cancel")),
            UIAlertActionStyle::Cancel,
            None,
            mtm,
        );
        sheet.addAction(&reveal);
        sheet.addAction(&cancel);
        // iPad presents an action sheet as a popover and needs an anchor; iPhone ignores it.
        if let Some(pop) = sheet.popoverPresentationController() {
            let uiapp = UIApplication::sharedApplication(mtm);
            if let Some(w) = uiapp.keyWindow() {
                pop.setSourceView(Some(&w));
            }
        }
        let _keep: Option<Retained<UIViewController>> = None;
        present(mtm, &sheet);
    }
}
