# Russian transcription with GigaAM

Kuali can transcribe Russian with Sber's GigaAM-v3 instead of Whisper. It is
an optional second speech-recognition engine: Whisper remains the default, and
existing configurations keep working unchanged.

GigaAM-v3 end-to-end CTC (`e2e_ctc`) outputs punctuated, normalized Russian
text. It supports Russian only.

## Using it

Choose **Solo ruso** (Russian only) in the model picker. Its technical name is
GigaAM v3 E2E CTC (INT8). The download is 225 MB and the RAM estimate is about
450 MB. In `config.toml` it is `model = "gigaam-v3-e2e-ctc"` under `[whisper]`.

Selecting it locks the audio language to Russian and disables the language
selector. GigaAM ignores the custom vocabulary; Kuali keeps it for Whisper.

## Behavior

- Silero VAD gates audio before recognition, exactly as for Whisper.
- GigaAM accepts at most about 25 seconds per call, so speech is decoded in
  chunks of up to 20 seconds split at Silero speech boundaries. A single
  uninterrupted stretch longer than 20 seconds is cut at the 20-second mark.
- GigaAM runs on the CPU. Whisper keeps Metal acceleration.
- The model loads on the first active meeting and unloads after the last, like
  Whisper. Raw audio stays in memory only.
- No new native runtime is added. GigaAM runs on the Sherpa ONNX library that
  Kuali already links for diarization (Rust crate `sherpa-onnx` 1.13.7,
  Apache-2.0; license in `crates/kuali-stt/native/LICENSE.sherpa-onnx`).

## Known limitations

- English terms are often spelled in mixed Latin and Cyrillic letters, such
  as `Hелм` for "Helm", and fillers such as `а` or `хе-хе` are transcribed.
- Words are occasionally merged across a short pause, such as `ячто`.
- CTC decoding takes no prompt, so the custom vocabulary cannot fix either.

## Model download

Kuali downloads the ONNX export made by the Sherpa ONNX project from its
[Hugging Face mirror](https://huggingface.co/csukuangfj/sherpa-onnx-nemo-ctc-punct-giga-am-v3-russian-2025-12-16/tree/4fb5407ff028a69fec516cdf4c10fac9ddea7c16),
pinned to commit `4fb5407ff028a69fec516cdf4c10fac9ddea7c16`. The download host
is huggingface.co, which Kuali already uses for other model weights.

| Installed file | Upstream model | Bytes | License |
| --- | --- | ---: | --- |
| `gigaam-v3-e2e-ctc.int8.onnx` | `model.int8.onnx` (INT8) from Sherpa's GigaAM-v3 export | 224,893,661 | MIT, GigaChat Team |

Expected SHA-256 digest:

```text
d5fea8df94263c285e54b21e5774b707c707192d3bdbeffd7b1eb07fb6743b35  gigaam-v3-e2e-ctc.int8.onnx
```

The 2 KB vocabulary (`tokens.txt` from the same commit, SHA-256
`142de7570b3de5b3035ce111a89c228e80e6085273731d944093ddf24fa539cd`) is bundled
in the source as `crates/kuali-stt/assets/gigaam-v3-e2e-ctc.tokens.txt`.

## Model attribution and license

**GigaAM-v3:** created by Sber's GigaChat Team. Original:
[salute-developers/GigaAM](https://github.com/salute-developers/GigaAM) and
[ai-sage/GigaAM-v3](https://huggingface.co/ai-sage/GigaAM-v3). Kuali uses the
Sherpa ONNX INT8 export pinned above. The model is released under the MIT
license:

```text
MIT License

Copyright (c) 2024 GigaChat Team

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
