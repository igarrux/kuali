import assert from "node:assert/strict";
import test from "node:test";

import { encodeAudio, encodeMeetingEvent, mapFrameChannel } from "../src/protocol.js";
import { healthUrl, isKualiHealthMessage } from "../src/health.js";
import { meetingPresence } from "../src/lifecycle.js";
import "../src/capture-policy.js";

const capturePolicy = globalThis.KualiCapturePolicy;

test("audio frames match capture.v1 little-endian layout", () => {
  const encoded = encodeAudio(42, 1_718_000_000_456, [-0.5, 0.25]);
  const view = new DataView(encoded);
  assert.equal(view.getUint32(0, true), 42);
  assert.equal(view.getFloat64(4, true), 1_718_000_000_456);
  assert.deepEqual([...new Float32Array(encoded, 12)], [-0.5, 0.25]);
});

test("the health handshake only accepts Kuali on the configured loopback port", () => {
  assert.equal(healthUrl(9099), "ws://127.0.0.1:9099/health?client=kuali-extension");
  assert.equal(isKualiHealthMessage(JSON.stringify({
    type: "health",
    service: "kuali",
    status: "ready",
    protocol: "capture.v1",
  })), true);
  assert.equal(isKualiHealthMessage('{"type":"ready"}'), false);
  assert.throws(() => healthUrl(0), RangeError);
});

test("participant metadata stays on the same meeting-event contract", () => {
  const encoded = encodeMeetingEvent({
    kind: "participant-upsert",
    ts: 123,
    speaker: "Ana",
    detail: {
      channel: 7,
      participantId: "teams-user-42",
      displayName: "Ana",
      avatarUrl: "https://example.test/ana.jpg",
      audioKind: "separate",
    },
  });
  assert.deepEqual(JSON.parse(encoded), {
    kind: "participant-upsert",
    ts: 123,
    speaker: "Ana",
    text: null,
    detail: {
      channel: 7,
      participantId: "teams-user-42",
      displayName: "Ana",
      avatarUrl: "https://example.test/ana.jpg",
      audioKind: "separate",
    },
  });
});

test("channels from different frames cannot collide", () => {
  assert.equal(mapFrameChannel(0, 1000), 1000);
  assert.equal(mapFrameChannel(1, 1), 2049);
  assert.equal(mapFrameChannel(2, 1), 4097);
  assert.notEqual(mapFrameChannel(1, 7), mapFrameChannel(2, 7));
  assert.throws(() => mapFrameChannel(0, 2048), RangeError);
});

test("the local microphone can never be rebound by active-speaker voting", () => {
  assert.equal(capturePolicy.MIC_CHANNEL, 1000);
  assert.equal(capturePolicy.shouldCorrelateIdentity(1000, null), false);
  assert.equal(
    capturePolicy.shouldCorrelateIdentity(1000, {
      id: "google_meet:self",
      name: "Tú",
      isSelf: true,
    }),
    false,
  );
});

test("direct remote identities stay stable while unknown tracks may be correlated", () => {
  assert.equal(
    capturePolicy.shouldCorrelateIdentity(4, {
      id: "spaces/meeting/devices/42",
      name: "Ana",
      isSelf: false,
    }),
    false,
  );
  assert.equal(capturePolicy.shouldCorrelateIdentity(4, null), true);
  assert.deepEqual(capturePolicy.connectedTrackDetail(4, "google_meet"), {
    channel: 4,
    audioKind: "separate",
    platform: "google_meet",
    identityPending: true,
  });
});

test("Meet media elements never assign a remote lane from DOM co-location", () => {
  const accidentalSelf = {
    id: "google_meet:self",
    name: "Tú",
    isSelf: true,
  };
  assert.equal(capturePolicy.mediaElementIdentity("google_meet", accidentalSelf), null);
  assert.equal(capturePolicy.mediaElementIdentity("zoom", accidentalSelf), accidentalSelf);
});

test("the roster count deduplicates Meet DOM copies instead of counting transport lanes", () => {
  const detail = capturePolicy.rosterDetail([
    { identity: { id: "self", name: "Garrux", isSelf: true } },
    { identity: { id: "self", name: "Garrux", isSelf: true } },
    { identity: { id: "phone", name: "Delphys JG", isSelf: false } },
  ], "google_meet");
  assert.equal(detail.participantCount, 2);
  assert.deepEqual(detail.participants.map((person) => person.displayName), ["Garrux", "Delphys JG"]);
});

test("Meet protocol names survive background-tab DOM placeholders", () => {
  const identity = capturePolicy.meetParticipantIdentity({
    deviceId: "spaces/meeting/devices/42",
    source: "9981",
    user: {
      deviceId: "spaces/meeting/devices/42",
      displayName: "Ana",
      fullName: "Ana Example",
      profilePicture: "https://example.test/ana.jpg",
      isCurrentUser: false,
    },
    domIdentity: {
      id: "spaces/meeting/devices/42",
      name: "Devices",
      avatarUrl: null,
      isSelf: false,
    },
  });
  assert.equal(identity.name, "Ana");
  assert.equal(identity.avatarUrl, "https://example.test/ana.jpg");
  assert.equal(capturePolicy.usableMeetParticipantName("devices"), "");
  assert.equal(capturePolicy.usableMeetParticipantName("Participante"), "");
});

test("Meet remembers valid names by device ID across partial background updates", () => {
  const learned = capturePolicy.mergeMeetParticipantIdentity(null, {
    id: "devices/remote",
    name: "Pivel",
    avatarUrl: "https://example.test/pivel.jpg",
    isSelf: false,
  });
  const backgroundUpdate = capturePolicy.mergeMeetParticipantIdentity(learned, {
    id: "devices/remote",
    name: "Participante",
    avatarUrl: null,
    isSelf: false,
  });
  assert.equal(backgroundUpdate.name, "Pivel");
  assert.equal(backgroundUpdate.avatarUrl, "https://example.test/pivel.jpg");

  const renamed = capturePolicy.mergeMeetParticipantIdentity(backgroundUpdate, {
    id: "devices/remote",
    name: "Pivel JG",
  });
  assert.equal(renamed.name, "Pivel JG");

  const unrelated = capturePolicy.mergeMeetParticipantIdentity(null, {
    id: "devices/other",
    name: "Participante",
  });
  assert.equal(unrelated.name, "Participante sin identificar");
});

test("Meet roster includes active protocol users hidden by tile virtualization", () => {
  const merged = capturePolicy.mergeMeetRoster([
    { identity: { id: "devices/self", name: "Garrux", isSelf: true } },
  ], [
    {
      deviceId: "devices/self",
      displayName: "Garrux",
      status: 1,
      parentDeviceId: null,
      isCurrentUser: true,
    },
    {
      deviceId: "devices/remote",
      displayName: "Pivel",
      status: 1,
      parentDeviceId: null,
      isCurrentUser: false,
    },
    {
      deviceId: "devices/screen",
      displayName: "Pivel's screen",
      status: 1,
      parentDeviceId: "devices/remote",
      isCurrentUser: false,
    },
    {
      deviceId: "devices/left",
      displayName: "Already left",
      status: 6,
      parentDeviceId: null,
      isCurrentUser: false,
    },
  ]);
  assert.deepEqual(merged.map(({ identity }) => identity.name), ["Garrux", "Pivel"]);
  assert.equal(capturePolicy.rosterDetail(merged, "google_meet").participantCount, 2);
});

test("participants with the same display name remain distinct by Meet device ID", () => {
  const merged = capturePolicy.mergeMeetRoster([
    { identity: { id: "devices/phone", name: "Garrux", isSelf: true } },
    { identity: { id: "devices/mac", name: "Garrux", isSelf: true } },
  ], [
    { deviceId: "devices/phone", displayName: "Garrux", status: 1, isCurrentUser: false },
    { deviceId: "devices/mac", displayName: "Garrux", status: 1, isCurrentUser: true },
  ]);
  const detail = capturePolicy.rosterDetail(merged, "google_meet");
  assert.equal(detail.participantCount, 2);
  assert.deepEqual(detail.participants.map((person) => person.displayName), ["Garrux", "Garrux"]);
  assert.deepEqual(detail.participants.map((person) => person.isSelf), [false, true]);
  assert.notEqual(detail.participants[0].participantId, detail.participants[1].participantId);
});

test("Meet microphone capture follows the current device disabled state", () => {
  const users = [
    { deviceId: "self-device", isCurrentUser: true },
    { deviceId: "remote-device", isCurrentUser: false },
  ];
  assert.equal(capturePolicy.localMeetAudioDisabled(users, []), null);
  assert.equal(capturePolicy.localMeetAudioDisabled(users, [
    { deviceId: "remote-device", outputType: 1, disabled: true },
  ]), null);
  assert.equal(capturePolicy.localMeetAudioDisabled(users, [
    { deviceId: "self-device", outputType: 1, disabled: true },
  ]), true);
  assert.equal(capturePolicy.localMeetAudioDisabled(users, [
    { deviceId: "self-device", outputType: 1, disabled: false },
  ]), false);
});

test("Meet microphone controls prefer data-is-muted over ambiguous labels", () => {
  assert.equal(capturePolicy.meetMicrophoneMuted({
    mutedAttribute: "false",
    label: "Micrófono",
  }), false);
  assert.equal(capturePolicy.meetMicrophoneMuted({
    mutedAttribute: "true",
    label: "Micrófono",
  }), true);
  assert.equal(capturePolicy.meetMicrophoneMuted({ label: "Desactivar micrófono (⌘ + d)" }), false);
  assert.equal(capturePolicy.meetMicrophoneMuted({ label: "Activar micrófono (⌘ + d)" }), true);
  assert.equal(capturePolicy.meetMicrophoneMuted({ label: "Turn off microphone" }), false);
  assert.equal(capturePolicy.meetMicrophoneMuted({ label: "Turn on microphone" }), true);
});

test("only a visible unmuted Meet control opens the local audio gate", () => {
  const decide = capturePolicy.shouldSendMeetMicrophone;
  assert.equal(decide({
    trackEnabled: true,
    controlMuted: false,
  }), true);
  assert.equal(decide({
    trackEnabled: true,
    controlMuted: true,
  }), false);
  assert.equal(decide({
    trackEnabled: true,
    controlMuted: null,
  }), false);
  assert.equal(decide({
    trackEnabled: false,
    controlMuted: false,
  }), false);
});

test("capture ends only after the top Meet roster has seen self disappear", () => {
  assert.equal(meetingPresence(2, true, []), null, "a child frame is not authoritative");
  assert.deepEqual(meetingPresence(0, false, []), {
    selfPresent: false,
    hadSelf: false,
    shouldScheduleStop: false,
  });
  assert.deepEqual(meetingPresence(0, false, [{ isSelf: true }]), {
    selfPresent: true,
    hadSelf: true,
    shouldScheduleStop: false,
  });
  assert.deepEqual(meetingPresence(0, true, [{ isSelf: false }]), {
    selfPresent: false,
    hadSelf: true,
    shouldScheduleStop: true,
  });
});
