// ---------------------------------------------------------------------------
// detect.rs — on-device SEMANTIC entity detection for the privacy guard.
//
// THE SEAM: `detect(model, tokenizer, texts, labels, threshold)` runs a GLiNER
// model (ONNX) via `gline-rs` (ort + tokenizers) to find OPEN-CLASS sensitive
// entities — person names, postal addresses, tax numbers — that the deterministic
// detectors in guard.js structurally cannot (those have no fixed format; they
// need meaning-in-context). It is ZERO-SHOT: the caller passes the labels to look
// for, so the same model finds "person", "address", "tax number", … in any of the
// model's languages.
//
// CROSS-PLATFORM: gline-rs → ort → ONNX Runtime runs on macOS, Linux, Windows,
// iOS and Android — ONE code path for the shared Rust core. (This is why GLiNER
// fits where Apple's NaturalLanguage — macOS/iOS only, and weak on names — did
// not.)
//
// BUILD GATE: compiled only under the `smart-guard` Cargo feature, because it
// pulls in ort / ONNX Runtime (a heavy native dependency). Without the feature,
// every fn returns a clear "not built" error and the guard simply relies on its
// deterministic layer (guard.js) — nothing breaks. Flip the feature on to build
// and activate the semantic layer.
//
// PRIVACY: like the rest of the guard, this is 100% on-device — the text and the
// findings never leave the machine. See the privacy-guard notes.
// ---------------------------------------------------------------------------

use std::path::Path;

/// One detected entity. `seq` is the index of the input text it was found in, so
/// an image/PDF box maps straight back to itself for redaction — no character
/// offsets required (we flag the whole box that carried a match).
#[derive(serde::Serialize, Clone)]
pub struct Entity {
    pub label: String,
    pub text: String,
    pub score: f32,
    pub seq: usize,
}

/// True if a usable model + tokenizer are present on disk at these paths.
pub fn available(model: &Path, tokenizer: &Path) -> bool {
    engine::available(model, tokenizer)
}

/// Run zero-shot NER over a BATCH of texts (e.g. every OCR box on a page) in one
/// inference, returning the entities found in each — aligned to `texts` by index.
pub fn detect(
    model: &Path,
    tokenizer: &Path,
    texts: &[String],
    labels: &[String],
    threshold: f32,
) -> Result<Vec<Vec<Entity>>, String> {
    engine::detect(model, tokenizer, texts, labels, threshold)
}

// --- real engine: only when the `smart-guard` feature is enabled --------------
#[cfg(feature = "smart-guard")]
mod engine {
    use super::Entity;
    use gliner::model::pipeline::span::SpanMode;
    use gliner::model::{input::text::TextInput, params::Parameters, GLiNER};
    use orp::params::RuntimeParameters;
    use std::path::Path;
    use std::sync::Mutex;

    // The loaded model is cached for the process, keyed by its path (so a model
    // (re)installed at a new location reloads). `GLiNER::inference` takes `&self`;
    // the Mutex makes it safely shareable across Tauri's worker threads and
    // serialises concurrent scans (fine — a scan is a one-shot pre-send check).
    struct Loaded {
        key: String,
        model: GLiNER<SpanMode>,
    }
    static MODEL: Mutex<Option<Loaded>> = Mutex::new(None);

    pub fn available(model: &Path, tokenizer: &Path) -> bool {
        model.is_file() && tokenizer.is_file()
    }

    pub fn detect(
        model: &Path,
        tokenizer: &Path,
        texts: &[String],
        labels: &[String],
        threshold: f32,
    ) -> Result<Vec<Vec<Entity>>, String> {
        if texts.is_empty() || labels.is_empty() {
            return Ok(texts.iter().map(|_| Vec::new()).collect());
        }
        let mut guard = MODEL.lock().map_err(|_| "model lock poisoned".to_string())?;

        // (Re)load the model if the path changed or nothing is loaded yet.
        let key = model.to_string_lossy().to_string();
        let reload = guard.as_ref().map(|l| l.key != key).unwrap_or(true);
        if reload {
            let params = Parameters::default().with_threshold(threshold);
            let m = GLiNER::<SpanMode>::new(
                params,
                RuntimeParameters::default(),
                tokenizer.to_string_lossy().as_ref(),
                model.to_string_lossy().as_ref(),
            )
            .map_err(|e| format!("gliner load failed: {e}"))?;
            *guard = Some(Loaded { key, model: m });
        }
        let loaded = guard.as_ref().ok_or("model not loaded")?;

        // One batched inference over every text (e.g. all boxes on a page).
        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let input = TextInput::from_str(&text_refs, &label_refs)
            .map_err(|e| format!("gliner input failed: {e}"))?;
        let output = loaded
            .model
            .inference(input)
            .map_err(|e| format!("gliner inference failed: {e}"))?;

        // `output.spans` is indexed by input-sequence: spans[i] = entities in texts[i].
        let mut out: Vec<Vec<Entity>> = texts.iter().map(|_| Vec::new()).collect();
        for (seq, spans) in output.spans.iter().enumerate() {
            if seq >= out.len() {
                break;
            }
            for span in spans.iter() {
                out[seq].push(Entity {
                    label: span.class().to_string(),
                    text: span.text().to_string(),
                    score: span.probability(),
                    seq,
                });
            }
        }
        Ok(out)
    }
}

// --- fallback: feature off → the guard uses its deterministic layer only -------
#[cfg(not(feature = "smart-guard"))]
mod engine {
    use super::Entity;
    use std::path::Path;

    pub fn available(_model: &Path, _tokenizer: &Path) -> bool {
        false
    }

    pub fn detect(
        _model: &Path,
        _tokenizer: &Path,
        _texts: &[String],
        _labels: &[String],
        _threshold: f32,
    ) -> Result<Vec<Vec<Entity>>, String> {
        Err("smart-guard feature not built".to_string())
    }
}
