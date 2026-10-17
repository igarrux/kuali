//! Average-linkage speaker clustering with support filtering and centroid reassignment.
//! Follows pyannote.audio 3.1.1 AgglomerativeClustering; no recording data is embedded.
//! https://github.com/pyannote/pyannote-audio/blob/3.1.1/pyannote/audio/pipelines/clustering.py
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Error {
    Cancelled,
    Invalid(String),
    TooLarge,
    Native(i32),
}
impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::Invalid(s.to_owned())
    }
}
impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Invalid(s)
    }
}

unsafe extern "C" {
    fn kuali_average_linkage(
        n: i32,
        distances: *mut f64,
        merge: *mut i32,
        heights: *mut f64,
    ) -> i32;
}

#[derive(Clone, Copy, Debug)]
struct Config {
    threshold: f64,
    min_training_seconds: f64,
    min_cluster_size: usize,
    expected_speakers: Option<usize>,
}

#[derive(Debug)]
struct Outcome {
    labels: Vec<i32>,
    #[cfg(test)]
    effective_min_cluster_size: usize,
}

fn allocated<T: Clone>(n: usize, value: T) -> Result<Vec<T>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Error::TooLarge)?;
    v.resize(n, value);
    Ok(v)
}
fn check(cancelled: &mut impl FnMut() -> bool) -> Result<(), Error> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn normalize(row: &mut [f64]) -> Result<(), Error> {
    let norm = row.iter().map(|x| x * x).sum::<f64>().sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err("invalid zero or non-finite embedding/centroid".into());
    }
    for x in row {
        *x /= norm;
    }
    Ok(())
}
fn root(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}
fn cut(n: usize, children: &[(usize, usize)], count: usize) -> Result<Vec<usize>, Error> {
    let mut parent = allocated(n, 0)?;
    for (i, p) in parent.iter_mut().enumerate() {
        *p = i;
    }
    let mut representatives = allocated(n + children.len(), 0)?;
    for (i, r) in representatives[..n].iter_mut().enumerate() {
        *r = i;
    }
    for (i, &(a, b)) in children.iter().take(count).enumerate() {
        let left = root(&mut parent, representatives[a]);
        let right = root(&mut parent, representatives[b]);
        parent[right] = left;
        representatives[n + i] = left;
    }
    let mut ids = allocated(n, usize::MAX)?;
    let mut next = 0;
    let mut labels = allocated(n, 0)?;
    for (i, label) in labels.iter_mut().enumerate() {
        let r = root(&mut parent, i);
        if ids[r] == usize::MAX {
            ids[r] = next;
            next += 1;
        }
        *label = ids[r];
    }
    Ok(labels)
}
fn cluster_counts(labels: &[usize]) -> Result<Vec<usize>, Error> {
    let mut counts = allocated(labels.iter().copied().max().unwrap_or(0) + 1, 0)?;
    for &k in labels {
        counts[k] += 1;
    }
    Ok(counts)
}
fn choose_cut(
    n: usize,
    merges: &[i32],
    heights: &[f64],
    config: Config,
    min_size: usize,
) -> Result<Vec<usize>, Error> {
    let mut children = allocated(n - 1, (0, 0))?;
    let mut sizes = allocated(n + n - 1, 1usize)?;
    let mut large_after = allocated(n - 1, 0usize)?;
    let mut large = if min_size == 1 { n } else { 0 };
    for i in 0..n - 1 {
        let node = |reference: i32| -> Result<usize, Error> {
            let result = if reference < 0 {
                usize::try_from(-(i64::from(reference)) - 1).map_err(|_| "bad dendrogram leaf")?
            } else if reference > 0 {
                n.checked_add(reference as usize - 1)
                    .ok_or(Error::TooLarge)?
            } else {
                return Err("zero dendrogram reference".into());
            };
            if result >= n + i || (reference < 0 && result >= n) {
                return Err("forward dendrogram reference".into());
            }
            Ok(result)
        };
        let a = node(merges[i])?;
        let b = node(merges[n - 1 + i])?;
        if a == b || !heights[i].is_finite() || (i > 0 && heights[i] < heights[i - 1]) {
            return Err("invalid average-linkage dendrogram".into());
        }
        children[i] = (a, b);
        let size = sizes[a].checked_add(sizes[b]).ok_or(Error::TooLarge)?;
        sizes[n + i] = size;
        large += usize::from(size >= min_size);
        large -= usize::from(sizes[a] >= min_size) + usize::from(sizes[b] >= min_size);
        large_after[i] = large;
    }
    let mut merge_count = heights.partition_point(|&height| height <= config.threshold);
    let mut labels = cut(n, &children, merge_count)?;
    let counts = cluster_counts(&labels)?;
    if let Some(requested) = config.expected_speakers {
        let target = requested.min(n);
        if target == n && min_size == 1 {
            return cut(n, &children, 0);
        }
        if counts.iter().filter(|&&size| size >= min_size).count() != target {
            let mut order = allocated(n - 1, 0usize)?;
            for (i, value) in order.iter_mut().enumerate() {
                *value = i;
            }
            order.sort_by(|&a, &b| {
                (heights[a] - config.threshold)
                    .abs()
                    .total_cmp(&(heights[b] - config.threshold).abs())
                    .then(a.cmp(&b))
            });
            let mut best_error = target.abs_diff(1);
            merge_count = n - 1;
            for i in order {
                if sizes[n + i] < min_size {
                    continue;
                }
                let error = large_after[i].abs_diff(target);
                if error < best_error {
                    best_error = error;
                    merge_count = i + 1;
                }
                if error == 0 {
                    merge_count = i + 1;
                    break;
                }
            }
            labels = cut(n, &children, merge_count)?;
        }
    }
    Ok(labels)
}

/// L2 is used for cosine distances, while centroids average original embeddings
/// matching pyannote's average-linkage path. Label numbers are arbitrary.
fn cluster_embeddings(
    embeddings: &[f32],
    dimensions: usize,
    speech_seconds: &[f64],
    config: Config,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Outcome, Error> {
    check(&mut cancelled)?;
    if dimensions == 0
        || !embeddings.len().is_multiple_of(dimensions)
        || config.min_cluster_size == 0
        || !config.threshold.is_finite()
        || !(0.0..=2.0).contains(&config.threshold)
        || !config.min_training_seconds.is_finite()
        || config.min_training_seconds < 0.0
        || config.expected_speakers == Some(0)
    {
        return Err("invalid clustering input/configuration".into());
    }
    let n = embeddings.len() / dimensions;
    if n == 0
        || n > i32::MAX as usize
        || speech_seconds.len() != n
        || speech_seconds.iter().any(|s| !s.is_finite() || *s < 0.0)
    {
        return Err("invalid embedding dimensions/durations".into());
    }
    let mut unit = allocated(embeddings.len(), 0f64)?;
    for (input, output) in embeddings
        .chunks_exact(dimensions)
        .zip(unit.chunks_exact_mut(dimensions))
    {
        check(&mut cancelled)?;
        for (a, b) in input.iter().zip(output.iter_mut()) {
            *b = f64::from(*a);
        }
        normalize(output)?;
    }
    let mut train = Vec::new();
    train.try_reserve(n).map_err(|_| Error::TooLarge)?;
    for (i, &seconds) in speech_seconds.iter().enumerate() {
        if seconds >= config.min_training_seconds {
            train.push(i);
        }
    }
    // A short recording should still return an honest available result.
    if train.len() < 2 {
        train.clear();
        train.extend(0..n);
    }
    let count = train.len();
    let min_size = config
        .min_cluster_size
        .min(((0.1 * count as f64).round_ties_even() as usize).max(1));
    let mut labels = if count == 1 {
        vec![0]
    } else {
        let pairs = count
            .checked_mul(count - 1)
            .and_then(|x| x.checked_div(2))
            .ok_or(Error::TooLarge)?;
        let mut distances = allocated(pairs, 0f64)?;
        let mut pos = 0;
        for (i, &a) in train.iter().enumerate() {
            check(&mut cancelled)?;
            for &b in train.iter().skip(i + 1) {
                let cosine: f64 = unit[a * dimensions..(a + 1) * dimensions]
                    .iter()
                    .zip(&unit[b * dimensions..(b + 1) * dimensions])
                    .map(|(x, y)| x * y)
                    .sum();
                distances[pos] = (1.0 - cosine).clamp(0.0, 2.0);
                pos += 1;
            }
        }
        let mut merges = allocated((count - 1).checked_mul(2).ok_or(Error::TooLarge)?, 0i32)?;
        let mut heights = allocated(count - 1, 0f64)?;
        check(&mut cancelled)?;
        // SAFETY: count is in 2..=i32::MAX. The writable, disjoint buffers stay
        // alive for the call and contain count*(count-1)/2 distances,
        // 2*(count-1) merge references, and count-1 heights. The C++ bridge
        // validates pointers/count and catches exceptions; Rust supplies lengths.
        // Cancellation is checked immediately before/after the native call.
        let code = unsafe {
            kuali_average_linkage(
                count as i32,
                distances.as_mut_ptr(),
                merges.as_mut_ptr(),
                heights.as_mut_ptr(),
            )
        };
        check(&mut cancelled)?;
        if code != 0 {
            return Err(Error::Native(code));
        }
        choose_cut(count, &merges, &heights, config, min_size)?
    };
    let counts = cluster_counts(&labels)?;
    let mut sums = allocated(
        counts
            .len()
            .checked_mul(dimensions)
            .ok_or(Error::TooLarge)?,
        0f64,
    )?;
    for (&index, &label) in train.iter().zip(&labels) {
        for d in 0..dimensions {
            sums[label * dimensions + d] += f64::from(embeddings[index * dimensions + d]);
        }
    }
    let mut large = Vec::new();
    large
        .try_reserve(counts.len())
        .map_err(|_| Error::TooLarge)?;
    for (k, &size) in counts.iter().enumerate() {
        if size >= min_size {
            large.push(k);
        }
    }
    // One supported identity needs no centroid comparison. Valid opposing
    // vectors may have a zero mean without making that assignment ambiguous.
    if large.len() <= 1 {
        check(&mut cancelled)?;
        let labels = allocated(n, 0i32)?;
        check(&mut cancelled)?;
        return Ok(Outcome {
            labels,
            #[cfg(test)]
            effective_min_cluster_size: min_size,
        });
    } else {
        let mut normalized = allocated(sums.len(), 0f64)?;
        normalized.copy_from_slice(&sums);
        for row in normalized.chunks_exact_mut(dimensions) {
            normalize(row)?;
        }
        let mut destination = allocated(counts.len(), 0usize)?;
        for k in 0..counts.len() {
            if counts[k] >= min_size {
                destination[k] = k;
                continue;
            }
            let mut best = (large[0], f64::NEG_INFINITY);
            for &candidate in &large {
                let score = normalized[k * dimensions..(k + 1) * dimensions]
                    .iter()
                    .zip(&normalized[candidate * dimensions..(candidate + 1) * dimensions])
                    .map(|(a, b)| a * b)
                    .sum::<f64>();
                if score > best.1 {
                    best = (candidate, score);
                }
            }
            destination[k] = best.0;
        }
        for label in &mut labels {
            *label = destination[*label];
        }
    }
    let mut centers = allocated(
        large.len().checked_mul(dimensions).ok_or(Error::TooLarge)?,
        0f64,
    )?;
    for (&index, &label) in train.iter().zip(&labels) {
        let output = large
            .binary_search(&label)
            .map_err(|_| "invalid reassigned centroid")?;
        for d in 0..dimensions {
            centers[output * dimensions + d] += f64::from(embeddings[index * dimensions + d]);
        }
    }
    for center in centers.chunks_exact_mut(dimensions) {
        normalize(center)?;
    }
    let mut assigned = allocated(n, 0i32)?;
    for (i, row) in unit.chunks_exact(dimensions).enumerate() {
        check(&mut cancelled)?;
        let mut best = (0i32, f64::NEG_INFINITY);
        for (k, center) in centers.chunks_exact(dimensions).enumerate() {
            let score = row.iter().zip(center).map(|(a, b)| a * b).sum::<f64>();
            if score > best.1 {
                best = (k as i32, score);
            }
        }
        assigned[i] = best.0;
    }
    Ok(Outcome {
        labels: assigned,
        #[cfg(test)]
        effective_min_cluster_size: min_size,
    })
}

/// Cluster available voice embeddings, retaining fewer speakers when evidence is insufficient.
pub(super) fn cluster(
    embeddings: &[f32],
    dim: usize,
    useful_samples: &[usize],
    sample_rate: u32,
    expected_speakers: Option<u32>,
    cancel: &AtomicBool,
) -> Result<Vec<usize>, Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if sample_rate == 0 {
        return Err(Error::Invalid("zero sample rate".into()));
    }
    if dim == 0 {
        return Err(Error::Invalid("zero embedding dimension".into()));
    }
    if embeddings.is_empty() && useful_samples.is_empty() {
        return Ok(Vec::new());
    }
    let mut durations = allocated(useful_samples.len(), 0f64)?;
    for (out, &samples) in durations.iter_mut().zip(useful_samples) {
        *out = samples as f64 / f64::from(sample_rate);
    }
    let config = Config {
        threshold: 0.5,
        min_training_seconds: 0.0,
        min_cluster_size: 12,
        expected_speakers: expected_speakers.map(|n| n as usize),
    };
    let outcome = cluster_embeddings(embeddings, dim, &durations, config, || {
        cancel.load(Ordering::Relaxed)
    })?;
    let mut labels = allocated(outcome.labels.len(), 0usize)?;
    for (out, label) in labels.iter_mut().zip(outcome.labels) {
        *out = label as usize;
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            threshold: 0.5,
            min_training_seconds: 0.0,
            min_cluster_size: 12,
            expected_speakers: None,
        }
    }
    fn speakers(labels: &[i32]) -> usize {
        labels
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    }

    #[test]
    fn requested_count_above_available_preserves_distinct_voices() {
        for data in [vec![1., 0., 0.9, 0.1], vec![1., 0., 0.9, 0.1, 0.8, 0.2]] {
            let n = data.len() / 2;
            let mut cfg = config();
            cfg.expected_speakers = Some(4);
            let output = cluster_embeddings(&data, 2, &vec![2.; n], cfg, || false).unwrap();
            assert_eq!(speakers(&output.labels), n);
        }
    }
    #[test]
    fn requested_count_does_not_fabricate_identical_voices() {
        let mut cfg = config();
        cfg.expected_speakers = Some(4);
        let output =
            cluster_embeddings(&[1., 0., 1., 0., 1., 0.], 2, &[2.; 3], cfg, || false).unwrap();
        assert_eq!(speakers(&output.labels), 1);
    }
    #[test]
    fn one_requested_speaker_accepts_opposing_valid_embeddings() {
        let cancel = AtomicBool::new(false);
        assert_eq!(
            cluster(&[1., 0., -1., 0.], 2, &[16000; 2], 16000, Some(1), &cancel),
            Ok(vec![0, 0])
        );
    }
    #[test]
    fn small_cluster_is_assigned_to_supported_speaker() {
        let mut data = Vec::new();
        for _ in 0..12 {
            data.extend([1., 0.]);
        }
        for _ in 0..12 {
            data.extend([0., 1.]);
        }
        data.extend([0.8, 0.7]);
        let mut cfg = config();
        cfg.threshold = 0.1;
        let output = cluster_embeddings(&data, 2, &[2.; 25], cfg, || false).unwrap();
        assert_eq!(output.effective_min_cluster_size, 2);
        assert_eq!(speakers(&output.labels), 2);
        assert_eq!(output.labels[24], output.labels[0]);
        assert_ne!(output.labels[0], output.labels[12]);
    }
    #[test]
    fn longer_recordings_keep_brief_supported_voice_and_absorb_isolated_fragments() {
        const DIM: usize = 18;
        for repetitions in [1, 4] {
            let frequent = 64 * repetitions;
            let mut data = Vec::new();
            for speaker in 0..3 {
                let count = if speaker == 2 { 12 } else { frequent };
                for index in 0..count {
                    let mut row = [0.; DIM];
                    row[speaker] = 1.;
                    row[3 + speaker] = if index % 2 == 0 { 0.04 } else { -0.04 };
                    data.extend(row);
                }
            }
            // Each fragment is far from every voice and other fragments, but
            // its closest supported voice is the first. It has no repeat support.
            for dimension in 6..DIM {
                let mut row = [0.; DIM];
                row[0] = 0.25;
                row[dimension] = 1.;
                data.extend(row);
            }
            let n = data.len() / DIM;
            let output = cluster_embeddings(&data, DIM, &vec![1.; n], config(), || false).unwrap();
            assert_eq!(speakers(&output.labels), 3);
            for speaker in 0..3 {
                let start = speaker * frequent;
                let count = if speaker == 2 { 12 } else { frequent };
                assert!(output.labels[start..start + count]
                    .iter()
                    .all(|&label| label == output.labels[start]));
            }
            assert_ne!(output.labels[0], output.labels[frequent]);
            assert_ne!(output.labels[0], output.labels[2 * frequent]);
            assert_ne!(output.labels[frequent], output.labels[2 * frequent]);
            assert!(output.labels[2 * frequent + 12..]
                .iter()
                .all(|&label| label == output.labels[0]));
        }
    }
    #[test]
    fn adaptive_minimum_uses_ties_to_even() {
        for (count, expected) in [(15, 2), (25, 2), (45, 4)] {
            let output =
                cluster_embeddings(&vec![1.; count], 1, &vec![1.; count], config(), || false)
                    .unwrap();
            assert_eq!(output.effective_min_cluster_size, expected);
        }
    }
    #[test]
    fn all_short_samples_are_not_discarded() {
        let mut cfg = config();
        cfg.min_training_seconds = 5.;
        let output = cluster_embeddings(&[1., 0., 0., 1.], 2, &[0.5, 0.5], cfg, || false).unwrap();
        assert_eq!(speakers(&output.labels), 2);
    }
    #[test]
    fn cancellation_stops_distance_construction() {
        let n = 20;
        let mut calls = 0;
        let result = cluster_embeddings(&vec![1.; n], 1, &vec![1.; n], config(), || {
            calls += 1;
            calls >= n + 3
        });
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(calls, n + 3);
    }
    #[test]
    fn cancellation_is_checked_after_native_linkage() {
        let mut calls = 0;
        let result = cluster_embeddings(&[1., 2., 3., 4.], 1, &[1.; 4], config(), || {
            calls += 1;
            calls >= 11
        });
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(calls, 11);
    }
    #[test]
    fn invalid_or_zero_vectors_return_errors() {
        for data in [vec![0., 0.], vec![f32::NAN, 1.], vec![f32::INFINITY, 1.]] {
            assert!(matches!(
                cluster_embeddings(&data, 2, &[1.], config(), || false),
                Err(Error::Invalid(_))
            ));
        }
        assert!(cluster_embeddings(&[1.], 2, &[1.], config(), || false).is_err());
    }
    #[test]
    fn oversized_allocation_returns_error() {
        assert!(matches!(
            allocated::<f64>(usize::MAX, 0.),
            Err(Error::TooLarge)
        ));
    }
    #[test]
    fn facade_rejects_zero_rate_and_honors_cancellation() {
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            cluster(&[1.], 1, &[16000], 0, None, &cancel),
            Err(Error::Invalid(_))
        ));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            cluster(&[1.], 1, &[16000], 16000, None, &cancel),
            Err(Error::Cancelled)
        );
    }
    #[test]
    fn empty_embeddings_preserve_short_recording_and_silence_paths() {
        let cancel = AtomicBool::new(false);
        assert_eq!(cluster(&[], 256, &[], 16000, None, &cancel), Ok(Vec::new()));
        assert!(matches!(
            cluster(&[], 0, &[], 16000, None, &cancel),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            cluster(&[], 256, &[], 0, None, &cancel),
            Err(Error::Invalid(_))
        ));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            cluster(&[], 256, &[], 16000, None, &cancel),
            Err(Error::Cancelled)
        );
    }
}
