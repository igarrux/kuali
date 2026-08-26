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

test("remote Meet audio survives mute metadata while self audio stays isolated", () => {
  const decide = capturePolicy.shouldSendMeetRemoteAudio;
  assert.equal(decide({ disabled: false, isSelf: false }), true);
  assert.equal(decide({ disabled: true, isSelf: false }), true);
  assert.equal(decide({ disabled: false, isSelf: false }), true);
  assert.equal(decide({ disabled: false, isSelf: true }), false);
  assert.equal(decide({ disabled: true, isSelf: true }), false);
});

test("Meet remote audio health resets only after sustained audible RTP without PCM", () => {
  let health = null;
  let encodedFrames = 0;
  let decodedFrames = 0;
  for (let now = 0; now <= 4_000; now += 250) {
    encodedFrames += 1;
    decodedFrames += 1;
    health = capturePolicy.nextMeetRemoteAudioHealth(health, {
      now,
      source: "remote/42",
      audioLevel: 0.08,
      encodedFrames,
      decodedFrames,
      pcmFrames: 0,
    });
  }
  assert.equal(health.stalled, true);

  health = capturePolicy.nextMeetRemoteAudioHealth(health, {
    now: 4_000,
    source: "remote/42",
    audioLevel: 0.08,
    encodedFrames,
    decodedFrames,
    pcmFrames: 0,
    recovering: true,
  });
  assert.equal(health.stalled, false);

  for (let now = 4_250; now < 12_000; now += 250) {
    encodedFrames += 1;
    decodedFrames += 1;
    health = capturePolicy.nextMeetRemoteAudioHealth(health, {
      now,
      source: "remote/42",
      audioLevel: 0.08,
      encodedFrames,
      decodedFrames,
      pcmFrames: 0,
    });
    assert.equal(health.stalled, false, `recovery cooldown at ${now}ms`);
  }
  encodedFrames += 1;
  decodedFrames += 1;
  health = capturePolicy.nextMeetRemoteAudioHealth(health, {
    now: 12_000,
    source: "remote/42",
    audioLevel: 0.08,
    encodedFrames,
    decodedFrames,
    pcmFrames: 0,
  });
  assert.equal(health.stalled, true);
});

test("Meet remote audio health treats silence and advancing PCM as healthy", () => {
  let silent = null;
  for (let now = 0; now <= 10_000; now += 500) {
    silent = capturePolicy.nextMeetRemoteAudioHealth(silent, {
      now,
      source: "remote/silent",
      audioLevel: 0,
      encodedFrames: now / 500 + 1,
      decodedFrames: now / 500 + 1,
      pcmFrames: 0,
    });
  }
  assert.equal(silent.stalled, false);

  let flowing = null;
  for (let now = 0; now <= 10_000; now += 500) {
    flowing = capturePolicy.nextMeetRemoteAudioHealth(flowing, {
      now,
      source: "remote/flowing",
      audioLevel: 0.08,
      encodedFrames: now / 500 + 1,
      decodedFrames: now / 500 + 1,
      pcmFrames: now / 500 + 1,
    });
    assert.equal(flowing.stalled, false);
  }
});

test("Meet remote audio health restarts after source handoffs, quiet gaps, and sub-threshold noise", () => {
  let sourceHealth = null;
  let encodedFrames = 0;
  let decodedFrames = 0;
  for (let now = 0; now <= 3_750; now += 250) {
    encodedFrames += 1;
    decodedFrames += 1;
    sourceHealth = capturePolicy.nextMeetRemoteAudioHealth(sourceHealth, {
      now,
      source: "remote/a",
      audioLevel: 0.08,
      encodedFrames,
      decodedFrames,
      pcmFrames: 0,
    });
  }
  assert.equal(sourceHealth.stalled, false);

  encodedFrames += 1;
  decodedFrames += 1;
  sourceHealth = capturePolicy.nextMeetRemoteAudioHealth(sourceHealth, {
    now: 4_000,
    source: "remote/b",
    audioLevel: 0.08,
    encodedFrames,
    decodedFrames,
    pcmFrames: 0,
  });
  assert.equal(sourceHealth.audibleSince, 4_000);
  assert.equal(sourceHealth.stalled, false);

  encodedFrames += 1;
  decodedFrames += 1;
  sourceHealth = capturePolicy.nextMeetRemoteAudioHealth(sourceHealth, {
    now: 5_000,
    source: "remote/b",
    audioLevel: 0,
    encodedFrames,
    decodedFrames,
    pcmFrames: 0,
  });
  assert.equal(sourceHealth.audibleSince, null);

  encodedFrames += 1;
  decodedFrames += 1;
  sourceHealth = capturePolicy.nextMeetRemoteAudioHealth(sourceHealth, {
    now: 5_250,
    source: "remote/b",
    audioLevel: 0.08,
    encodedFrames,
    decodedFrames,
    pcmFrames: 0,
  });
  assert.equal(sourceHealth.audibleSince, 5_250);
  assert.equal(sourceHealth.stalled, false);

  let noiseHealth = null;
  for (let now = 0; now <= 10_000; now += 500) {
    noiseHealth = capturePolicy.nextMeetRemoteAudioHealth(noiseHealth, {
      now,
      source: "remote/noise",
      audioLevel: 0.009,
      encodedFrames: now / 500 + 1,
      decodedFrames: now / 500 + 1,
      pcmFrames: 0,
    });
  }
  assert.equal(noiseHealth.stalled, false);
});

function redHeader(payloadType, length) {
  return [0x80 | payloadType, 0, (length >> 8) & 0x03, length & 0xff];
}

test("Meet extracts negotiated Opus payloads from RED without fixed payload types", () => {
  const codecs = [
    { payloadType: 118, mimeType: "audio/red" },
    { payloadType: 109, mimeType: "audio/opus" },
  ];
  const red = Uint8Array.from([
    ...redHeader(109, 2),
    109,
    0xaa, 0xbb,
    1, 2, 3, 4,
  ]);
  assert.deepEqual(
    [...capturePolicy.meetPrimaryOpusPayload(red, { payloadType: 118 }, codecs)],
    [1, 2, 3, 4],
  );
  assert.deepEqual(
    [...capturePolicy.meetPrimaryOpusPayload(red, { mimeType: "audio/red" }, codecs)],
    [1, 2, 3, 4],
  );

  const direct = Uint8Array.from([7, 8, 9]);
  assert.equal(
    capturePolicy.meetPrimaryOpusPayload(
      direct,
      { payloadType: 109 },
      codecs,
    ),
    direct,
  );
});

test("Meet RED parsing handles multiple redundant blocks and rejects unsafe payloads", () => {
  const codecs = [
    { payloadType: 63, mimeType: "audio/red" },
    { payloadType: 111, mimeType: "audio/opus" },
    { payloadType: 0, mimeType: "audio/PCMU" },
  ];
  const red = Uint8Array.from([
    ...redHeader(111, 1),
    ...redHeader(111, 2),
    111,
    0xaa,
    0xbb, 0xcc,
    5, 6, 7,
  ]);
  assert.deepEqual(
    [...capturePolicy.meetPrimaryOpusPayload(red, { payloadType: 63 }, codecs)],
    [5, 6, 7],
  );

  const truncated = Uint8Array.from([...redHeader(111, 20), 111, 1, 2]);
  assert.equal(
    capturePolicy.meetPrimaryOpusPayload(truncated, { payloadType: 63 }, codecs),
    null,
  );
  assert.equal(
    capturePolicy.meetPrimaryOpusPayload(Uint8Array.from([0, 1, 2]), { payloadType: 63 }, codecs),
    null,
  );

  const legacy = Uint8Array.from([3, 4, 5]);
  assert.equal(capturePolicy.meetPrimaryOpusPayload(legacy), legacy);
});

test("Meet rejects an unknown payload type until renegotiated codecs are refreshed", () => {
  const staleCodecs = [
    { payloadType: 63, mimeType: "audio/red" },
    { payloadType: 111, mimeType: "audio/opus" },
  ];
  const renegotiatedCodecs = [
    { payloadType: 118, mimeType: "audio/red" },
    { payloadType: 109, mimeType: "audio/opus" },
  ];
  const red = Uint8Array.from([109, 7, 8, 9]);

  assert.equal(
    capturePolicy.meetPrimaryOpusPayload(red, { payloadType: 118 }, staleCodecs),
    null,
  );
  assert.deepEqual(
    [...capturePolicy.meetPrimaryOpusPayload(red, { payloadType: 118 }, renegotiatedCodecs)],
    [7, 8, 9],
  );
});

test("sustained Meet activity can replace a stale protocol route owner", () => {
  let vote = null;
  for (const now of [0, 60, 120, 180, 240, 300]) {
    vote = capturePolicy.nextMeetIdentityVote(vote, "devices/luis", now);
  }
  assert.equal(capturePolicy.meetIdentityVoteReady(vote), true);

  const corrected = capturePolicy.resolveMeetRouteIdentity({
    protocolRawDeviceId: "raw/vivetix",
    protocolDeviceId: "devices/vivetix",
    currentDeviceId: "devices/vivetix",
    activityDeviceId: "devices/luis",
    overriddenProtocolRawDeviceId: "raw/vivetix",
  });
  assert.deepEqual(corrected, {
    deviceId: "devices/luis",
    activityDeviceId: "devices/luis",
    overriddenProtocolRawDeviceId: "raw/vivetix",
  });

  assert.deepEqual(capturePolicy.resolveMeetRouteIdentity({
    ...corrected,
    protocolRawDeviceId: "raw/pedro",
    protocolDeviceId: "devices/pedro",
    currentDeviceId: corrected.deviceId,
  }), {
    deviceId: "devices/pedro",
    activityDeviceId: "",
    overriddenProtocolRawDeviceId: "",
  });
});

test("Meet parent canonicalization does not erase an activity-confirmed owner", () => {
  assert.deepEqual(capturePolicy.resolveMeetRouteIdentity({
    protocolRawDeviceId: "devices/presentation",
    protocolDeviceId: "devices/parent-after-roster-refresh",
    currentDeviceId: "devices/luis",
    activityDeviceId: "devices/luis",
    overriddenProtocolRawDeviceId: "devices/presentation",
  }), {
    deviceId: "devices/luis",
    activityDeviceId: "devices/luis",
    overriddenProtocolRawDeviceId: "devices/presentation",
  });
});

test("an activity-learned Meet owner persists until protocol evidence arrives", () => {
  assert.deepEqual(capturePolicy.resolveMeetRouteIdentity({
    currentDeviceId: "devices/luis",
    activityDeviceId: "devices/luis",
  }), {
    deviceId: "devices/luis",
    activityDeviceId: "devices/luis",
    overriddenProtocolRawDeviceId: "",
  });
  assert.deepEqual(capturePolicy.resolveMeetRouteIdentity({
    protocolRawDeviceId: "raw/pedro",
    protocolDeviceId: "devices/pedro",
    currentDeviceId: "devices/luis",
    activityDeviceId: "devices/luis",
  }), {
    deviceId: "devices/pedro",
    activityDeviceId: "",
    overriddenProtocolRawDeviceId: "",
  });
});

test("Meet identity votes reset after a gap or a different active participant", () => {
  const first = capturePolicy.nextMeetIdentityVote(null, "devices/luis", 0);
  const gap = capturePolicy.nextMeetIdentityVote(first, "devices/luis", 401);
  assert.equal(gap.samples, 1);
  const switched = capturePolicy.nextMeetIdentityVote(gap, "devices/gabriel", 420);
  assert.deepEqual(switched, {
    deviceId: "devices/gabriel",
    samples: 1,
    firstAt: 420,
    lastAt: 420,
  });
  assert.equal(capturePolicy.meetIdentityVoteReady(switched), false);
});

test("Meet identity votes survive quiet frames but expire after the maximum gap", () => {
  const vote = capturePolicy.nextMeetIdentityVote(null, "devices/luis", 100);
  assert.equal(capturePolicy.meetIdentityVoteFresh(vote, 499), true);
  assert.equal(capturePolicy.meetIdentityVoteFresh(vote, 501), false);
});

test("Meet source selection ignores loud CSRCs left over from the ten-second window", () => {
  const selected = capturePolicy.selectMeetContributingSource([
    { source: 165, timestamp: 1_000, audioLevel: 0.9 },
    { source: 160, timestamp: 10_000, audioLevel: 0.02 },
  ]);
  assert.equal(selected.source, 160);

  assert.equal(capturePolicy.selectMeetContributingSource([
    { source: 165, timestamp: 10_000, audioLevel: 0.03 },
    { source: 160, timestamp: 9_990, audioLevel: 0.025 },
  ]), null);
  assert.equal(capturePolicy.selectMeetContributingSource([
    { source: 165, timestamp: 10_000, audioLevel: 0.01 },
    { source: 160, timestamp: 9_990, audioLevel: 0.04 },
  ]).source, 160);
  assert.equal(capturePolicy.selectMeetContributingSource([
    { source: 165, audioLevel: 0.04 },
    { source: 160, audioLevel: 0.02 },
  ]), null);
});

test("Meet receiver leases suppress overlap but accept a handoff or reclock after a gap", () => {
  const receiverA = {};
  const receiverB = {};
  const first = capturePolicy.updateMeetReceiverLease(null, receiverA, 1_000, 0);
  assert.equal(first.accepted, true);
  assert.equal(
    capturePolicy.updateMeetReceiverLease(first.lease, receiverA, 1_000, 10).accepted,
    false,
  );
  assert.equal(
    capturePolicy.updateMeetReceiverLease(first.lease, receiverB, 10, 20).accepted,
    false,
  );
  const handoff = capturePolicy.updateMeetReceiverLease(first.lease, receiverB, 10, 251);
  assert.equal(handoff.accepted, true);
  const reclock = capturePolicy.updateMeetReceiverLease(handoff.lease, receiverB, 1, 502);
  assert.equal(reclock.accepted, true);
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
