//! Pyannote window aggregation and diarization post-processing.
//! Adapted from sherpa-onnx v1.13.7: sherpa-onnx/csrc/offline-speaker-diarization-pyannote-impl.h
//! (Copyright 2024 Xiaomi Corporation; Apache-2.0). No inference or clustering.
use std::{fmt, ops::Range};

#[derive(Clone, Copy, Debug)]
pub(super) struct Metadata {
    pub(super) sample_rate: usize,
    pub(super) window_size: usize,
    pub(super) window_shift: usize,
    pub(super) receptive_field_size: usize,
    pub(super) receptive_field_shift: usize,
    pub(super) local_speakers: u8,
}
impl Metadata {
    pub(super) fn validate(self) -> Result<(), Error> {
        if self.sample_rate == 0
            || self.window_size == 0
            || self.window_shift == 0
            || self.receptive_field_shift == 0
            || !(1..=8).contains(&self.local_speakers)
        {
            return Err(Error::Invalid("invalid model metadata"));
        }
        Ok(())
    }
    fn frame_start(self, chunk: usize) -> usize {
        // Match native float32 arithmetic, including rounding before int cast.
        ((chunk as f32 * self.window_shift as f32 / self.receptive_field_shift as f32) as f64 + 0.5)
            as usize
    }
    fn frame_time(self, frame: usize) -> f32 {
        let scale = self.receptive_field_shift as f32 / self.sample_rate as f32;
        let offset = (0.5 * self.receptive_field_size as f64 / self.sample_rate as f64) as f32;
        (frame as f32).mul_add(scale, offset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ChunkMasks {
    pub(super) frames: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EmbeddingInput {
    pub(super) chunk: usize,
    pub(super) local_speaker: u8,
    /// Non-overlapping audio slices concatenated for this embedding, clipped to real audio.
    pub(super) samples: Vec<Range<usize>>,
}
impl EmbeddingInput {
    pub(super) fn useful_samples(&self) -> usize {
        self.samples.iter().map(Range::len).sum()
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Segment {
    pub(super) start: f32,
    pub(super) end: f32,
    pub(super) speaker: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PostprocessConfig {
    pub(super) min_duration_on: f32,
    pub(super) min_duration_off: f32,
}
impl Default for PostprocessConfig {
    fn default() -> Self {
        Self {
            min_duration_on: 0.3,
            min_duration_off: 0.5,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Cancelled,
    Invalid(&'static str),
    TooLarge,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
fn check(cancelled: &impl Fn() -> bool) -> Result<(), Error> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// Powerset order is silence, individual speakers, then lexicographic pairs.
/// First maximum wins, exactly as Eigen maxCoeff for the original model output.
pub(super) fn decode_powerset(
    values: &[f32],
    classes: usize,
    local_speakers: u8,
) -> Result<ChunkMasks, Error> {
    if !(1..=8).contains(&local_speakers) {
        return Err(Error::Invalid("local speaker count"));
    }
    let mut mapping = vec![0];
    for a in 0..local_speakers {
        mapping.push(1u8 << a);
    }
    for a in 0..local_speakers {
        for b in a + 1..local_speakers {
            mapping.push((1u8 << a) | (1u8 << b));
        }
    }
    if classes != mapping.len() || !values.len().is_multiple_of(classes) {
        return Err(Error::Invalid("powerset shape"));
    }
    let mut frames = Vec::with_capacity(values.len() / classes);
    for row in values.chunks_exact(classes) {
        if row.iter().any(|v| !v.is_finite()) {
            return Err(Error::Invalid("nonfinite segmentation"));
        }
        let mut best = 0;
        for i in 1..classes {
            if row[i] > row[best] {
                best = i;
            }
        }
        frames.push(mapping[best]);
    }
    Ok(ChunkMasks { frames })
}
fn validate_chunks(chunks: &[ChunkMasks], meta: Metadata) -> Result<(), Error> {
    meta.validate()?;
    let valid_bits = ((1u16 << meta.local_speakers) - 1) as u8;
    if chunks
        .iter()
        .any(|c| c.frames.is_empty() || c.frames.iter().any(|m| m & !valid_bits != 0))
    {
        return Err(Error::Invalid("empty frames or invalid speaker mask"));
    }
    Ok(())
}

/// Match native minimum of ten exclusive frames, excluding overlapping speakers.
/// Clips every resulting range to available samples and drops fully padded inputs.
pub(super) fn prepare_embedding_inputs(
    chunks: &[ChunkMasks],
    num_samples: usize,
    meta: Metadata,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<EmbeddingInput>, Error> {
    check(&cancelled)?;
    validate_chunks(chunks, meta)?;
    let mut result = Vec::new();
    for (chunk, masks) in chunks.iter().enumerate() {
        check(&cancelled)?;
        let frame_count = masks.frames.len();
        let offset = chunk
            .checked_mul(meta.window_shift)
            .ok_or(Error::TooLarge)?;
        let sample_at = |frame: usize| -> usize {
            (frame as f32 / frame_count as f32).mul_add(meta.window_size as f32, offset as f32)
                as usize
        };
        for local_speaker in 0..meta.local_speakers {
            let bit = 1u8 << local_speaker;
            if masks.frames.iter().filter(|&&m| m == bit).count() < 10 {
                continue;
            }
            let mut ranges = Vec::new();
            let mut active = None;
            let mut finish = |start: usize, end: usize| {
                let range = sample_at(start).min(num_samples)..sample_at(end).min(num_samples);
                if !range.is_empty() {
                    ranges.push(range);
                }
            };
            for (frame, &mask) in masks.frames.iter().enumerate() {
                if mask == bit {
                    if active.is_none() {
                        active = Some(frame);
                    }
                } else if let Some(start) = active.take() {
                    finish(start, frame);
                }
            }
            if let Some(start) = active {
                finish(start, frame_count - 1);
            }
            if !ranges.is_empty() {
                result.push(EmbeddingInput {
                    chunk,
                    local_speaker,
                    samples: ranges,
                });
            }
        }
    }
    Ok(result)
}

fn positive_top_indices(votes: &[u32], k: usize) -> Vec<usize> {
    let mut candidates: Vec<_> = (0..votes.len()).filter(|&i| votes[i] > 0).collect();
    candidates.sort_unstable_by(|&a, &b| votes[b].cmp(&votes[a]).then(a.cmp(&b)));
    candidates.truncate(k);
    candidates
}

/// Reconstruct final turns from local segmentation and externally assigned
/// embedding labels. `inputs` must contain only rows retained by extraction;
/// their order must match labels exactly. No clustering is done here.
pub(super) fn reconstruct(
    chunks: &[ChunkMasks],
    inputs: &[EmbeddingInput],
    labels: &[usize],
    num_samples: usize,
    meta: Metadata,
    config: PostprocessConfig,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<Segment>, Error> {
    check(&cancelled)?;
    validate_chunks(chunks, meta)?;
    if !config.min_duration_on.is_finite()
        || !config.min_duration_off.is_finite()
        || config.min_duration_on < 0.0
        || config.min_duration_off < 0.0
    {
        return Err(Error::Invalid("duration thresholds"));
    }
    if chunks.is_empty() || num_samples == 0 {
        return Ok(Vec::new());
    }
    if chunks.len() == 1 {
        // Native bypasses clustering for one chunk. Restrict padded output even
        // when input is shorter than a full window (native negative-modulo bug).
        let rows = chunks[0]
            .frames
            .len()
            .min(num_samples / meta.receptive_field_shift + 1);
        return segments_from_active(
            rows,
            meta.local_speakers as usize,
            num_samples,
            meta,
            config,
            |frame, speaker| chunks[0].frames[frame] & (1u8 << speaker) != 0,
            &cancelled,
        );
    }
    if inputs.len() != labels.len() {
        return Err(Error::Invalid("embedding/label count"));
    }
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    let speakers = labels
        .iter()
        .max()
        .and_then(|s| s.checked_add(1))
        .ok_or(Error::TooLarge)?;
    let padded_samples = meta
        .window_size
        .checked_add(
            (chunks.len() - 1)
                .checked_mul(meta.window_shift)
                .ok_or(Error::TooLarge)?,
        )
        .ok_or(Error::TooLarge)?;
    let padded_frames = padded_samples / meta.receptive_field_shift + 1;
    let rows = padded_frames.min(num_samples / meta.receptive_field_shift + 1);
    let cells = rows.checked_mul(speakers).ok_or(Error::TooLarge)?;
    if cells > 200_000_000 {
        return Err(Error::TooLarge);
    }
    let mut votes = vec![0u32; cells];
    let mut count = vec![0u32; rows];
    let mut weight = vec![0u32; rows];
    let mut mapping = vec![[None; 8]; chunks.len()];
    for (input, &label) in inputs.iter().zip(labels) {
        if input.chunk >= chunks.len() || input.local_speaker >= meta.local_speakers {
            return Err(Error::Invalid("embedding speaker index"));
        }
        let slot = &mut mapping[input.chunk][input.local_speaker as usize];
        if slot.replace(label).is_some() {
            return Err(Error::Invalid("duplicate embedding speaker"));
        }
    }
    for (chunk, masks) in chunks.iter().enumerate() {
        check(&cancelled)?;
        let start = meta.frame_start(chunk);
        for (local_frame, &mask) in masks.frames.iter().enumerate() {
            let frame = start + local_frame;
            if frame >= rows {
                break;
            }
            count[frame] += mask.count_ones();
            weight[frame] += 1;
            // Multiple local speakers mapped to one cluster vote once per chunk.
            let mut seen = [usize::MAX; 8];
            let mut len = 0;
            for local in 0..meta.local_speakers {
                if mask & (1u8 << local) == 0 {
                    continue;
                }
                if let Some(label) = mapping[chunk][local as usize] {
                    if !seen[..len].contains(&label) {
                        seen[len] = label;
                        len += 1;
                        votes[frame * speakers + label] += 1;
                    }
                }
            }
        }
    }
    // Store only at most local_speakers chosen IDs per frame, instead of a second
    // dense frame×cluster matrix. usize::MAX marks unused slots.
    let max_active = meta.local_speakers as usize;
    let mut active = vec![usize::MAX; rows * max_active];
    for frame in 0..rows {
        if frame % 1024 == 0 {
            check(&cancelled)?;
        }
        let k = (count[frame] as f32 / (weight[frame] as f32 + 1e-12) + 0.5) as usize;
        let frame_votes = &votes[frame * speakers..(frame + 1) * speakers];
        let selected = positive_top_indices(frame_votes, k);
        for (i, label) in selected.into_iter().enumerate() {
            active[frame * max_active + i] = label;
        }
    }
    drop(votes);
    segments_from_active(
        rows,
        speakers,
        num_samples,
        meta,
        config,
        |frame, speaker| active[frame * max_active..(frame + 1) * max_active].contains(&speaker),
        &cancelled,
    )
}
fn segments_from_active(
    rows: usize,
    speakers: usize,
    num_samples: usize,
    meta: Metadata,
    config: PostprocessConfig,
    is_active: impl Fn(usize, usize) -> bool,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<Segment>, Error> {
    let duration = num_samples as f32 / meta.sample_rate as f32;
    let mut result = Vec::new();
    for speaker in 0..speakers {
        check(cancelled)?;
        let mut raw = Vec::<Segment>::new();
        let mut start = None;
        let mut finish = |a: usize, b: usize| {
            let segment = Segment {
                start: meta.frame_time(a).clamp(0., duration),
                end: meta.frame_time(b).clamp(0., duration),
                speaker,
            };
            if segment.end <= segment.start {
                return;
            }
            if let Some(previous) = raw.last_mut() {
                if previous.end < segment.start
                    && previous.end + config.min_duration_off >= segment.start
                {
                    previous.end = segment.end;
                    return;
                }
            }
            raw.push(segment);
        };
        for frame in 0..rows {
            if frame % 4096 == 0 {
                check(cancelled)?;
            }
            if is_active(frame, speaker) {
                if start.is_none() {
                    start = Some(frame);
                }
            } else if let Some(a) = start.take() {
                finish(a, frame);
            }
        }
        if let Some(a) = start {
            finish(a, rows.saturating_sub(1));
        }
        result.extend(
            raw.into_iter()
                .filter(|s| s.end - s.start > config.min_duration_on),
        );
    }
    result.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.speaker.cmp(&b.speaker)));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn meta() -> Metadata {
        Metadata {
            sample_rate: 16000,
            window_size: 160000,
            window_shift: 16000,
            receptive_field_size: 991,
            receptive_field_shift: 270,
            local_speakers: 3,
        }
    }
    #[test]
    fn powerset_order_and_first_tie() {
        let mut values = vec![0.; 49];
        for i in 0..7 {
            values[i * 7 + i] = 1.;
        }
        assert_eq!(
            decode_powerset(&values, 7, 3).unwrap().frames,
            [0, 1, 2, 4, 3, 5, 6]
        );
        assert_eq!(decode_powerset(&[1.; 7], 7, 3).unwrap().frames, [0]);
    }
    #[test]
    fn silence_has_no_embeddings_or_segments() {
        let c = vec![
            ChunkMasks {
                frames: vec![0; 589]
            };
            2
        ];
        assert!(prepare_embedding_inputs(&c, 176000, meta(), || false)
            .unwrap()
            .is_empty());
        assert!(
            reconstruct(&c, &[], &[], 176000, meta(), Default::default(), || false)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn overlap_excluded_only_for_embeddings() {
        let c = vec![ChunkMasks {
            frames: vec![3; 589],
        }];
        assert!(prepare_embedding_inputs(&c, 160000, meta(), || false)
            .unwrap()
            .is_empty());
        let s = reconstruct(&c, &[], &[], 160000, meta(), Default::default(), || false).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].start, s[1].start);
    }
    #[test]
    fn short_audio_bounds_and_padding() {
        let c = vec![ChunkMasks {
            frames: (0..589).map(|f| if f < 45 { 1 } else { 2 }).collect(),
        }];
        let s = reconstruct(
            &c,
            &[],
            &[],
            8000,
            meta(),
            PostprocessConfig {
                min_duration_on: 0.,
                min_duration_off: 0.,
            },
            || false,
        )
        .unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].speaker, 0);
        assert!(s[0].end <= 0.5);
        assert!(s[0].start >= 0.);
    }
    #[test]
    fn ranges_clipped_to_available_audio() {
        let c = vec![
            ChunkMasks {
                frames: vec![1; 589]
            };
            2
        ];
        let inputs = prepare_embedding_inputs(&c, 20000, meta(), || false).unwrap();
        assert!(inputs
            .iter()
            .flat_map(|i| &i.samples)
            .all(|r| r.end <= 20000 && r.start < r.end));
    }
    #[test]
    fn minimum_ten_exclusive_frames() {
        let mut f = vec![0; 589];
        f[..9].fill(1);
        let c = vec![ChunkMasks { frames: f.clone() }];
        assert!(prepare_embedding_inputs(&c, 160000, meta(), || false)
            .unwrap()
            .is_empty());
        f[9] = 1;
        assert_eq!(
            prepare_embedding_inputs(&[ChunkMasks { frames: f }], 160000, meta(), || false)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn cancellation_before_any_work() {
        let c = vec![ChunkMasks {
            frames: vec![1; 589],
        }];
        assert_eq!(
            prepare_embedding_inputs(&c, 160000, meta(), || true),
            Err(Error::Cancelled)
        );
        assert_eq!(
            reconstruct(&c, &[], &[], 160000, meta(), Default::default(), || true),
            Err(Error::Cancelled)
        );
    }

    #[test]
    fn positive_ranking_is_stable_and_never_invents_zero_vote_speaker() {
        assert!(positive_top_indices(&[0, 0, 0], 2).is_empty());
        assert_eq!(positive_top_indices(&[0, 5, 5, 4], 2), [1, 2]);
        assert_eq!(positive_top_indices(&[0, 0, 3], 3), [2]);
    }

    #[test]
    fn cancellation_between_chunks() {
        let c = vec![
            ChunkMasks {
                frames: vec![1; 589]
            };
            3
        ];
        let calls = std::cell::Cell::new(0);
        let result = prepare_embedding_inputs(&c, 192000, meta(), || {
            calls.set(calls.get() + 1);
            calls.get() > 2
        });
        assert_eq!(result, Err(Error::Cancelled));
    }

    #[test]
    fn missing_embedding_votes_do_not_create_extra_speaker() {
        let c = vec![
            ChunkMasks {
                frames: vec![3; 589]
            };
            2
        ];
        let inputs = vec![
            EmbeddingInput {
                chunk: 0,
                local_speaker: 0,
                samples: std::iter::once(0..1000).collect(),
            },
            EmbeddingInput {
                chunk: 1,
                local_speaker: 0,
                samples: std::iter::once(16000..17000).collect(),
            },
        ];
        let out = reconstruct(
            &c,
            &inputs,
            &[2, 2],
            176000,
            meta(),
            Default::default(),
            || false,
        )
        .unwrap();
        assert!(!out.is_empty());
        assert!(out.iter().all(|s| s.speaker == 2));
    }
}
