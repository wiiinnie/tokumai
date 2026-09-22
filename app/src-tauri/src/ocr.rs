// ---------------------------------------------------------------------------
// ocr.rs — on-device text recognition for the privacy guard.
//
// THE SEAM: `recognize(bytes)` (an image) and `recognize_pdf(bytes)` (a scanned
// PDF, rendered page-by-page), each returning plain text. macOS uses Apple
// Vision (on-device, offline); scanned PDFs are rendered with CoreGraphics. Other
// platforms return `Err("ocr-unsupported-platform")`, the frontend's cue to fall
// back to bundled Tesseract-WASM (images) / a clear message (scanned PDFs).
//
// Adding a platform later = add a `#[cfg(...)]` branch here. Nothing above this
// file changes — the guard's detectors (guard.js) run on the returned text.
// ---------------------------------------------------------------------------

/// One recognised text run with its normalized (0..1, TOP-LEFT origin) box.
#[derive(serde::Serialize)]
pub struct TextBox {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Vision's confidence, 0..1. The guard uses it to tell "read it, nothing sensitive"
    /// from "could not read this at all" — silence must not mean both.
    pub conf: f64,
}

/// Recognise text in an encoded image (PNG/JPEG bytes) WITH per-line boxes, so
/// the guard can highlight/redact exactly where a match is.
pub fn recognize(image: &[u8]) -> Result<Vec<TextBox>, String> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        apple::recognize_image(image)
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let _ = image;
        Err("ocr-unsupported-platform".to_string())
    }
}

/// Recognise text in a scanned/image-only PDF by rendering its pages and OCR'ing.
pub fn recognize_pdf(pdf: &[u8]) -> Result<String, String> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        apple::recognize_pdf(pdf)
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let _ = pdf;
        Err("ocr-unsupported-platform".to_string())
    }
}

/// A rendered PDF page: its PNG image plus the text boxes on it — everything the
/// redaction UI needs to show the page and black out the matches.
pub struct PdfPage {
    pub png: Vec<u8>,
    pub boxes: Vec<TextBox>,
}

/// Render each PDF page to a PNG and recognise its text boxes (for redaction).
pub fn recognize_pdf_pages(pdf: &[u8]) -> Result<Vec<PdfPage>, String> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        apple::recognize_pdf_pages(pdf)
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let _ = pdf;
        Err("ocr-unsupported-platform".to_string())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple {
    use objc2::rc::Retained;
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_image_io::CGImagePropertyOrientation;
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedTextObservation,
        VNRequestTextRecognitionLevel,
    };

    fn empty_options() -> Retained<NSDictionary<NSString>> {
        NSDictionary::new()
    }

    /// Run a text-recognition request against a prepared handler and return each
    /// recognised line with its normalized (top-left origin) box and Vision's own
    /// confidence — which is what tells an upside-down misread from a real line.
    fn boxes_from_handler(
        handler: &VNImageRequestHandler,
        level: VNRequestTextRecognitionLevel,
    ) -> Result<Vec<super::TextBox>, String> {
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(level);
        request.setUsesLanguageCorrection(true);
        let requests = NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(
            request.clone(),
        ))]);
        handler
            .performRequests_error(&requests)
            .map_err(|e| format!("vision perform failed: {e:?}"))?;
        let mut out = Vec::new();
        if let Some(results) = request.results() {
            for obs in results.iter() {
                if let Some(o) = obs.downcast_ref::<VNRecognizedTextObservation>() {
                    let cands = o.topCandidates(1);
                    if let Some(top) = cands.firstObject() {
                        // Vision boxes are normalized with a BOTTOM-LEFT origin; flip
                        // to top-left for the canvas.
                        let bb = unsafe { o.boundingBox() };
                        out.push(super::TextBox {
                            text: top.string().to_string(),
                            x: bb.origin.x,
                            y: 1.0 - (bb.origin.y + bb.size.height),
                            w: bb.size.width,
                            h: bb.size.height,
                            conf: top.confidence() as f64,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    /// The four right-angle readings of a page. Vision reads horizontal text in the
    /// orientation it is given and never looks for a rotated one — so a letter whose
    /// ADDRESS LABEL was stuck on upside down comes back with the letter recognised and
    /// the label missing entirely, which is exactly the case that started this
    /// (2026-09-06). Sideways labels are just as common on parcels and receipts.
    const ORIENTATIONS: [CGImagePropertyOrientation; 4] = [
        CGImagePropertyOrientation::Up,
        CGImagePropertyOrientation::Down,
        CGImagePropertyOrientation::Right,
        CGImagePropertyOrientation::Left,
    ];

    /// Map a box from the upright frame Vision reported it in back to the ORIGINAL
    /// buffer, so boxes from different passes share one coordinate system (the webview
    /// draws highlight rectangles from these, and the guard sorts them into lines).
    /// Normalized coordinates are relative to each frame's own size, so the 90° cases
    /// swap width and height and no aspect ratio is needed.
    fn to_buffer_frame(b: &mut super::TextBox, o: CGImagePropertyOrientation) {
        let (x, y, w, h) = (b.x, b.y, b.w, b.h);
        let (nx, ny, nw, nh) = match o {
            CGImagePropertyOrientation::Down => (1.0 - x - w, 1.0 - y - h, w, h),
            CGImagePropertyOrientation::Right => (y, 1.0 - x - w, h, w),
            CGImagePropertyOrientation::Left => (1.0 - y - h, x, h, w),
            _ => (x, y, w, h),
        };
        b.x = nx;
        b.y = ny;
        b.w = nw;
        b.h = nh;
    }

    /// Below this, a "line" is almost always the recogniser hallucinating words out of
    /// text that is upside down in this pass — the same region read the right way up
    /// scores far higher, and the overlap rule then keeps the better one.
    const MIN_CONF: f64 = 0.35;
    /// A probe pass counts as "there is text at this angle" above this many characters.
    const PROBE_CHARS: usize = 12;

    fn handler_for(
        data: &NSData,
        o: CGImagePropertyOrientation,
    ) -> Retained<VNImageRequestHandler> {
        unsafe {
            VNImageRequestHandler::initWithData_orientation_options(
                VNImageRequestHandler::alloc(),
                data,
                o,
                &empty_options(),
            )
        }
    }

    /// Two boxes cover the same part of the page? Then they are the same text read at two
    /// angles; keep the confident one.
    fn overlaps(a: &super::TextBox, b: &super::TextBox) -> bool {
        let ix = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let iy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        if ix <= 0.0 || iy <= 0.0 {
            return false;
        }
        let inter = ix * iy;
        let smaller = (a.w * a.h).min(b.w * b.h);
        smaller > 0.0 && inter / smaller > 0.5
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn bx(x: f64, y: f64, w: f64, h: f64) -> super::super::TextBox {
            super::super::TextBox { text: "x".into(), x, y, w, h, conf: 1.0 }
        }
        fn tup(b: &super::super::TextBox) -> (f64, f64, f64, f64) {
            ((b.x * 1e6).round() / 1e6, (b.y * 1e6).round() / 1e6,
             (b.w * 1e6).round() / 1e6, (b.h * 1e6).round() / 1e6)
        }

        // A box Vision reported in the upright frame has to land where the text actually
        // is in the ORIGINAL buffer, or the highlight rectangles point at the wrong part
        // of the page and the line sort comes out scrambled.
        #[test]
        fn maps_each_rotation_back_to_the_buffer() {
            let mut b = bx(0.1, 0.2, 0.3, 0.4);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Up);
            assert_eq!(tup(&b), (0.1, 0.2, 0.3, 0.4), "upright is untouched");

            let mut b = bx(0.1, 0.2, 0.3, 0.4);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Down);
            assert_eq!(tup(&b), (0.6, 0.4, 0.3, 0.4), "180° flips both axes, keeps w/h");

            let mut b = bx(0.1, 0.2, 0.3, 0.4);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Right);
            assert_eq!(tup(&b), (0.2, 0.6, 0.4, 0.3), "90° swaps w/h");
        }

        #[test]
        fn rotations_are_invertible() {
            // Down twice, and Right-then-Left, must both be the identity.
            let mut b = bx(0.13, 0.27, 0.31, 0.09);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Down);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Down);
            assert_eq!(tup(&b), (0.13, 0.27, 0.31, 0.09));

            let mut b = bx(0.13, 0.27, 0.31, 0.09);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Right);
            to_buffer_frame(&mut b, CGImagePropertyOrientation::Left);
            assert_eq!(tup(&b), (0.13, 0.27, 0.31, 0.09));
        }

        // The upside-down pass reads the RIGHT-way-up text as garbage in the same place.
        // Whichever pass read a region better must be the one that survives.
        #[test]
        fn overlapping_reads_are_one_region() {
            let a = bx(0.10, 0.10, 0.30, 0.05);
            let b = bx(0.11, 0.105, 0.29, 0.05);
            assert!(overlaps(&a, &b), "the same line read twice");
            let far = bx(0.60, 0.60, 0.30, 0.05);
            assert!(!overlaps(&a, &far), "different parts of the page");
        }
    }

    pub fn recognize_image(image: &[u8]) -> Result<Vec<super::TextBox>, String> {
        let data = NSData::with_bytes(image);

        // 1. Probe all four angles CHEAPLY. `Fast` is about an order of magnitude less
        //    work than `Accurate`, so four probes cost less than one real pass — and the
        //    probe also answers "is there any readable text at all", which is what lets
        //    the guard say "I could not read this" instead of staying silent.
        let mut worth = Vec::new();
        for o in ORIENTATIONS {
            let found = boxes_from_handler(&handler_for(&data, o), VNRequestTextRecognitionLevel::Fast)
                .unwrap_or_default();
            let chars: usize = found
                .iter()
                .filter(|b| b.conf >= MIN_CONF)
                .map(|b| b.text.chars().count())
                .sum();
            if chars >= PROBE_CHARS {
                worth.push((o, chars));
            }
        }
        // Nothing anywhere: still read once upright, so a page with a handful of words
        // is not thrown away by the probe threshold.
        if worth.is_empty() {
            worth.push((CGImagePropertyOrientation::Up, 0));
        }
        // Strongest angle first, so its boxes win the overlap comparison by default.
        worth.sort_by(|a, b| b.1.cmp(&a.1));

        // 2. Read properly, but only at the angles that showed something.
        let mut out: Vec<super::TextBox> = Vec::new();
        for (o, _) in worth {
            let mut found =
                boxes_from_handler(&handler_for(&data, o), VNRequestTextRecognitionLevel::Accurate)?;
            for b in found.iter_mut() {
                to_buffer_frame(b, o);
            }
            for b in found {
                if b.conf < MIN_CONF || b.text.trim().is_empty() {
                    continue;
                }
                match out.iter().position(|e| overlaps(e, &b)) {
                    Some(i) if out[i].conf >= b.conf => {}
                    Some(i) => out[i] = b,
                    None => out.push(b),
                }
            }
        }
        Ok(out)
    }

    pub fn recognize_pdf(pdf: &[u8]) -> Result<String, String> {
        use core::ffi::c_void;
        use core::ptr::NonNull;
        use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
        use objc2_core_graphics::{
            CGBitmapContextCreateImage, CGColorSpace, CGContext, CGDataProvider, CGImageAlphaInfo,
            CGPDFDocument,
        };

        // The classic bitmap-context constructor isn't wrapped safely in this crate;
        // declare it directly (CoreGraphics is already linked).
        extern "C-unwind" {
            fn CGBitmapContextCreate(
                data: *mut c_void,
                width: usize,
                height: usize,
                bits_per_component: usize,
                bytes_per_row: usize,
                space: Option<&CGColorSpace>,
                bitmap_info: u32,
            ) -> Option<NonNull<CGContext>>;
        }

        let cfdata = unsafe { CFData::new(None, pdf.as_ptr(), pdf.len() as isize) }
            .ok_or("could not wrap PDF data")?;
        let provider =
            CGDataProvider::with_cf_data(Some(&cfdata)).ok_or("cgdataprovider failed")?;
        let doc = CGPDFDocument::with_provider(Some(&*provider)).ok_or("not a valid PDF")?;
        let pages = CGPDFDocument::number_of_pages(Some(&*doc)).min(10);
        let scale = 2.0f64;
        let mut out = String::new();

        for i in 1..=pages {
            let Some(page) = CGPDFDocument::page(Some(&*doc), i) else { continue };
            let media = page_box_rect(&page);
            let w = (media.size.width * scale).ceil() as usize;
            let h = (media.size.height * scale).ceil() as usize;
            if w == 0 || h == 0 {
                continue;
            }
            let cs = CGColorSpace::new_device_rgb().ok_or("colorspace")?;
            let ctx = unsafe {
                CGBitmapContextCreate(
                    core::ptr::null_mut(),
                    w,
                    h,
                    8,
                    w * 4,
                    Some(&*cs),
                    CGImageAlphaInfo::PremultipliedLast.0 as u32,
                )
            }
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or("bitmap context")?;

            // White background, upscale, draw the page.
            CGContext::set_rgb_fill_color(Some(&*ctx), 1.0, 1.0, 1.0, 1.0);
            CGContext::fill_rect(
                Some(&*ctx),
                CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)),
            );
            CGContext::scale_ctm(Some(&*ctx), scale, scale);
            CGContext::draw_pdf_page(Some(&*ctx), Some(&*page));

            let Some(cgimg) = CGBitmapContextCreateImage(Some(&*ctx)) else { continue };
            out.push_str(&recognize_cgimage_text(&cgimg)?);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn recognize_pdf_pages(pdf: &[u8]) -> Result<Vec<super::PdfPage>, String> {
        use core::ffi::c_void;
        use core::ptr::NonNull;
        use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGRect, CGSize};
        use objc2_core_graphics::{
            CGBitmapContextCreateImage, CGBitmapContextGetData, CGColorSpace, CGContext,
            CGDataProvider, CGImageAlphaInfo, CGPDFDocument,
        };
        extern "C-unwind" {
            fn CGBitmapContextCreate(
                data: *mut c_void,
                width: usize,
                height: usize,
                bits_per_component: usize,
                bytes_per_row: usize,
                space: Option<&CGColorSpace>,
                bitmap_info: u32,
            ) -> Option<NonNull<CGContext>>;
        }

        let cfdata = unsafe { CFData::new(None, pdf.as_ptr(), pdf.len() as isize) }
            .ok_or("could not wrap PDF data")?;
        let provider = CGDataProvider::with_cf_data(Some(&cfdata)).ok_or("cgdataprovider failed")?;
        let doc = CGPDFDocument::with_provider(Some(&*provider)).ok_or("not a valid PDF")?;
        let n = CGPDFDocument::number_of_pages(Some(&*doc)).min(10);
        let scale = 2.0f64;
        let mut out = Vec::new();

        for i in 1..=n {
            let Some(page) = CGPDFDocument::page(Some(&*doc), i) else { continue };
            let media = page_box_rect(&page);
            let w = (media.size.width * scale).ceil() as usize;
            let h = (media.size.height * scale).ceil() as usize;
            if w == 0 || h == 0 {
                continue;
            }
            let cs = CGColorSpace::new_device_rgb().ok_or("colorspace")?;
            let ctx = unsafe {
                CGBitmapContextCreate(
                    core::ptr::null_mut(),
                    w,
                    h,
                    8,
                    w * 4,
                    Some(&*cs),
                    CGImageAlphaInfo::PremultipliedLast.0 as u32,
                )
            }
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or("bitmap context")?;
            CGContext::set_rgb_fill_color(Some(&*ctx), 1.0, 1.0, 1.0, 1.0);
            CGContext::fill_rect(
                Some(&*ctx),
                CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)),
            );
            CGContext::scale_ctm(Some(&*ctx), scale, scale);
            CGContext::draw_pdf_page(Some(&*ctx), Some(&*page));

            let Some(cgimg) = CGBitmapContextCreateImage(Some(&*ctx)) else { continue };
            let handler = unsafe {
                VNImageRequestHandler::initWithCGImage_options(
                    VNImageRequestHandler::alloc(),
                    &cgimg,
                    &empty_options(),
                )
            };
            let boxes = boxes_from_handler(&handler, VNRequestTextRecognitionLevel::Accurate)?;

            let data = CGBitmapContextGetData(Some(&*ctx));
            if data.is_null() {
                continue;
            }
            let buf = unsafe { std::slice::from_raw_parts(data as *const u8, w * h * 4) };
            let img = image::RgbaImage::from_raw(w as u32, h as u32, buf.to_vec())
                .ok_or("image buffer")?;
            let mut png = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
            out.push(super::PdfPage { png, boxes });
        }
        Ok(out)
    }

    fn page_box_rect(page: &objc2_core_foundation::CFRetained<objc2_core_graphics::CGPDFPage>) -> objc2_core_foundation::CGRect {
        objc2_core_graphics::CGPDFPage::box_rect(Some(&**page), objc2_core_graphics::CGPDFBox::MediaBox)
    }

    fn recognize_cgimage_text(cg: &objc2_core_graphics::CGImage) -> Result<String, String> {
        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                cg,
                &empty_options(),
            )
        };
        let boxes = boxes_from_handler(&handler, VNRequestTextRecognitionLevel::Accurate)?;
        Ok(boxes
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
