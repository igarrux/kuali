//! Sends audio through the same Kuali engine path used by the application.
//!
//! Usage: probe <model file> <language> <comma,separated,vocabulary|-> <audio>...
//!
//! This is a diagnostic tool for comparing engine output on real audio before
//! and after parameter changes. The model file name selects the engine, Silero
//! is read from the model's directory, and audio may be any supported format.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kuali_core::{WhisperConfig, WhisperModel};
use kuali_stt::audio_file::decode_audio_file;
use kuali_stt::{samples_to_ms, SpeechEngine};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = Path::new(&args[1]);
    let file_name = model_path.file_name().and_then(|name| name.to_str());
    let model = WhisperModel::ALL
        .into_iter()
        .find(|model| Some(model.file_name()) == file_name)
        .expect("the model file must keep its Kuali file name");

    let mut config = WhisperConfig {
        model,
        models_directory: model_path.parent().map(Path::to_path_buf),
        language: args[2].clone(),
        custom_vocabulary: match args[3].as_str() {
            "-" => Vec::new(),
            terms => terms.split(',').map(|t| t.trim().to_string()).collect(),
        },
        gpu: true,
        threads: 6,
        ..WhisperConfig::default()
    };
    config.lock_model_language();

    let started = Instant::now();
    let mut engine = SpeechEngine::load(model_path, &config).unwrap();
    println!(
        "{} loaded in {:?}",
        model.technical_name(),
        started.elapsed()
    );
    let cancel = AtomicBool::new(false);
    for path in &args[4..] {
        let samples = decode_audio_file(Path::new(path), &cancel, |_, _| {}).unwrap();
        let started = Instant::now();
        let out = engine.transcribe(&samples).unwrap();
        let audio_ms = samples_to_ms(samples.len());
        println!(
            "[{}] {path} ({:.1} s audio, {:.2} s)  {:?}",
            config.language,
            audio_ms as f64 / 1_000.0,
            started.elapsed().as_secs_f64(),
            out.text
        );
    }
}
