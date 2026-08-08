//! Sends audio through the same Kuali engine path used by the application.
//!
//! Usage: probe <model.bin> <language> <comma,separated,vocabulary|-> <audio.wav>...
//!
//! This is a diagnostic tool for comparing `WhisperEngine` output on real audio
//! before and after parameter changes.

use kuali_core::WhisperConfig;
use kuali_stt::WhisperEngine;

fn read_wav_f32(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("failed to read WAV file");
    // Find the `data` chunk instead of assuming a 44-byte header.
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            return bytes[i + 8..(i + 8 + size).min(bytes.len())]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
        }
        i += 8 + size + (size & 1);
    }
    panic!("WAV file has no data chunk");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let config = WhisperConfig {
        language: args[2].clone(),
        custom_vocabulary: match args[3].as_str() {
            "-" => Vec::new(),
            terms => terms.split(',').map(|t| t.trim().to_string()).collect(),
        },
        gpu: true,
        threads: 6,
        ..WhisperConfig::default()
    };

    let mut engine = WhisperEngine::load(std::path::Path::new(&args[1]), &config).unwrap();
    for path in &args[4..] {
        let out = engine.transcribe(&read_wav_f32(path)).unwrap();
        println!("[{}] {path}  {:?}", config.language, out.text);
    }
}
