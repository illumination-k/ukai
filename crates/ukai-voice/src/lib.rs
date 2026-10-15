//! Voice input: record the microphone ([`Recorder`]) and turn the recording
//! into text on this machine with whisper.cpp ([`Transcriber`]). Nothing
//! leaves the machine.
//!
//! [`audio`] holds the pure conversions between the two and how the text
//! joins the draft.

pub mod audio;
mod capture;
mod transcribe;

pub use capture::{CaptureError, Recorder, Recording};
pub use transcribe::{TranscribeError, Transcriber};
