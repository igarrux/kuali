# Kuali Privacy Policy

**Effective date:** August 8, 2026
**Español:** [PRIVACY.es.md](PRIVACY.es.md)

Kuali is an open-source, self-hosted meeting transcription application. This
policy covers the Kuali desktop application, Discord bot, and Kuali browser
extension distributed from this repository.

## The short version

Kuali does not operate an account service, advertising network, analytics
service, or cloud transcription backend. The browser extension sends meeting
audio and participant information only to the Kuali application running on
your own computer. Whisper transcribes that audio locally. Kuali does not sell
personal information and does not retain the captured audio as an audio file.

If you configure a summary provider, Discord delivery, or a webhook, the
desktop application sends the data described below to that service at your
request. Those services have their own privacy policies.

## Information Kuali processes

When you choose to record and transcribe a supported browser meeting, Kuali may
process:

- live meeting audio;
- the meeting platform, meeting identifier, page title, and start/end times;
- participant display names, usernames, platform identifiers, avatar URLs,
  mute state, speaking activity, and whether an audio channel is separate or
  mixed; and
- the resulting speaker-attributed transcript, confidence values, summary,
  key points, decisions, open questions, and action items.

For Discord calls, the desktop application may process the equivalent server,
channel, participant, voice, and message-delivery information required to join
the call, attribute speakers, announce recording, and deliver results.

Kuali also stores settings you enter, which may include a Discord bot token,
Discord username or user ID, model location, special vocabulary, summary
provider settings and API keys, and webhook URLs and signing secrets. The
browser extension itself stores only the local port used to find the desktop
application.

## How information is used

Kuali uses this information only to:

- capture a meeting you explicitly start;
- show live participants and transcription;
- create and search a local meeting record;
- generate a title, summary, decisions, questions, and action items;
- deliver results to Discord or user-configured webhooks; and
- diagnose capture status and keep simultaneous meetings separate.

Kuali does not use meeting data for advertising, credit decisions, profiling,
or training a Kuali-operated machine-learning model.

## Where information goes

Browser meeting data travels from the meeting page to the extension and then
over a loopback WebSocket (`127.0.0.1`) to the Kuali desktop application on the
same computer. The local service rejects ordinary website origins. The
extension does not send this data to a Kuali-operated server.

Whisper transcription runs locally. Depending on settings you choose:

- a configured OpenAI, Anthropic, Gemini, OpenAI-compatible, or command-line
  summary provider receives meeting metadata, participant display names, and
  the transcript needed to generate the summary;
- Discord receives notices, summaries, tasks, and transcripts you request the
  bot to post; and
- each enabled webhook receives the completed-meeting payload, including
  meeting/channel metadata, participants, transcript, summary, and tasks.

Kuali does not enable these destinations on your behalf. Review the privacy
terms of each service before configuring it.

## Storage, retention, and deletion

The desktop application stores meeting records in the operating system's Kuali
application-data directory and settings in its Kuali configuration directory.
Downloaded speech-model weights are stored in `~/.kuali` by default or in the
folder you select. On Unix systems, Kuali writes its configuration file with
owner-only (`0600`) permissions; secrets are not additionally encrypted at
rest.

Meeting records remain until you delete them in Kuali or remove their files.
Model weights remain until you delete them in Kuali. Removing the extension
deletes its browser-managed local-port preference according to the browser's
normal extension-data behavior. Data already delivered to Discord, a summary
provider, or a webhook is controlled by that service and must be deleted there.

Captured PCM audio is processed in memory and is not retained as an audio file
by Kuali. Temporary in-memory audio disappears when processing ends or the
application closes.

## Recording notice and consent

Kuali requires the person starting browser capture to confirm that participants
were informed and that they have permission to record and transcribe. It also
shows a persistent recording indicator. Laws and platform rules vary by
location; the person operating Kuali is responsible for obtaining every
required consent.

## Security

Kuali limits the browser bridge to the loopback interface, restricts browser
origins to Chrome extensions, requests access only to supported meeting sites,
and ships all extension code in its package. Because Kuali is software running
on your computer, anyone who can access your account, files, configured tokens,
or enabled destinations may be able to access meeting information.

## Chrome Web Store Limited Use

Kuali's use and transfer of information received from Google services complies
with the Chrome Web Store User Data Policy, including its Limited Use
requirements. Kuali uses data from Google Meet only to provide the
user-visible capture, transcription, meeting library, summary, and delivery
features described in this policy.

## Changes and contact

Material changes will be documented in this file and its effective date will be
updated. Questions, privacy requests, and security reports can be opened at
<https://github.com/igarrux/kuali/issues>. Do not include meeting content,
tokens, API keys, or other secrets in a public issue.
