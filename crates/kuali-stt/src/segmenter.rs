//! Determines where each speech turn begins and ends.
//!
//! Discord provides one audio stream per participant, so diarization is
//! unnecessary. The remaining problem is choosing cuts because Whisper
//! transcribes finite segments rather than endless streams.
//!
//! A cut occurs when a speaker becomes silent or has spoken continuously for too long.

use std::collections::HashMap;

use kuali_core::{DiscordUserId, RecordingConfig};

use crate::audio::{ms_to_samples, rms, samples_to_ms};

/// Inexpensive gate that avoids buffering digital silence. It measures only
/// energy for segmentation and does not decide whether human speech exists;
/// Silero makes that decision immediately before Whisper.
const SILENCE_RMS: f32 = 0.0015;

/// Per-speaker memory ceiling. Five minutes of 16 kHz mono audio is roughly
/// 19 MB; reaching this point indicates a problem, but a cut is safer than
/// unbounded growth.
const MAX_BUFFERED_MS: u64 = 5 * 60 * 1_000;

/// Draft refresh cadence while a speaker remains active. The entire open turn
/// is transcribed so the UI can replace the draft instead of stitching
/// speculative text itself.
const LIVE_PREVIEW_INTERVAL_MS: u64 = 3_000;

/// A duration cut may fall beside a syllable. Carrying a short tail into the
/// next block lets Whisper hear the boundary again; repeated text is removed
/// later with same-speaker context.
const FORCED_CUT_OVERLAP_MS: u64 = 600;

/// Discord stops sending packets during some pauses. Restoring part of that
/// silence preserves prosody and punctuation without filling buffers with zeros.
const MAX_PRESERVED_GAP_MS: u64 = 1_000;

/// Initial capacity rather than a limit. The map can grow further, while the
/// reservation avoids reallocations when many people speak at once.
const EXPECTED_CONCURRENT_SPEAKERS: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Stable ID within the meeting. A turn's draft and final version share it.
    pub id: u64,
    pub speaker_id: DiscordUserId,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Mono 16 kHz audio ready for Whisper.
    pub samples: Vec<f32>,
    /// Whether the beginning repeats audio from the previous segment around a cut.
    pub overlap_with_previous: bool,
}

impl Segment {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

#[derive(Debug, Default)]
pub struct PushResult {
    pub preview: Option<Segment>,
    pub final_segment: Option<Segment>,
}

struct Buffer {
    id: u64,
    samples: Vec<f32>,
    start_ms: u64,
    last_voice_ms: u64,
    last_audio_end_ms: u64,
    next_preview_ms: u64,
    overlap_with_previous: bool,
}

impl Buffer {
    /// Whether this speech turn has ended.
    ///
    /// Brief silence closes a turn only after enough audio has accumulated.
    /// Below the target, collecting more context is safer than sending
    /// half-second fragments that encourage Whisper hallucinations.
    ///
    /// Waiting is still bounded. A short complete reply followed by silence must
    /// be emitted rather than merged with speech half a minute later.
    fn is_done(&self, now_ms: u64, config: &RecordingConfig) -> bool {
        let quiet_ms = now_ms.saturating_sub(self.last_voice_ms);
        if samples_to_ms(self.samples.len()) >= config.target_segment_ms {
            quiet_ms >= config.silence_ms
        } else {
            quiet_ms >= config.end_of_turn_ms
        }
    }
}

pub struct Segmenter {
    config: RecordingConfig,
    buffers: HashMap<DiscordUserId, Buffer>,
    next_segment_id: u64,
}

impl Segmenter {
    pub fn new(config: RecordingConfig) -> Self {
        Self {
            config,
            buffers: HashMap::with_capacity(EXPECTED_CONCURRENT_SPEAKERS),
            next_segment_id: 1,
        }
    }

    /// Adds speaker audio and returns a segment after reaching the duration cap.
    pub fn push(
        &mut self,
        speaker_id: DiscordUserId,
        now_ms: u64,
        samples: &[f32],
    ) -> Option<Segment> {
        self.push_continuous(speaker_id, now_ms, samples)
            .final_segment
    }

    /// Live-engine variant that may return a draft in addition to a final cut.
    pub fn push_continuous(
        &mut self,
        speaker_id: DiscordUserId,
        now_ms: u64,
        samples: &[f32],
    ) -> PushResult {
        if samples.is_empty() {
            return PushResult::default();
        }

        let config = self.config;
        let chunk_start_ms = now_ms.saturating_sub(samples_to_ms(samples.len()));
        let mut result = PushResult::default();

        // If the loop remained busy until a new turn started before `tick`, close
        // the previous turn here.
        let previous_is_done = self
            .buffers
            .get(&speaker_id)
            .is_some_and(|buffer| buffer.is_done(chunk_start_ms, &config));
        if previous_is_done {
            result.final_segment = self.close(speaker_id);
        }

        if !self.buffers.contains_key(&speaker_id) {
            let id = self.next_segment_id;
            self.next_segment_id += 1;
            self.buffers.insert(
                speaker_id,
                Buffer {
                    id,
                    samples: Vec::new(),
                    // Newly arrived audio began recording before the current instant.
                    start_ms: chunk_start_ms,
                    last_voice_ms: now_ms,
                    last_audio_end_ms: chunk_start_ms,
                    next_preview_ms: config
                        .target_segment_ms
                        .max(config.min_segment_ms)
                        .max(1_000),
                    overlap_with_previous: false,
                },
            );
        }

        let next_tail_id = self.next_segment_id;
        let buffer = self.buffers.get_mut(&speaker_id).expect("just inserted");

        let gap_ms = chunk_start_ms.saturating_sub(buffer.last_audio_end_ms);
        if gap_ms > 0 {
            let preserved_ms = gap_ms.min(MAX_PRESERVED_GAP_MS);
            buffer
                .samples
                .resize(buffer.samples.len() + ms_to_samples(preserved_ms), 0.0);
        }

        buffer.samples.extend_from_slice(samples);
        buffer.last_voice_ms = now_ms;
        buffer.last_audio_end_ms = now_ms;

        let buffered_ms = samples_to_ms(buffer.samples.len());
        if buffered_ms >= config.max_segment_ms.min(MAX_BUFFERED_MS) {
            result.final_segment = Self::cut(speaker_id, buffer, &config, next_tail_id);
            self.next_segment_id += 1;
            return result;
        }

        if buffered_ms >= buffer.next_preview_ms {
            result.preview = snapshot(speaker_id, buffer, &config);
            buffer.next_preview_ms = buffered_ms.saturating_add(LIVE_PREVIEW_INTERVAL_MS);
        }
        result
    }

    /// Closes turns silent beyond the configured threshold. Called on every
    /// voice-receiver tick, roughly 50 times per second.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Segment> {
        let config = self.config;
        let stale: Vec<DiscordUserId> = self
            .buffers
            .iter()
            .filter(|(_, b)| !b.samples.is_empty() && b.is_done(now_ms, &config))
            .map(|(id, _)| *id)
            .collect();

        stale.into_iter().filter_map(|id| self.close(id)).collect()
    }

    /// Closes the turn of a participant who just left the channel.
    pub fn close(&mut self, speaker_id: DiscordUserId) -> Option<Segment> {
        let buffer = self.buffers.remove(&speaker_id)?;
        // A turn ends at the last audible speech, not merely at the end of stored
        // samples. Turns may absorb pauses, and ignoring them shortens timestamps.
        finish(
            buffer.id,
            speaker_id,
            buffer.start_ms,
            buffer.last_voice_ms,
            buffer.samples,
            buffer.overlap_with_previous,
            &self.config,
        )
    }

    /// Closes every turn; buffered audio still belongs to the meeting at hangup.
    pub fn flush(&mut self) -> Vec<Segment> {
        let ids: Vec<_> = self.buffers.keys().copied().collect();
        let mut segments: Vec<_> = ids.into_iter().filter_map(|id| self.close(id)).collect();
        segments.sort_by_key(|s| s.start_ms);
        segments
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.values().all(|b| b.samples.is_empty())
    }

    /// Forced duration cut that finds the quietest trailing point to avoid
    /// splitting a word. Whisper is unreliable when boundaries bisect syllables.
    fn cut(
        speaker_id: DiscordUserId,
        buffer: &mut Buffer,
        config: &RecordingConfig,
        next_segment_id: u64,
    ) -> Option<Segment> {
        // `push` normally receives 20 ms packets, but a larger block must still
        // never move the head beyond the configured maximum.
        let limit = buffer
            .samples
            .len()
            .min(ms_to_samples(config.max_segment_ms.min(MAX_BUFFERED_MS)));
        let split = quietest_split_point(&buffer.samples[..limit]);
        let overlap_start = split.saturating_sub(ms_to_samples(FORCED_CUT_OVERLAP_MS));
        let head = buffer.samples[..split].to_vec();
        let tail = buffer.samples[overlap_start..].to_vec();
        let head_start = buffer.start_ms;
        let head_id = buffer.id;
        let head_overlaps = buffer.overlap_with_previous;

        // This cut falls inside an active turn, so its timestamp comes directly
        // from audio duration while the participant keeps speaking.
        let head_end = head_start + samples_to_ms(head.len());
        buffer.id = next_segment_id;
        buffer.samples = tail;
        buffer.start_ms = head_start + samples_to_ms(overlap_start);
        buffer.next_preview_ms = config
            .target_segment_ms
            .max(config.min_segment_ms)
            .max(1_000);
        buffer.overlap_with_previous = true;
        finish(
            head_id,
            speaker_id,
            head_start,
            head_end,
            head,
            head_overlaps,
            config,
        )
    }
}

fn snapshot(
    speaker_id: DiscordUserId,
    buffer: &Buffer,
    config: &RecordingConfig,
) -> Option<Segment> {
    finish(
        buffer.id,
        speaker_id,
        buffer.start_ms,
        buffer.last_voice_ms,
        buffer.samples.clone(),
        buffer.overlap_with_previous,
        config,
    )
}

fn finish(
    id: u64,
    speaker_id: DiscordUserId,
    start_ms: u64,
    end_ms: u64,
    samples: Vec<f32>,
    overlap_with_previous: bool,
    config: &RecordingConfig,
) -> Option<Segment> {
    let duration = samples_to_ms(samples.len());
    if duration < config.min_segment_ms {
        return None;
    }
    if rms(&samples) < SILENCE_RMS {
        return None;
    }
    Some(Segment {
        id,
        speaker_id,
        start_ms,
        end_ms: end_ms.max(start_ms + duration),
        samples,
        overlap_with_previous,
    })
}

/// Lowest-energy 10 ms window within the final half-second. Falls back to the end
/// when no search window exists.
fn quietest_split_point(samples: &[f32]) -> usize {
    const WINDOW_MS: u64 = 10;
    const SEARCH_MS: u64 = 500;

    let window = ms_to_samples(WINDOW_MS);
    let search = ms_to_samples(SEARCH_MS);
    if samples.len() <= window + search {
        return samples.len();
    }

    let search_start = samples.len() - search;
    let mut best = samples.len();
    let mut best_energy = f32::MAX;

    for start in (search_start..samples.len() - window).step_by(window) {
        let energy = rms(&samples[start..start + window]);
        if energy < best_energy {
            best_energy = energy;
            best = start + window;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANA: DiscordUserId = 10;
    const LUIS: DiscordUserId = 20;

    fn config() -> RecordingConfig {
        RecordingConfig {
            silence_ms: 700,
            target_segment_ms: 3_000,
            end_of_turn_ms: 2_500,
            max_segment_ms: 5_000,
            min_segment_ms: 350,
        }
    }

    /// Audio with enough energy not to be treated as silence.
    fn voice(ms: u64) -> Vec<f32> {
        (0..ms_to_samples(ms))
            .map(|i| (i as f32 * 0.1).sin() * 0.3)
            .collect()
    }

    fn silence(ms: u64) -> Vec<f32> {
        vec![0.0; ms_to_samples(ms)]
    }

    #[test]
    fn audio_alone_does_not_close_a_segment() {
        let mut s = Segmenter::new(config());
        assert!(s.push(ANA, 1_000, &voice(500)).is_none());
        assert!(!s.is_empty());
    }

    #[test]
    fn silence_closes_a_segment_that_is_already_long_enough() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(3_000));

        // Not enough silence has elapsed yet.
        assert!(s.tick(1_500).is_empty());

        let closed = s.tick(1_800);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].speaker_id, ANA);
        assert!(s.is_empty());
    }

    #[test]
    fn a_pause_does_not_split_a_turn_that_is_still_too_short() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(1_000));

        // After a brief pause with only one second recorded, wait for more
        // context rather than inviting Whisper to invent the rest.
        assert!(s.tick(1_800).is_empty());

        // Continued speech remains part of the same turn.
        s.push(ANA, 2_600, &voice(2_500));
        let closed = s.tick(3_400);
        assert_eq!(closed.len(), 1);
        assert_eq!(
            closed[0].samples.len(),
            ms_to_samples(3_500),
            "both parts should remain in the same segment"
        );
    }

    #[test]
    fn a_turn_that_really_ended_is_sent_even_if_it_is_short() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(1_000));

        // A short complete response followed by silence should close below the
        // target rather than merge with speech much later.
        assert!(s.tick(2_000).is_empty());
        let closed = s.tick(3_600);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].duration_ms(), 1_000);
    }

    #[test]
    fn each_speaker_is_segmented_independently() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(3_000));
        s.push(LUIS, 1_400, &voice(3_000));

        // Ana has been silent since 1_000; Luis only since 1_400.
        let closed = s.tick(1_900);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].speaker_id, ANA);

        let closed = s.tick(2_300);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].speaker_id, LUIS);
    }

    #[test]
    fn sixty_four_simultaneous_speakers_keep_independent_audio() {
        let mut s = Segmenter::new(config());
        let audio = voice(3_000);
        for speaker_id in 1..=64 {
            assert!(s.push(speaker_id, 1_000, &audio).is_none());
        }

        let closed = s.tick(1_800);
        let speakers = closed
            .iter()
            .map(|segment| segment.speaker_id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(closed.len(), 64);
        assert_eq!(speakers.len(), 64);
    }

    #[test]
    fn a_monologue_gets_cut_before_it_grows_forever() {
        let mut s = Segmenter::new(config());
        let mut produced = Vec::new();
        // Twelve continuous seconds in one-second chunks with a five-second cap.
        for i in 0..12 {
            if let Some(seg) = s.push(ANA, 1_000 + i * 1_000, &voice(1_000)) {
                produced.push(seg);
            }
        }
        assert!(
            produced.len() >= 2,
            "the monologue should have been cut at least twice"
        );
        assert!(produced.iter().all(|s| s.duration_ms() <= 5_100));
    }

    #[test]
    fn a_forced_cut_keeps_a_small_overlap_in_the_timeline() {
        let mut s = Segmenter::new(config());

        let mut elapsed = 1_000;
        let mut first = None;
        for _ in 0..6 {
            let seg = s.push(ANA, elapsed, &voice(1_000));
            elapsed += 1_000;
            if let Some(seg) = seg {
                first = Some(seg);
                break;
            }
        }
        let first = first.expect("segment should close at the maximum duration");

        // The quietest-point cut may leave only a few hundred milliseconds,
        // below the minimum on its own. If speech continues, that tail leads the
        // next segment and must preserve its original timing.
        s.push(ANA, elapsed, &voice(1_000));

        let rest = s.flush();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].start_ms, first.end_ms - FORCED_CUT_OVERLAP_MS);
        assert!(rest[0].overlap_with_previous);
    }

    #[test]
    fn a_lone_tail_preserves_the_forced_cut_overlap() {
        let mut s = Segmenter::new(config());
        let mut elapsed = 1_000;
        for _ in 0..6 {
            if s.push(ANA, elapsed, &voice(1_000)).is_some() {
                break;
            }
            elapsed += 1_000;
        }
        let tail = s.flush();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].overlap_with_previous);
        assert!(tail[0].duration_ms() >= FORCED_CUT_OVERLAP_MS);
    }

    #[test]
    fn a_live_preview_and_its_final_turn_share_the_same_id() {
        let mut s = Segmenter::new(config());
        let preview = s.push_continuous(ANA, 3_000, &voice(3_000));
        let preview = preview.preview.expect("a draft should be published");

        let final_segment = s.tick(3_800).pop().expect("speaker turn should close");
        assert_eq!(preview.id, final_segment.id);
        assert_eq!(preview.samples, final_segment.samples);
    }

    #[test]
    fn a_short_pause_is_preserved_as_audio_instead_of_gluing_words_together() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(1_000));
        s.push(ANA, 2_500, &voice(1_000));

        let segment = s.close(ANA).unwrap();
        assert_eq!(samples_to_ms(segment.samples.len()), 2_500);
        let pause = &segment.samples[ms_to_samples(1_000)..ms_to_samples(1_500)];
        assert!(pause.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_cough_is_too_short_to_be_worth_transcribing() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(100)); // por debajo de min_segment_ms
        assert!(s.tick(4_000).is_empty());
    }

    #[test]
    fn buffered_silence_is_dropped_instead_of_hallucinated_over() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &silence(2_000));
        assert!(
            s.tick(4_000).is_empty(),
            "silence should not reach Whisper"
        );
    }

    #[test]
    fn flush_returns_everything_still_open_in_chronological_order() {
        let mut s = Segmenter::new(config());
        s.push(LUIS, 5_000, &voice(1_000));
        s.push(ANA, 1_000, &voice(1_000));

        let segments = s.flush();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].speaker_id, ANA);
        assert_eq!(segments[1].speaker_id, LUIS);
        assert!(s.is_empty());
    }

    #[test]
    fn closing_a_speaker_who_left_yields_their_last_words() {
        let mut s = Segmenter::new(config());
        s.push(ANA, 1_000, &voice(1_000));
        let last = s
            .close(ANA)
            .expect("the final utterance should be returned");
        assert_eq!(last.speaker_id, ANA);
        assert!(
            s.close(ANA).is_none(),
            "closing twice should not duplicate audio"
        );
    }

    #[test]
    fn segment_timing_matches_when_the_audio_actually_happened() {
        let mut s = Segmenter::new(config());
        // At 5 s, one second of audio arrives that occurred between 4 s and 5 s.
        s.push(ANA, 5_000, &voice(1_000));
        let seg = &s.tick(7_600)[0];
        assert_eq!(seg.start_ms, 4_000);
        assert_eq!(seg.end_ms, 5_000);
    }
}
