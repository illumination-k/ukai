//! Speech to text with whisper.cpp, on this machine.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::audio;

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("whisper model {}: {message}", path.display())]
    Model { path: PathBuf, message: String },
    #[error("transcription failed: {0}")]
    Whisper(String),
}

/// A loaded whisper model. Cheap to clone; the model is shared.
#[derive(Clone)]
pub struct Transcriber {
    context: Arc<WhisperContext>,
    /// Spoken language as an ISO 639-1 code, or `None` to detect it.
    language: Option<String>,
}

impl std::fmt::Debug for Transcriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcriber")
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl Transcriber {
    /// Load the ggml model at `path`. Takes a moment for the larger models,
    /// so call it off the UI thread.
    pub fn load(path: &Path, language: Option<String>) -> Result<Self, TranscribeError> {
        static LOGGING: Once = Once::new();
        // whisper.cpp prints to stdout and stderr; route it through tracing.
        LOGGING.call_once(whisper_rs::install_logging_hooks);
        if !path.is_file() {
            return Err(TranscribeError::Model {
                path: path.to_owned(),
                message: "not found".to_owned(),
            });
        }
        let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|e| TranscribeError::Model {
                path: path.to_owned(),
                message: e.to_string(),
            })?;
        Ok(Self {
            context: Arc::new(context),
            language,
        })
    }

    /// The text spoken in `samples` (16 kHz mono), empty if nothing was
    /// said. Blocks for about as long as the model needs; run it off the UI
    /// thread.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String, TranscribeError> {
        if !audio::has_speech(samples) {
            return Ok(String::new());
        }
        let mut state = self
            .context
            .create_state()
            .map_err(|e| TranscribeError::Whisper(e.to_string()))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(self.language.as_deref().unwrap_or("auto")));
        params.set_translate(false);
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        params.set_n_threads(i32::try_from(threads).unwrap_or(4));
        state
            .full(params, samples)
            .map_err(|e| TranscribeError::Whisper(e.to_string()))?;
        let segments: Vec<String> = state
            .as_iter()
            .map(|segment| segment.to_str_lossy().map(|s| s.into_owned()))
            .collect::<Result<_, _>>()
            .map_err(|e| TranscribeError::Whisper(e.to_string()))?;
        Ok(audio::join_segments(segments.iter().map(String::as_str)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_is_reported_with_its_path() {
        let error = Transcriber::load(Path::new("/nonexistent/ggml-base.bin"), None)
            .err()
            .map(|e| e.to_string());
        assert_eq!(
            error.as_deref(),
            Some("whisper model /nonexistent/ggml-base.bin: not found")
        );
    }
}
