//! Dictation into a composer: record the microphone, then transcribe on this
//! machine with the whisper model named in the user's `[voice]`
//! (`ukai_voice`). Nothing is sent anywhere.

use std::path::PathBuf;
use std::sync::Arc;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use tokio::sync::OnceCell;
use ukai_voice::{Recorder, Transcriber};

use crate::projects::Paths;

/// The user's voice settings and, once first used, the loaded model.
#[derive(Clone)]
pub struct Voice {
    /// The model and language, or why voice input is unavailable.
    setup: Result<(PathBuf, Option<String>), String>,
    model: Arc<OnceCell<Result<Transcriber, String>>>,
}

impl Voice {
    pub fn load(paths: &Paths) -> Self {
        let config_dir = paths.user_config.as_deref().and_then(|file| file.parent());
        let setup = match ukai_config::load_user(paths.user_config.as_deref()) {
            Ok(config) => config
                .voice
                .map(|voice| (voice.model_path(config_dir), voice.language))
                .ok_or_else(|| {
                    "Voice input needs a whisper model: set `[voice] model = \"…/ggml-base.bin\"` \
                     in config.toml"
                        .to_owned()
                }),
            Err(error) => Err(error.to_string()),
        };
        Self {
            setup,
            model: Arc::new(OnceCell::new()),
        }
    }

    /// The model, loaded on first use off the UI thread.
    async fn transcriber(&self) -> Result<Transcriber, String> {
        let (path, language) = self.setup.clone()?;
        self.model
            .get_or_init(|| async move {
                tokio::task::spawn_blocking(move || {
                    Transcriber::load(&path, language).map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            })
            .await
            .clone()
    }
}

/// Where a composer's dictation is.
#[derive(Default)]
pub enum Dictation {
    #[default]
    Idle,
    Recording(Recorder),
    Transcribing,
}

/// Drops a composer's recording when the last session holding it goes, so a
/// closed pane does not keep the microphone open.
pub struct DictationOwner(pub Signal<Dictation>);

impl Drop for DictationOwner {
    fn drop(&mut self) {
        if let Ok(mut dictation) = self.0.try_write() {
            *dictation = Dictation::Idle;
        }
    }
}

/// Start recording into `draft`, or stop and transcribe what was recorded.
/// Problems go to `error`.
pub fn toggle(
    voice: &Voice,
    mut dictation: Signal<Dictation>,
    mut draft: Signal<String>,
    mut error: Signal<Option<String>>,
) {
    let current = std::mem::take(&mut *dictation.write());
    match current {
        Dictation::Idle => {
            if let Err(message) = &voice.setup {
                error.set(Some(message.clone()));
                return;
            }
            match Recorder::start() {
                Ok(recorder) => {
                    dictation.set(Dictation::Recording(recorder));
                    error.set(None);
                    // Load the model while the user speaks.
                    let voice = voice.clone();
                    spawn_forever(async move {
                        if let Err(message) = voice.transcriber().await {
                            error.set(Some(message));
                        }
                    });
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        }
        Dictation::Recording(recorder) => {
            dictation.set(Dictation::Transcribing);
            let recording = recorder.finish();
            let voice = voice.clone();
            // Not tied to the pane's component: switching tabs remounts it,
            // and the transcript still belongs in the draft.
            spawn_forever(async move {
                let heard = match voice.transcriber().await {
                    Ok(transcriber) => tokio::task::spawn_blocking(move || {
                        transcriber
                            .transcribe(&recording.for_whisper())
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
                    Err(message) => Err(message),
                };
                match heard {
                    Ok(text) if text.is_empty() => error.set(Some("No speech heard".to_owned())),
                    Ok(text) => {
                        let updated = ukai_voice::audio::insert(&draft.peek(), &text);
                        draft.set(updated);
                        error.set(None);
                    }
                    Err(message) => error.set(Some(message)),
                }
                dictation.set(Dictation::Idle);
            });
        }
        // Wait for the transcript; the next press starts a new recording.
        Dictation::Transcribing => dictation.set(Dictation::Transcribing),
    }
}
