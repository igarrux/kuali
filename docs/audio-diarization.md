# Local audio-file diarization

Kuali processes recorded audio locally and assigns anonymous speaker labels to
timed turns. Diarization answers **who spoke when**. It does not isolate voices
into separate audio files or establish that the same label in two recordings
belongs to the same person. Optional AI analysis can then infer names from
explicit evidence in the transcript, as described below.

The first implementation uses the native Rust bindings for
[Sherpa ONNX 1.13.7](https://github.com/k2-fsa/sherpa-onnx/tree/v1.13.7/sherpa-onnx/rust)
with pyannote segmentation 3.0 and WeSpeaker ResNet34-LM embeddings. Audio is
decoded and resampled to normalized, mono, 16 kHz PCM before inference. The app's
file-import flow also transcribes the recording and requires a separately
installed Whisper model. The standalone benchmark example performs diarization
without transcription.

Use **Importar audio** in the library or Home, choose a recording and leave the
speaker count on Auto unless it is known. Supported inputs are WAV, MP3,
M4A/AAC, FLAC and OGG Vorbis, up to two hours and 4 GiB. The original file is
left in place; the library stores the transcript and raw diarization timeline.

`source.speakerTurns` preserves simultaneous speakers. Transcription processes
each time interval once; overlapping voices receive the explicit label
**Voces superpuestas**, rather than duplicating a mixed recording under two
people's names. This group is excluded from detected-voice counts. Imported
meeting headers show detected voices rather than confirmed attendance, with a
visible reminder that the separation can split or combine voices. An optional
`source.requestedSpeakers` retains the user's count; a mismatch displays a
persistent notice after saving and reopening the meeting. Earlier imports without
this field remain readable. The original
capture date is unknown: `source.importedAt` records import time and the meeting
clock dates are synthetic, while turn timestamps remain offsets into the file.
Imports never automatically send summaries or webhooks.

## Names from the conversation

When AI summaries are enabled in Settings, a completed import first saves its
transcript, then uses the configured summary provider to identify anonymous
participants and generate a summary. Regenerating an imported meeting's summary
also attempts to identify any remaining anonymous participants. With summaries
disabled, the import remains local and keeps its anonymous labels.

The naming request uses transcript text, stable speaker and utterance identifiers,
and timestamps; it does not upload the recording or voice embeddings. The selected
provider may process this text remotely, just as it does for other summaries.

Names require high-confidence evidence: a self-introduction, or a clear direct
address followed by that person's reply. The application verifies the cited
utterances and exact supporting quotes, rejects conflicting mappings and name
collisions, and preserves already named participants and **Voces superpuestas**.
A name merely mentioned in the meeting is not enough. Accepted names update
participant labels before the summary is generated, so the transcript, task
owners and note authors use the same names. Speaker IDs and timings stay intact.

Ambiguous cases remain **Participante N**. This is contextual inference, not voice
identity recognition or a guarantee of correctness; transcription and diarization
errors can affect it. Provider failures leave the saved transcript available,
and analysis can be retried by regenerating the summary.

## Models and installation

The backend downloads two fixed model files into the model directory supplied by
the caller. Model downloads are the only network requests made by this module;
audio samples, speaker embeddings, and diarization results are not uploaded.
The two downloads total **32,523,463 bytes** (about 31 MiB). Loading also creates
a verified local copy of the embedding model with corrected preprocessing
metadata, bringing the complete cache to **59,054,052 bytes** (about 56 MiB).
Python, Hugging Face credentials, and a GPU are not required.

| Installed file | Upstream model | Bytes | License |
| --- | --- | ---: | --- |
| `pyannote-segmentation-3.0.onnx` | [Sherpa's pyannote segmentation export](https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/tree/9403a6902bb58e3d5ae8c7e77c3422de279db2e0) | 5,992,913 | MIT, CNRS |
| `wespeaker-resnet34-lm.onnx` | [Sherpa's WeSpeaker ResNet34-LM export](https://github.com/k2-fsa/sherpa-onnx/releases/tag/speaker-recongition-models) | 26,530,550 | CC BY 4.0, WeSpeaker |

Direct download URLs, if preparing a machine without internet access:

```text
https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/9403a6902bb58e3d5ae8c7e77c3422de279db2e0/model.onnx
https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx
```

Save these as the **installed filenames** above in the same directory. Expected
SHA-256 digests, verified from the maintainer's
[segmentation metadata](https://huggingface.co/api/models/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/tree/main)
and [embedding checksums](https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/checksum.txt):

```text
220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079  pyannote-segmentation-3.0.onnx
e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012  wespeaker-resnet34-lm.onnx
```

### Embedding preprocessing compatibility

The pinned WeSpeaker export omits the `feature_normalize_type` metadata used by
Sherpa. WeSpeaker's own
[inference implementation](https://github.com/wenet-e2e/wespeaker/blob/9fecd6cb4f47475d01761d87c826298dff4ef18c/runtime/core/speaker/speaker_engine.cc#L132-L142)
subtracts the temporal mean of each filterbank feature before ONNX inference.
Sherpa's
[general embedding extractor](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/csrc/speaker-embedding-extractor-general-impl.h#L62-L75)
requires `feature_normalize_type=global-mean` to apply that operation. Omitting
it distorts the distances used to group voices, even when a result happens to
contain the requested number of labels.

Kuali retains the original SHA-verified download and appends one 39-byte ONNX
`ModelProto.metadata_props` entry to a separate local copy. No weights or graph
operators change. This transformation is restricted to the pinned model; the
complete derived file is also checked against a fixed size and SHA-256 before
publication and native loading:

```text
26,530,589 bytes
ce2c5db3fb2c438f1d58de35577c951bc27ea0d41ef74968de4a480a1a59e39b  wespeaker-resnet34-lm-cmn.onnx
```

The prepared copy is reusable offline. A corrupt copy is rebuilt from the
verified original; a corrupt original is rejected. Concurrent preparation uses
distinct temporary files and atomic replacement of the completed cache file.
This corrects the input contract; automatic counting still requires separate
quality evaluation and is not an exact attendance count.

Automatic clustering uses a cosine-distance cutoff of **0.9** with these
normalized features. Sherpa's generic 0.5 default fragments the four-speaker
reference into nine labels after correcting preprocessing. At 0.9 it preserves
the same ten attributed turns as the supplied-count run. This value is also used
in the upstream diarization guide, but thresholds are model- and data-dependent;
the public regression fixtures and real recordings must be checked separately.
A supplied speaker count bypasses this distance cutoff in the native engine.

Downloads use `.part` files, validate exact size and SHA-256, and promote the
completed file only after validation. Cancelled or failed downloads clean up
their temporary file. A previously corrupted model is replaced only after its
replacement has downloaded successfully. There is a 15-second connection timeout
and a 15-minute total request timeout including the response body. Stalled
network requests remain cancellable. The caller serializes jobs and access to
the model directory.

Each load checks both models' sizes and complete digests before constructing the
native engine. A complete verified cache works offline. To recover from a failed
download, retry the operation; partially downloaded data is not resumed.
Cache verification during acquisition uses asynchronous disk reads, including
cancellation while waiting for I/O. The final synchronous verification and
native model load run on the caller's blocking worker.

Building the app is separate from downloading its models. The pinned Rust crate
defaults to static linking and downloads the matching native Sherpa/ONNX Runtime
build when it is not cached. Its bundled macOS runtime requires macOS 13.4 or
later; Kuali's installer and Homebrew cask enforce that minimum. CPU prebuilt
libraries are available for macOS,
Linux, and Windows on x64 and arm64. For an offline build,
`SHERPA_ONNX_LIB_DIR` or `SHERPA_ONNX_ARCHIVE_DIR` can point to prepared native
artifacts; see the upstream
[build implementation](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/rust/sherpa-onnx-sys/build.rs).

## Behavior and quality limits

- Automatic speaker counting is the default. A known count from 1 through 32
  can be supplied as guidance to the grouping stage. If a nonempty result contains
  a different number of voices, Kuali keeps the result and transcribes it. The
  saved meeting reports the discrepancy instead of discarding the import or
  inventing speakers to reach the supplied count. An empty result remains
  available for no-speech handling.
  If the requested count exceeds the number of extracted embeddings, the
  upstream clustering routine can assign all embeddings to one speaker. A known
  count is guidance, not proof that the audio contains enough distinct speech to
  identify that many voices. Use Auto unless the count is known. See the pinned
  [clustering implementation](https://github.com/csukuangfj/hclust-cpp/blob/2026-02-25/fastcluster.cpp).
- Each result is sorted by time. Labels start at zero internally and follow the
  order in which speakers first appear. Overlapping turns are retained, and all
  timestamps are clipped to the actual input duration.
- One invocation consumes a complete recording of at most two hours. The limit
  bounds memory and the cost of grouping speaker embeddings; the algorithm still
  keeps recording-level intermediate data in memory. Actual RAM and processing
  time must be measured on the target hardware. CPU inference uses up to four
  available threads.
- Background noise, reverberation, distant microphones, very brief turns,
  similar voices, and overlapping speech make speaker assignment harder.
  Pyannote segmentation 3.0 models at most three local speakers in a 10-second
  window and two simultaneous voices; recording-level grouping supports more
  speakers across windows. See the original
  [segmentation model card](https://huggingface.co/pyannote/segmentation-3.0).
- WeSpeaker's selected model is trained on VoxCeleb2 Dev and is labeled English
  upstream. It is a candidate for Spanish recordings, but **this exact pipeline
  has not been benchmarked for Spanish in Kuali**. No Spanish accuracy or speed
  guarantee is attached to it. See the
  [WeSpeaker model card](https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34-LM).
- The safe native Rust API runs inference as a blocking call and does not expose
  a cancellation callback. Cancel requested during that call remains pending
  until native inference finishes; the result is then discarded. The inference
  stage reports indeterminate progress rather than estimating a percentage.
  Download cancellation is responsive independently of inference.

The Python-based
[pyannote community-1 pipeline](https://huggingface.co/pyannote/speaker-diarization-community-1)
is a useful local comparison baseline. Its published diarization error rates
include 11.2% on VoxConverse and 17.0–19.9% on AMI, with overlaps scored and no
forgiveness collar. Those results belong to that complete pipeline; they are not
measurements of Kuali or this Sherpa combination. DER also measures missed and
extra speech time, not just speaker identity errors, and is not a sentence
accuracy percentage.

## Reproducible evaluation

An initial **0.1.16 smoke run, before the preprocessing correction**, on
**Apple M4 with 16 GB RAM** processed the upstream
16-second English fixture with automatic counting, returning two speakers and
four turns. Loading took **1.28 seconds** and inference **0.75 seconds**
(RTF **0.047**). This used Rust's development profile with the prebuilt native
libraries. It verifies this short input on that machine; it is not a release
benchmark, Spanish quality measurement, or long-recording performance claim.
Requesting 32 speakers on the same fixture completed without a native crash but
returned one speaker inside the native library. That mismatch previously caused
Kuali to reject the import. It now preserves the result, retains the requested
count and shows the discrepancy in the saved meeting for review.

The preprocessing regression was checked against the official
[four-speaker example and its ten reference turns](https://k2-fsa.github.io/sherpa/onnx/speaker-diarization/models.html).
With a supplied count of four, the corrected model matches all ten speaker
assignments, including repeated speakers. After matching anonymous labels, the
time agreement with the documented reference is 30.593 of 30.595 seconds (2 ms
of timestamp rounding), versus 20.181 seconds with the original preprocessing.
This is agreement with one documented fixture, not a corpus DER or a general
accuracy estimate. The existing two-speaker English fixture keeps identical
turns after the correction, and two additional public two-speaker fixtures
retain two speakers. With the corrected preprocessing and automatic threshold,
the four fixtures return 4, 2, 2, and 2 voices respectively. The four-speaker
reference preserves all ten attributed turns in both modes. The other three
fixtures are count/stability smoke checks without independent reference labels;
they do not establish attribution accuracy.

An opt-in native regression checks every identity and turn boundary in the
four-speaker reference, in both automatic and supplied-count modes. It requires
local models and the public fixture and never downloads them:

```sh
KUALI_DIARIZATION_MODELS_DIR=/path/to/diarization-models \
KUALI_DIARIZATION_TEST_AUDIO=/path/to/0-four-speakers-zh.wav \
cargo test --locked -p kuali-stt --test diarization_reference -- --ignored
```

Use the native example to run the same model path outside the UI:

```sh
cargo run --release -p kuali-stt --example diarize_file -- /path/to/recording.wav /path/to/diarization-models 2
```

The arguments are the recording, model directory, and optional known speaker
count. Omit the last argument to evaluate automatic counting. Missing models
are downloaded and verified. The example writes RTTM turns to standard output
and timing information to standard error. For example:

```sh
cargo run --release -p kuali-stt --example diarize_file -- /path/to/recording.wav /path/to/diarization-models > hypothesis.rttm
```

1. Start with a native smoke run using the upstream
   [16-second English two-speaker fixture](https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/1-two-speakers-en.wav).
   This validates model acquisition, native loading and timed output. It does not
   validate Spanish quality.
2. Prepare representative, consented Spanish recordings and manually annotate
   speaker turns in RTTM. Include clean interviews, room microphones, brief
   interjections, overlapping voices, and the actual channels used by the app.
   Keep development and final evaluation recordings separate.
3. Run release builds on the intended hardware. Record OS, CPU, RAM, model
   digests, audio duration, number of true speakers, inferred count, and inference
   time. Measure cold download/loading separately from warmed inference. Report
   real-time factor as `inference_seconds / audio_seconds` and peak process RAM.
4. Compare against reference RTTM using
   [pyannote.metrics](https://pyannote.github.io/pyannote-metrics/), which may live
   in a separate evaluation environment. Use consistent file identifiers, score
   overlapping speech, and report DER with a zero-second collar. Include missed
   speech, false alarm and speaker confusion separately. If also reporting a
   relaxed collar, clearly label that result.
5. Evaluate both automatic counting and a supplied true count. Review the
   count errors and performance by recording type. If testing other grouping
   thresholds, tune only on the development set, then freeze parameters before
   evaluating the held-out set.
6. Test cancellation during model download and inference, then repeat a warm run
   with networking disabled. No success result should be committed for a job
   cancelled while inference was running.

## Boundary for future live diarization

The reusable backend accepts PCM independently of file decoding and transcription
and returns `SpeakerSegment { speaker_id, start_ms, end_ms }`. `offset_ms` adds
the recording's timeline origin. It **does not** preserve identity across
separate invocations. Concatenating chunk results would incorrectly treat local
speaker labels as stable identities.

A live implementation needs a session with overlapping windows, an embedding
history, incremental speaker matching, provisional versus final turns, and a
policy for correcting recent labels. Sherpa exposes separate
[speaker embedding extraction and matching APIs](https://github.com/k2-fsa/sherpa-onnx/blob/v1.13.7/sherpa-onnx/rust/sherpa-onnx/src/speaker_embedding.rs)
that can reuse the installed embedding model. A final pass over the completed
recording can refine provisional output. The current implementation provides
**offline diarization only**; no streaming latency is claimed. The
[Diart research implementation](https://github.com/juanmc2005/diart) is a useful
reference for incremental clustering and overlap handling.

## Model attribution and licenses

**WeSpeaker ResNet34-LM:** supplied by the WeSpeaker project, trained on VoxCeleb2
Dev, and converted/annotated for Sherpa ONNX by the Sherpa project. Original:
[Wespeaker/wespeaker-voxceleb-resnet34-LM](https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34-LM).
License: [Creative Commons Attribution 4.0 International](https://creativecommons.org/licenses/by/4.0/).
Kuali uses the pinned Sherpa export without further changes to its weights.
Model attribution should accompany distributions of the weights. The
[upstream pretrained-model license policy](https://github.com/wenet-e2e/wespeaker/blob/master/docs/pretrained.md)
distinguishes model licenses from the toolkit's Apache-2.0 source license.

**Pyannote segmentation 3.0:** original model by the pyannote project, trained by
Séverin Baroudi, exported to ONNX by the Sherpa maintainer. Original:
[pyannote/segmentation-3.0](https://huggingface.co/pyannote/segmentation-3.0).
Kuali uses the pinned FP32 export without further changes. The export's
[MIT license](https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/raw/main/LICENSE)
is reproduced below and must accompany copies of those weights:

```text
MIT License

Copyright (c) 2022 CNRS

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Sherpa ONNX's Rust wrapper and main project use Apache-2.0; its native artifacts
also carry their own third-party notices. Those runtime notices are separate
from the model attributions above.
