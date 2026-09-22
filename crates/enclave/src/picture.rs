//! Making a generated picture small enough to travel.
//!
//! A model hands back a PNG of several megabytes. Every byte of it then has to cross the
//! mixnet in 64 KB pieces, and megabytes do not cross it well: the pieces pile up, their
//! acknowledgements come late, the sender repeats itself, and the answer never arrives.
//! So the picture is packed again here, inside the enclave, before it is sent.
//!
//! WebP, two ways:
//!
//! - **as delivered** (the default): quality 90, which takes a 3 MB picture to a few
//!   hundred kilobytes. The pixels change slightly, the way any photograph changes when it
//!   is saved. Google's invisible SynthID mark is made to survive ordinary compression, so
//!   a picture stays recognisable as machine-made;
//! - **unchanged**: lossless, every pixel as the model made it, for whoever wants exactly
//!   that. It is perhaps half the size of the PNG, so it is the slow way.
//!
//! Which one is the person's own choice (`lossless` on the request), and the answer says
//! what was done, so nobody is told they have the original when they do not.

use serde_json::{json, Value};

/// Quality of the packing when the picture may change: high enough that the difference is
/// not visible on a screen, low enough to be worth doing.
const QUALITY: f32 = 90.0;

/// What came back from packing one picture.
pub struct Packed {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub was: usize,
}

/// Pack one picture. A picture that cannot be read is handed back untouched: a delivery
/// decision must never lose someone's answer.
pub fn pack(bytes: &[u8], lossless: bool) -> Option<Packed> {
    let was = bytes.len();
    let picture = image::load_from_memory(bytes).ok()?;
    let encoder = webp::Encoder::from_image(&picture).ok()?;
    let out = if lossless { encoder.encode_lossless() } else { encoder.encode(QUALITY) };
    let out = out.to_vec();
    // Lossless WebP is not always smaller than the PNG it came from.
    (out.len() < was).then_some(Packed { mime: "image/webp".into(), bytes: out, was })
}

/// The pictures of an answer, packed. `[{mimeType, data}]` in, the same out — base64 as the
/// app already expects it, so nothing downstream changes.
pub fn pack_all(images: Value, lossless: bool) -> Value {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let Some(list) = images.as_array() else { return images };
    let packed: Vec<Value> = list
        .iter()
        .map(|image| {
            let Some(data) = image.get("data").and_then(|d| d.as_str()) else { return image.clone() };
            let Some(raw) = B64.decode(data).ok() else { return image.clone() };
            match pack(&raw, lossless) {
                Some(p) => json!({ "mimeType": p.mime, "data": B64.encode(&p.bytes), "packed": { "from": p.was, "to": p.bytes.len(), "lossless": lossless } }),
                None => image.clone(),
            }
        })
        .collect();
    json!(packed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture the size of a real one: packed it must be far smaller, and lossless it
    /// must still be a picture of the same size.
    #[test]
    fn a_picture_travels_smaller_and_says_so() {
        let mut raw = image::RgbImage::new(256, 256);
        for (x, y, p) in raw.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x * y) % 256) as u8]);
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(raw).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();

        let small = pack(&png, false).expect("packed");
        assert_eq!(small.mime, "image/webp");
        assert!(small.bytes.len() < png.len() / 2, "{} is not much smaller than {}", small.bytes.len(), png.len());
        let exact = pack(&png, true).expect("packed losslessly");
        let back = webp::Decoder::new(&exact.bytes).decode().expect("a picture again");
        assert_eq!((back.width(), back.height()), (256, 256));
        // Lossless means lossless: every pixel as it was.
        assert_eq!(back.to_image().to_rgb8().into_raw(), image::load_from_memory(&png).unwrap().to_rgb8().into_raw());

        // An answer's pictures keep their shape, and say what was done to them.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let answer = json!([{ "mimeType": "image/png", "data": B64.encode(&png) }]);
        let packed = pack_all(answer, false);
        assert_eq!(packed[0]["mimeType"], "image/webp");
        assert_eq!(packed[0]["packed"]["lossless"], false);
        assert!(packed[0]["data"].as_str().unwrap().len() < B64.encode(&png).len());

        // Something that is not a picture is handed on untouched.
        let odd = json!([{ "mimeType": "image/png", "data": B64.encode(b"not a picture") }]);
        assert_eq!(pack_all(odd.clone(), false), odd);
    }
}
