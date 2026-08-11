# Reviewer testing instructions

Kuali is an open-source extension with a required desktop companion. It does
not use a Kuali cloud account and no reviewer credentials are required.

1. Download and open the current public desktop release from
   `https://kuali.garrux.dev/`.
2. Keep the default local browser-bridge port (`9099`) in Kuali.
3. Join a meeting you are authorized to record in Google Meet and remain on the
   meeting page. Google Meet is the stable browser integration; Microsoft Teams
   and Zoom currently have experimental, partial support.
4. Select the Kuali toolbar icon. It should identify the platform and say that
   Kuali is available.
5. Review the disclosure, select the participant-notice checkbox, and choose
   **Record and transcribe**.
6. Speak in the meeting. The page shows a persistent Kuali recording indicator,
   the toolbar badge shows `REC`, and the desktop app displays the live
   participant/transcript state.
7. Select **Stop**, leave the meeting, or end it. Capture stops, the indicator
   disappears, and the desktop app finalizes the local meeting record.

The extension sends data only to `ws://127.0.0.1:<configured-port>`. Whisper
transcription runs in the desktop app. The extension contains no remote code,
analytics, advertising, account system, or Kuali-operated cloud service. It
does not retain captured audio as a file.

For code review and reproducible tests, see:
`https://github.com/igarrux/kuali/tree/main/browser-extension`.
