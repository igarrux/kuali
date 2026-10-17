//! Opt-in native regression using an upstream public recording; never downloads
//! models or audio. Set KUALI_DIARIZATION_MODELS_DIR and
//! KUALI_DIARIZATION_TEST_AUDIO, then run:
//! cargo test -p kuali-stt --test diarization_reference -- --ignored

use std::{collections::HashMap, path::PathBuf, sync::atomic::AtomicBool};

use kuali_stt::{
    audio_file::decode_audio_file,
    diarization::{DiarizationOptions, Diarizer},
};

#[test]
#[ignore = "requires local diarization models and the public four-speaker fixture"]
fn four_speaker_reference_preserves_every_turn_identity() {
    // Official fixture and published reference output:
    // https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/0-four-speakers-zh.wav
    // https://k2-fsa.github.io/sherpa/onnx/speaker-diarization/models.html#d-speaker-model-onnx
    // This compares the published speaker assignments, not a hand-labeled DER
    // benchmark. A correct count alone misses the old preprocessing regression:
    // it returned four labels but reassigned multiple turns to the wrong voice.
    const REFERENCE: [(u64, u64, u32); 10] = [
        (318, 6_865, 0),
        (7_017, 10_747, 1),
        (11_455, 13_632, 1),
        (13_750, 17_041, 2),
        (22_137, 24_837, 0),
        (27_638, 29_478, 3),
        (30_001, 31_553, 3),
        (33_680, 37_932, 3),
        (48_040, 50_470, 2),
        (52_529, 54_605, 0),
    ];
    // At most one segmentation frame of boundary variation across runtimes.
    const MAX_BOUNDARY_ERROR_MS: u64 = 35;

    let models = PathBuf::from(
        std::env::var_os("KUALI_DIARIZATION_MODELS_DIR")
            .expect("set KUALI_DIARIZATION_MODELS_DIR to the local verified model cache"),
    );
    let audio =
        PathBuf::from(std::env::var_os("KUALI_DIARIZATION_TEST_AUDIO").expect(
            "set KUALI_DIARIZATION_TEST_AUDIO to the public 0-four-speakers-zh.wav fixture",
        ));
    let cancel = AtomicBool::new(false);
    let samples = decode_audio_file(&audio, &cancel, |_, _| {})
        .expect("decode the local public four-speaker fixture");
    for num_speakers in [Some(4), None] {
        let mode = if num_speakers.is_some() {
            "known"
        } else {
            "auto"
        };
        let diarizer = Diarizer::load(&models, DiarizationOptions { num_speakers })
            .expect("load verified local diarization models without downloading");
        let turns = diarizer
            .diarize(&samples, 0, &cancel, |_, _| {})
            .expect("diarize the public four-speaker fixture");
        assert_eq!(turns.len(), REFERENCE.len(), "{mode}: reference turn count");

        // Canonicalize first appearance so native cluster numbering is
        // irrelevant, while each voice keeps its identity across later turns.
        let mut identities = HashMap::new();
        for (index, (actual, &(start_ms, end_ms, speaker_id))) in
            turns.iter().zip(&REFERENCE).enumerate()
        {
            let next_id = identities.len() as u32;
            let canonical_id = *identities.entry(actual.speaker_id).or_insert(next_id);
            assert_eq!(
                canonical_id, speaker_id,
                "{mode}: speaker identity in turn {index}"
            );
            assert!(
                actual.start_ms.abs_diff(start_ms) <= MAX_BOUNDARY_ERROR_MS,
                "{mode}: turn {index} starts at {} ms, expected {start_ms} ms",
                actual.start_ms
            );
            assert!(
                actual.end_ms.abs_diff(end_ms) <= MAX_BOUNDARY_ERROR_MS,
                "{mode}: turn {index} ends at {} ms, expected {end_ms} ms",
                actual.end_ms
            );
        }
        assert_eq!(
            identities.len(),
            4,
            "{mode}: all four reference voices survive"
        );
    }
}
