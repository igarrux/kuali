# Kuali roadmap

Kuali is an open-source, local-first meeting transcription project. The roadmap
describes areas where contributions are useful; it is not a promise of dates or
a reason to compromise privacy, consent, speaker attribution, or meeting
isolation.

## Project principles

Every roadmap item must preserve these invariants:

1. Raw meeting audio is processed in memory and is not retained as a recording.
2. Transcription remains local and usable without an external LLM.
3. Participant identity comes from the meeting platform when available; Kuali
   does not pretend mixed audio has reliable attribution.
4. Simultaneous meetings never share participants, clocks, audio, or lifecycle
   state.
5. Optional destinations receive data only after the user configures them.
6. User-facing behavior and documentation remain available in English and
   Spanish.

## Stable foundations

- Discord voice capture, consent notices, live attribution, and result delivery.
- Google Meet capture through the browser extension.
- Local Whisper transcription with Silero voice activity detection.
- Searchable meeting library, summaries, decisions, questions, and tasks.
- Concurrent meetings and signed Standard Webhooks.
- Guided setup, model management, automatic updates, and macOS distribution.

## Active priorities

### Windows support

- Package and test the desktop application on Windows.
- Validate CUDA acceleration and CPU fallbacks.
- Document model storage, startup behavior, browser integration, and updates.
- Add release automation without weakening platform security defaults.

### Browser-platform coverage

- Expand real-world Microsoft Teams test coverage across meeting modes.
- Expand Zoom Web Client coverage and document modes where separate participant
  tracks are unavailable.
- Keep platform adapters resilient to DOM and WebRTC changes.
- Grow reproducible fixtures for roster, mute, track, lifecycle, and disconnect
  behavior.

### Quality and accessibility

- Complete keyboard and screen-reader audits of setup, live meetings, library,
  tasks, and settings.
- Improve deterministic audio fixtures for VAD, segmentation, overlap, and model
  regression tests.
- Add more stress tests for long and high-participant meetings.
- Continue English and Spanish copy review as the interface evolves.

### Contributor experience

- Keep setup available through versioned `just` recipes.
- Maintain focused `good first issue` and `help wanted` queues.
- Document stable capture contracts and privacy boundaries near their code.
- Publish architecture decisions when a change affects integrations or data flow.

## Proposing roadmap work

Start with a [GitHub Discussion](https://github.com/igarrux/kuali/discussions) for
large design changes. A proposal should explain the user problem, affected
platforms, privacy and consent impact, expected tests, and what existing
behavior must remain compatible.

Small fixes and clearly bounded improvements can start directly from an issue.
See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.
