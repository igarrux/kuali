//! Standalone native smoke test / benchmark, independent of UI and Whisper.
//! cargo run -p kuali-stt --example diarize_file -- audio.wav model-directory [speakers]

use std::{path::PathBuf, sync::atomic::AtomicBool, time::Instant};

use kuali_stt::{
    audio_file::decode_audio_file,
    diarization::{ensure_models, DiarizationOptions, Diarizer},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let audio = PathBuf::from(
        args.next()
            .ok_or("usage: diarize_file audio model-directory [speakers]")?,
    );
    let models = PathBuf::from(args.next().ok_or("missing model directory")?);
    let num_speakers = args
        .next()
        .map(|n| n.to_string_lossy().parse::<u32>())
        .transpose()?;
    let cancel = AtomicBool::new(false);
    let samples = decode_audio_file(&audio, &cancel, |_, _| {})?;
    eprintln!("Preparing local models in {}", models.display());
    ensure_models(&models, &cancel, |_, _| {}).await?;
    let start = Instant::now();
    let diarizer = Diarizer::load(&models, DiarizationOptions { num_speakers })?;
    let loaded = start.elapsed();
    let turns = diarizer.diarize(&samples, 0, &cancel, |_, _| {})?;
    let inference = start.elapsed() - loaded;
    let duration = samples.len() as f64 / 16_000.0;
    let count = turns
        .iter()
        .map(|turn| turn.speaker_id)
        .collect::<std::collections::HashSet<_>>()
        .len();
    eprintln!(
        "Audio: {duration:.2}s; load: {:.2}s; inference: {:.2}s; RTF: {:.3}; speakers: {count}",
        loaded.as_secs_f64(),
        inference.as_secs_f64(),
        inference.as_secs_f64() / duration
    );
    for turn in turns {
        println!(
            "SPEAKER recording 1 {:.3} {:.3} <NA> <NA> speaker_{} <NA> <NA>",
            turn.start_ms as f64 / 1_000.0,
            (turn.end_ms - turn.start_ms) as f64 / 1_000.0,
            turn.speaker_id
        );
    }
    Ok(())
}
