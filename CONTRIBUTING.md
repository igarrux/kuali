# Contributing to Kuali

Thank you for helping build private, useful meeting transcription in the open.
Kuali welcomes code, controlled capture testing, documentation, translations,
accessibility work, design feedback, and reproducible bug reports.

Read the [code of conduct](CODE_OF_CONDUCT.md) before participating. The
[roadmap](ROADMAP.md) describes current priorities without reserving work for a
particular contributor.

## Choose a contribution

| If you want to… | Start here |
|---|---|
| Fix a small, bounded problem | [`good first issue`](https://github.com/igarrux/kuali/labels/good%20first%20issue) |
| Work on a project priority | [`help wanted`](https://github.com/igarrux/kuali/labels/help%20wanted) |
| Report a desktop or website bug | [Bug report](https://github.com/igarrux/kuali/issues/new?template=bug_report.yml) |
| Report voice, roster, mute, attribution, or lifecycle behavior | [Capture report](https://github.com/igarrux/kuali/issues/new?template=capture_problem.yml) |
| Propose a focused improvement | [Feature request](https://github.com/igarrux/kuali/issues/new?template=feature_request.yml) |
| Discuss a broad design change | [Ideas](https://github.com/igarrux/kuali/discussions/categories/ideas) |
| Ask for setup help | [Q&A](https://github.com/igarrux/kuali/discussions/categories/q-a) |

Do not start implementation on a large architectural change until its direction
is agreed in a discussion or issue. Small fixes do not need ceremonial approval.

## Protect meeting data

Before posting logs, screenshots, fixtures, or recordings:

- remove participant names, avatars, usernames, IDs, channel names, and meeting
  content;
- remove Discord bot tokens, API keys, webhook secrets, private URLs, and local
  filesystem paths that identify a person;
- reproduce capture bugs in a controlled test call whenever possible; and
- use [private vulnerability reporting](SECURITY.md) for sensitive security or
  accidental secret exposure.

Real private meeting audio does not belong in the repository, issue tracker, or
test fixtures.

## Development setup

The current packaged target is macOS on Apple Silicon. Install:

- Rust 1.89 or newer;
- CMake and a C/C++ toolchain for whisper.cpp;
- Node.js 22 or newer for browser-extension and website tests;
- [`just`](https://just.systems/) for versioned project commands; and
- [`fzf`](https://junegunn.github.io/fzf/) for the optional interactive command
  picker.

On macOS:

```sh
brew install cmake just fzf node
```

Fork and clone the repository, then create focused branches from `sandbox`:

```sh
git clone git@github.com:YOUR-USER/kuali.git
cd kuali
git remote add upstream https://github.com/igarrux/kuali.git
git fetch upstream
git switch -c fix/short-description upstream/sandbox
```

External pull requests should target `sandbox`. Maintainers promote tested
changes from `sandbox` to `main` for releases.

Run the desktop app:

```sh
just dev
```

Run `just` with no recipe to choose from the project command menu. Use
`just --list` to inspect every available command.

## Validation

Run the nearest tests while developing and the complete validation before a
pull request:

```sh
just check
```

The complete check includes:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test src/i18n.test.mjs
node --test website/tests/site.test.mjs
npm --prefix browser-extension test
npm --prefix browser-extension run package:store
```

The interactive Google Meet E2E is intentionally separate because it opens a
real browser meeting and requires user participation:

```sh
just meet-e2e
```

Mention any test you could not run and why. A capture change should include a
controlled manual test when automation cannot reproduce the platform behavior.

## Architecture and invariants

| Area | Responsibility |
|---|---|
| `kuali-core` | Shared contracts and configuration |
| `kuali-discord` | Discord gateway, consent, and per-speaker audio |
| `kuali-meet` | Local browser-extension ingest and wire protocol |
| `kuali-stt` | Segmentation, Silero VAD, Whisper, and model storage |
| `kuali-llm` | Provider discovery and structured meeting insights |
| `kuali-store` | Meeting persistence, search, and export |
| `kuali-engine` | Cross-source lifecycle and concurrency |
| `src-tauri` / `src` | Desktop shell and bilingual interface |
| `browser-extension` | Browser capture for Meet, Teams, and Zoom |
| `website` | Dependency-free public website deployed through Cloudflare |

Changes must preserve these system boundaries:

1. Capture paths remain non-blocking.
2. Raw audio remains in memory and is not retained as a recording.
3. Participant IDs crossing JavaScript boundaries remain strings.
4. Known platform identity is preferred over inferred diarization.
5. Mixed audio is labeled honestly instead of receiving invented attribution.
6. Simultaneous meetings never share lifecycle, audio, participants, or clocks.
7. External LLM and webhook destinations remain user-configured and optional.

## Code and documentation

- Keep one reason for change per commit and pull request.
- Add or update the nearest test for every behavior change.
- Keep developer-facing comments, commit messages, and documentation in English.
- Update English and Spanish localization coverage for user-facing behavior.
- Run Rust formatting instead of mixing manual formatting with functional work.
- Do not commit model weights, meetings, credentials, generated store ZIPs,
  application bundles, or local development profiles.
- Update `ROADMAP.md`, `WEBSITE.md`, or architecture documentation when a change
  alters the public direction or deployment contract.

## Pull requests

Complete the pull request template. Explain the problem, validation, and any
impact on privacy, consent, attribution, meeting isolation, or external data
destinations. Include redacted before-and-after evidence for UI and capture work
when it materially improves review.

Reviews may request a smaller pull request when unrelated concerns are mixed.
Draft pull requests are welcome for early technical feedback.

## Licensing

The desktop workspace is MIT licensed. Files under `browser-extension/` are
Apache-2.0 licensed and must retain their SPDX and provenance notices. By
submitting a contribution, you agree to license it under the license of its
target area.
