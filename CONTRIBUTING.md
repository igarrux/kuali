# Contributing to Kuali

Thank you for helping make private, local-first meeting notes more accessible.
Focused bug reports, reproducible capture tests, documentation fixes, and
small well-tested changes are especially useful.

## Before opening an issue

- Search existing issues and include the platform, Kuali revision, meeting
  source, Whisper model, and transcription language.
- Remove participant names, meeting content, bot tokens, API keys, webhook
  secrets, and other private data from screenshots and logs.
- For capture bugs, explain who joined first, who was muted, whether speakers
  overlapped, and whether the UI activity indicator reacted.

Security vulnerabilities and accidental secret exposure belong in a private
report; see [SECURITY.md](SECURITY.md).

## Development setup

The primary development target is macOS on Apple Silicon. Install:

- Rust 1.89 or newer;
- CMake and a C/C++ toolchain for whisper.cpp;
- Node.js 22 or newer for browser-extension tests; and
- Tauri CLI 2 only when producing an application bundle.

Run the desktop application:

```sh
cargo run -p kuali-app
```

Run the regular validation suites:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd browser-extension && npm test)
```

The interactive Google Meet E2E is intentionally separate because it opens a
real browser meeting and requires user participation:

```sh
cd browser-extension
npm run test:e2e:meet
```

## Architecture and boundaries

| Area | Responsibility |
|---|---|
| `kuali-core` | Shared contracts and configuration |
| `kuali-discord` | Discord gateway, consent, and per-speaker audio |
| `kuali-meet` | Local browser-extension ingest and wire protocol |
| `kuali-stt` | Segmentation, Silero VAD, Whisper, and model storage |
| `kuali-llm` | Provider discovery and structured summaries |
| `kuali-store` | Meeting persistence, search, and export |
| `kuali-engine` | Cross-source lifecycle and concurrency |
| `src-tauri` / `src` | Desktop shell and bilingual interface |

Keep capture paths non-blocking. Raw audio must remain in memory, participant
IDs that cross JavaScript boundaries must remain strings, and simultaneous
meetings must never share lifecycle state.

## Pull requests

1. Keep one reason for change per commit.
2. Add or update the nearest test for every behavior change.
3. Explain privacy, consent, memory, or attribution effects when relevant.
4. Do not commit model weights, meeting data, credentials, generated store ZIPs,
   or local development profiles.
5. Keep developer-facing comments and documentation in English. User-facing UI
   changes must update both English and Spanish localization coverage.

The desktop workspace is MIT licensed. Files under `browser-extension/` are
Apache-2.0 licensed and must retain their SPDX/provenance notices. By submitting
a contribution, you agree to license it under the license of its target area.
