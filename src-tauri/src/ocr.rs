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
}

/// Recognise text in an encoded image (PNG/JPEG bytes) WITH per-line boxes, so
/// the guard can highlight/redact exactly where a match is.
pub fn recognize(image: &[u8]) -> Result<Vec<TextBox>, String> {
    #[cfg(target_os = "macos")]
    {
        macos::recognize_image(image)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = image;
        Err("ocr-unsupported-platform".to_string())
    }
}

/// Recognise text in a scanned/image-only PDF by rendering its pages and OCR'ing.
pub fn recognize_pdf(pdf: &[u8]) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        macos::recognize_pdf(pdf)
    }
    #[cfg(not(target_os = "macos"))]
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
    #[cfg(target_os = "macos")]
    {
        macos::recognize_pdf_pages(pdf)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pdf;
        Err("ocr-unsupported-platform".to_string())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::Retained;
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRecognizedTextObservation,
        VNRequestTextRecognitionLevel,
    };

    fn empty_options() -> Retained<NSDictionary<NSString>> {
        NSDictionary::new()
    }

    /// Run an accurate text-recognition request against a prepared handler and
    /// return each recognised line with its normalized (top-left origin) box.
    fn boxes_from_handler(handler: &VNImageRequestHandler) -> Result<Vec<super::TextBox>, String> {
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
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
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn recognize_image(image: &[u8]) -> Result<Vec<super::TextBox>, String> {
        let data = NSData::with_bytes(image);
        let options = empty_options();
        let handler = VNImageRequestHandler::initWithData_options(
            VNImageRequestHandler::alloc(),
            &data,
            &options,
        );
        boxes_from_handler(&handler)
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
            let boxes = boxes_from_handler(&handler)?;

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
        let boxes = boxes_from_handler(&handler)?;
        Ok(boxes
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
