# English store listing

## Name

Kuali

## Summary

Capture Meet, Teams, and Zoom audio for private transcription in the local Kuali desktop app.

## Detailed description

Kuali turns browser meetings into speaker-aware, searchable notes without
adding a recording bot to the call.

Start capture from the toolbar, review the data disclosure, and confirm that
participants have been informed. Kuali then sends meeting audio directly to
the companion desktop app on the same computer. Whisper transcribes speech
locally and Kuali shows a persistent recording indicator until capture stops.

Features:

- live transcription in the Kuali desktop app;
- participant names and separate speaker channels when the meeting platform
  exposes them;
- simultaneous capture from multiple supported meeting tabs;
- automatic stop when you leave or the meeting ends;
- local meeting history, search, summaries, decisions, open questions, and
  action items; and
- optional user-configured AI providers and webhooks.

The free, open-source Kuali desktop app is required. The extension connects
only to Kuali on `127.0.0.1`; it has no Kuali-operated cloud backend and does
not retain captured audio as an audio file. Transcripts leave the computer only
when you configure a summary provider, Discord delivery, or a webhook.

Google Meet, Microsoft Teams, and Zoom may change their private browser
interfaces. Speaker separation is best-effort, and Kuali labels unavoidable
mixed audio instead of assigning it to the wrong person.

Always tell participants and obtain any consent required in your location.
Kuali is not affiliated with Google, Microsoft, or Zoom.

Source, desktop downloads, documentation, and privacy policy:
https://github.com/igarrux/kuali
