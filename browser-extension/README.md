# Kuali browser extension

**English** · [Español](README.es.md)

This unpacked Chrome/Chromium extension connects browser meetings to the Kuali
desktop app. Google Meet support is stable. Microsoft Teams and Zoom support is
experimental and partial while those integrations receive broader real-world
testing. You remain a normal participant in the meeting; no external bot
account joins it.

| Platform | Status |
|---|---|
| Google Meet | Stable |
| Microsoft Teams | Experimental · partial support |
| Zoom | Experimental · partial support |

## Install

Install Kuali from its official
[Chrome Web Store listing](https://chromewebstore.google.com/detail/kuali/cgojkmdggflcggedmapamcmkelgaahhp).
The same listing works in Chrome, Edge, Brave, and Arc.

## Install for development

1. Open `chrome://extensions` (or `edge://extensions`).
2. Enable **Developer mode**.
3. Select **Load unpacked** and choose this `browser-extension` directory.
4. Open Kuali, join a supported meeting, select the Kuali toolbar icon, and
   review the disclosure. Confirm that participants were informed, then choose
   **Record and transcribe**.

The extension connects only to `ws://127.0.0.1:9099` by default. The port can
be changed in the popup. Audio is sent as mono 16 kHz PCM; participant metadata
is sent on the same WebSocket before or alongside its channel's audio.
Multiple supported meeting tabs can capture concurrently. Each tab gets its own
session and transcript; the desktop app shares one loaded Whisper model between
them and any active Discord call.

## Real Google Meet E2E

Run the interactive end-to-end test from this directory:

```sh
npm run test:e2e:meet
```

The first run downloads the official Chrome for Testing build into
`target/e2e`, starts the real Kuali engine behind a transparent wire probe, and
opens Meet with this extension in a disposable browser profile. It does not
change the normal Chrome profile, contact summary providers, deliver webhooks,
or retain the test meeting in Kuali's library.

Follow the instructions printed in the terminal. Full mode needs a second
participant (another profile or a phone) so it can verify remote track
separation, platform ID, name, avatar, local microphone, live transcription,
final attribution, clean shutdown, and Whisper being released from memory.
Use `npm run test:e2e:meet -- --solo` for the smaller one-person smoke test.

The machine-readable report is written to `target/e2e/reports`. Chrome for
Testing is used because current branded Chrome releases do not allow automation
to sideload an unpacked extension through the old command-line flag.

## Speaker separation contract

Every audio frame references a stable numeric channel. A `participant-upsert`
event binds that channel to the platform participant ID, display name, avatar,
platform, and an `audioKind` of either `separate` or `mixed`.

- Meet: observes the receiver's encoded Opus frames before Meet consumes them,
  decodes a copy with WebCodecs, and routes every frame by its contributing
  source (CSRC). Meet's three virtual transport lanes may change owners, while
  Kuali's participant channels remain stable. Identity comes from Meet's
  participant collection when available or from a one-time, confidence-gated
  CSRC/activity correlation; later audio never follows the visual glow.
- Teams (experimental): captures individual WebRTC tracks when Teams exposes
  them and maps voice-outline activity to the corresponding participant.
- Zoom (experimental): captures separate WebRTC tracks when the active Zoom
  mode exposes them.
  If none appear, an offscreen `tabCapture` fallback preserves the meeting as
  one `mixed` channel. It is discarded as soon as an individual page track is
  proven, so the two paths never intentionally transcribe the same audio.

Speaker mapping is deliberately conservative. A source stays unknown during
overlap and is named only after sustained decoded speech agrees with exactly
one non-local active tile. Name and avatar selectors are necessarily
best-effort because all three sites can change their private DOM.

## Capacity

Meet currently sends three virtual audio streams containing the most relevant
speakers. Kuali can retain stable identities for a larger roster as people take
turns and can preserve three simultaneous remote speakers, but it cannot obtain
a fourth concurrent source that Meet did not send to the browser. Silent audio
is gated before crossing the extension boundary. The mixed fallback for other
platforms lives in a dedicated offscreen document and replays the captured tab
because Chrome otherwise mutes it. Actual capacity still depends on the browser,
machine, meeting layout, and platform behavior; validate a real 30-person call
before treating it as a guarantee.

## Chrome Web Store release

Run `npm run package:store` to create the minimal upload ZIP and SHA-256 under
`dist`. The descriptions, privacy declarations, reviewer instructions, promo
graphics, and dashboard checklist live in [`store`](store/README.md). Manual
unpacked installation remains supported for development and as a fallback.

## Privacy

Capture requires an affirmative participant-notice confirmation and remains
visible through an in-page indicator and `REC` toolbar badge. Audio goes only
to the Kuali app on `127.0.0.1`; the extension retains no audio file. Read the
complete [privacy policy](../PRIVACY.md).

## License and provenance

This directory is Apache-2.0 licensed. It is intentionally isolated from the
MIT-licensed desktop workspace. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
The wire format and parts of the capture approach were adapted from Vexa at the
exact revision recorded in `NOTICE`; modified code carries Kuali naming and no
Vexa trademark assets are distributed.
