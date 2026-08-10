# Chrome Web Store privacy answers

These answers describe version `0.1.4`.

## Single purpose

> Capture audio and participant information from a user-selected Google Meet,
> Microsoft Teams, or Zoom meeting and send it to the Kuali desktop application
> on the same computer for live transcription and meeting notes.

## Permission justifications

### `storage`

> Stores only the user-selected loopback port used to connect to the local Kuali
> desktop application. No meeting audio or transcript is stored in extension
> storage.

### `tabCapture`

> Captures the current meeting tab's mixed audio only when Zoom or Microsoft
> Teams does not expose individual WebRTC audio tracks. It is started by an
> explicit user action and stopped when capture or the meeting ends.

### `offscreen`

> Runs the user-authorized mixed tab-audio fallback in Manifest V3 and replays
> the captured tab audio so the user can continue hearing the meeting. It is
> used only while a supported meeting is actively being captured.

### Host permissions: Google Meet, Microsoft Teams, and Zoom

> Injects the capture and participant-mapping code only into supported meeting
> pages so Kuali can identify the meeting, observe its roster and audio tracks,
> show consent/recording controls, and stop when the participant leaves.

### Host permission: `ws://127.0.0.1/*`

> Connects the extension to the Kuali desktop application on the same computer
> for health checks and transmission of live audio and meeting metadata. The
> app listens only on loopback and rejects ordinary website origins.

## Remote code

**Does the extension use remote code?** No.

> All executable JavaScript and the audio worklet are included in the extension
> package. The extension does not download scripts, use `eval`, execute remote
> WebAssembly, or treat network responses as code.

## User-data categories

Declare these categories:

- **Personally identifiable information:** participant display names,
  usernames, platform identifiers, and avatar URLs.
- **Personal communications:** live meeting audio and its transcript.
- **User activity:** mute state and speaking activity used to attribute audio.
- **Website content:** meeting identifier and meeting-page title.

Do not select financial, health, authentication, location, or web-history
categories. Kuali's own Discord/API tokens are entered in the separate desktop
app and are not collected by this extension.

## Data-use certifications

Certify all of the following truthfully:

- data is used only to provide the extension's single purpose;
- data is not sold to third parties;
- data is not used or transferred for purposes unrelated to the single purpose;
- data is not used or transferred to determine creditworthiness or for lending;
- humans do not read the data except with the user's affirmative agreement,
  for security/abuse, to comply with law, or as part of an allowed internal
  operation; and
- use of Google-service data complies with the Chrome Web Store User Data
  Policy, including Limited Use.

## Disclosure surfaces

Before every capture, both the toolbar popup and the in-page suggestion disclose
audio, participant identity, local processing, and optional configured
destinations. Capture remains disabled until the user confirms participant
notice/permission. During capture, the page shows a persistent indicator, the
toolbar badge reads `REC`, and both provide a stop control.
