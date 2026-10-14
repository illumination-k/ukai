//! Check a whisper model without the app: transcribe a WAV file, or five
//! seconds of the microphone when no file is given.
//!
//! ```sh
//! cargo run -p ukai-voice --example transcribe -- ggml-base.bin [speech.wav] [language]
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::Path;
use std::time::Duration;

use ukai_voice::audio::{self, WHISPER_RATE};
use ukai_voice::{Recorder, Recording, Transcriber};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(model) = args.first() else {
        eprintln!("usage: transcribe <model> [file.wav] [language]");
        std::process::exit(2);
    };
    if let Err(error) = run(Path::new(model), args.get(1), args.get(2).cloned()) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(
    model: &Path,
    wav: Option<&String>,
    language: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let recording = match wav {
        Some(path) => read_wav(Path::new(path))?,
        None => {
            let recorder = Recorder::start()?;
            eprintln!("recording for 5 seconds…");
            std::thread::sleep(Duration::from_secs(5));
            recorder.finish()
        }
    };
    let transcriber = Transcriber::load(model, language)?;
    let samples = recording.for_whisper();
    #[allow(clippy::cast_precision_loss)]
    let seconds = samples.len() as f32 / WHISPER_RATE as f32;
    eprintln!("transcribing {seconds:.1}s…");
    println!("{}", transcriber.transcribe(&samples)?);
    Ok(())
}

fn read_wav(path: &Path) -> Result<Recording, Box<dyn std::error::Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            #[allow(clippy::cast_precision_loss)]
            let scale = (1_i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    Ok(Recording {
        samples: audio::downmix(&interleaved, usize::from(spec.channels)),
        rate: spec.sample_rate,
    })
}
