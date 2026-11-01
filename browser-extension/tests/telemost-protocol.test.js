import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import "../src/capture-policy.js";
import "../src/telemost-protocol.js";

const telemost = globalThis.KualiTelemostProtocol;
const capturePolicy = globalThis.KualiCapturePolicy;
const fixture = JSON.parse(readFileSync(new URL("./fixtures/telemost-signalling.json", import.meta.url), "utf8"));
const { VAD_ON_LATENCY_MS, VAD_OFF_LATENCY_MS, VAD_OFF_MARGIN_MS } = telemost;

// Raw WebSocket text, optionally with placeholder
// ids swapped for another vantage point.
function frame(key, index = 0, replacements = {}) {
  let text = JSON.stringify(fixture[key][index].msg);
  for (const [from, to] of Object.entries(replacements)) text = text.split(from).join(to);
  return text;
}

function slots(speaking, extra = []) {
  return JSON.stringify({
    uid: "slots",
    slotsConfig: {
      key: 1,
      slots: [
        { selfView: {}, vad: false },
        ...speaking.map(([participantId, vad]) => ({ participant: { participantId }, vad })),
        ...extra,
      ],
      audioSlots: [],
    },
  });
}

function joinedHost() {
  const state = telemost.createState();
  telemost.ingestOutgoing(state, frame("out.hello"), 1_000);
  telemost.ingestIncoming(state, frame("in.updateDescription.snapshot_on_join_alone"), 1_100);
  return state;
}

test("hello and updateMe identify self and its mute without a server echo", () => {
  const state = telemost.createState();
  const hello = telemost.ingestOutgoing(state, frame("out.hello"), 1_000);
  assert.equal(hello.self, true);
  assert.equal(state.selfId, "<P_HOST>");
  assert.equal(state.selfName, "<NAME_HOST>");
  assert.equal(state.selfMuted, false);
  assert.equal(state.sessionOpen, true);

  assert.equal(telemost.ingestOutgoing(state, frame("out.updateMe.mic_off"), 2_000).self, true);
  assert.equal(state.selfMuted, true);
  assert.equal(telemost.ingestOutgoing(state, frame("out.updateMe.camera_off"), 3_000).self, true);
  assert.equal(state.selfMuted, false, "camera-off updateMe still reports sendAudio:true");
  assert.deepEqual(telemost.ingestOutgoing(state, frame("out.updateMe.camera_off"), 3_100).self, false);

  for (const ignored of ["out.ping", "out.setSlots"]) {
    assert.deepEqual(telemost.ingestOutgoing(state, frame(ignored), 4_000), {
      roster: false, vad: false, self: false, left: [],
    });
  }
  assert.deepEqual(telemost.roster(state), [{
    participantId: "<P_HOST>", displayName: "<NAME_HOST>", avatarUrl: null, isSelf: true, muted: false,
  }]);
});

test("the roster follows the join snapshot, joins, remote mute and leaves", () => {
  const state = telemost.createState();
  telemost.ingestOutgoing(state, frame("out.hello"), 1_000);
  assert.equal(state.rosterKnown, false);
  const snapshot = telemost.ingestIncoming(state, frame("in.updateDescription.snapshot_on_join_alone"), 1_100);
  assert.equal(snapshot.roster, true);
  assert.equal(state.rosterKnown, true);

  const join = telemost.ingestIncoming(state, frame("in.upsertDescription.join"), 2_000);
  assert.equal(join.roster, true);
  telemost.ingestIncoming(state, frame("in.upsertDescription.remote_mic_muted"), 2_100);
  assert.deepEqual(telemost.roster(state).map(({ participantId, displayName, isSelf, muted }) => ({
    participantId, displayName, isSelf, muted,
  })), [
    { participantId: "<P_HOST>", displayName: "<NAME_HOST>", isSelf: true, muted: false },
    { participantId: "<P1>", displayName: "<NAME_P1>", isSelf: false, muted: false },
    { participantId: "<P2>", displayName: "<NAME_P2>", isSelf: false, muted: true },
  ]);

  // remote_camera_off was captured on a guest;
  // here <P_HOST> is self and never a remote entry.
  assert.equal(telemost.ingestIncoming(state, frame("in.upsertDescription.remote_camera_off"), 2_200).roster, false);

  const leave = telemost.ingestIncoming(state, frame("in.removeDescription.leave"), 3_000);
  assert.deepEqual(leave.left, ["<P2>"]);
  assert.deepEqual(telemost.roster(state).map((person) => person.participantId), ["<P_HOST>", "<P1>"]);
  assert.deepEqual(
    telemost.ingestIncoming(state, frame("in.removeDescription.leave"), 3_100).left,
    [],
    "a repeated leave is not a second departure",
  );
});

test("a guest sees the host in the snapshot and a later snapshot replaces the roster", () => {
  const state = telemost.createState();
  telemost.ingestOutgoing(state, frame("out.hello", 0, { "<P_HOST>": "<P1>", "<NAME_HOST>": "<NAME_P1>" }), 1_000);
  telemost.ingestIncoming(state, frame("in.updateDescription.snapshot_on_join_one_peer"), 1_100);
  telemost.ingestIncoming(state, frame("in.upsertDescription.remote_mic_muted"), 1_200);
  assert.deepEqual(telemost.roster(state).map((person) => [person.participantId, person.isSelf]), [
    ["<P1>", true], ["<P_HOST>", false], ["<P2>", false],
  ]);

  const reconnect = telemost.ingestIncoming(state, frame("in.updateDescription.snapshot_on_join_alone"), 5_000);
  assert.deepEqual(reconnect.left.sort(), ["<P2>", "<P_HOST>"]);
  assert.deepEqual(telemost.roster(state).map((person) => person.participantId), ["<P1>"]);
});

test("slot VAD keeps one interval per speech turn for participant and video slots", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, frame("in.upsertDescription.join"), 1_200);
  telemost.ingestIncoming(state, frame("in.upsertDescription.remote_mic_muted", 0, { '"sendAudio":false': '"sendAudio":true' }), 1_200);

  assert.equal(telemost.ingestIncoming(state, frame("in.slotsConfig.remote_speaking_vad_true"), 10_000).vad, true);
  // The same speaker turning the camera off moves
  // to a `participant` slot: still one turn.
  assert.equal(telemost.ingestIncoming(state, frame("in.slotsConfig.remote_speaking_camera_off_vad_true"), 10_500).vad, false);
  assert.deepEqual(state.vad.get("<P2>"), [{ on: 10_000, off: null, cut: false }]);
  assert.deepEqual(telemost.activeSpeakers(state).map((person) => person.participantId), ["<P2>"]);

  // vad:false on a listed slot is a hangover
  // edge; vanishing from the slots is a cut.
  assert.equal(telemost.ingestIncoming(state, slots([["<P2>", false]]), 12_000).vad, true);
  assert.deepEqual(state.vad.get("<P2>"), [{ on: 10_000, off: 12_000, cut: false }]);
  assert.deepEqual(telemost.activeSpeakers(state), []);
  assert.equal(telemost.ingestIncoming(state, frame("in.slotsConfig.three_remote_peers"), 13_000).vad, true);
  assert.deepEqual(state.vad.get("<P1>"), [{ on: 13_000, off: null, cut: false }]);
  telemost.ingestIncoming(state, frame("in.slotsConfig.alone"), 14_000);
  assert.deepEqual(state.vad.get("<P1>"), [{ on: 13_000, off: 14_000, cut: true }]);
});

test("speakerAt compensates the measured VAD on/off latency", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, slots([["<P1>", true]]), 10_000);
  telemost.ingestIncoming(state, slots([["<P1>", false]]), 13_000);

  assert.equal(telemost.speakerAt(state, 10_000 - VAD_ON_LATENCY_MS), "<P1>", "speech precedes the on edge");
  assert.equal(telemost.speakerAt(state, 10_000 - VAD_ON_LATENCY_MS - 1), null);
  assert.equal(telemost.speakerAt(state, 11_000), "<P1>");
  const lastSpoken = 13_000 - VAD_OFF_LATENCY_MS + VAD_OFF_MARGIN_MS;
  assert.equal(telemost.speakerAt(state, lastSpoken - 1), "<P1>");
  assert.equal(telemost.speakerAt(state, lastSpoken), null, "the off edge trails speech by the hangover");
  assert.equal(telemost.speakerAt(state, 20_000), null);
  assert.equal(telemost.speakerAt(state, Number.NaN), null);

  // A VAD blip shorter than the hangover cannot hide the earlier, cut turn.
  const blip = joinedHost();
  telemost.ingestIncoming(blip, slots([["<P1>", true]]), 10_000);
  telemost.ingestIncoming(blip, slots([]), 13_000);
  telemost.ingestIncoming(blip, slots([["<P1>", true]]), 13_100);
  telemost.ingestIncoming(blip, slots([["<P1>", false]]), 13_600);
  assert.equal(telemost.speakerAt(blip, 12_800), "<P1>");
  assert.equal(telemost.speakerAt(blip, 13_200), null);
});

test("an open turn and overlapping speakers resolve to the most recent on edge", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, slots([["<P1>", true]]), 10_000);
  telemost.ingestIncoming(state, slots([["<P1>", true], ["<P2>", true]]), 12_000);

  assert.equal(telemost.speakerAt(state, 11_000), "<P1>", "P2 had not started yet");
  assert.equal(telemost.speakerAt(state, 12_000 - VAD_ON_LATENCY_MS), "<P2>", "the newer turn wins the overlap");
  assert.equal(telemost.speakerAt(state, 14_000), "<P2>");

  // P2 stops, P1 still flagged: once P2's
  // hangover has passed P1 owns the mix again.
  telemost.ingestIncoming(state, slots([["<P1>", true], ["<P2>", false]]), 16_000);
  assert.equal(telemost.speakerAt(state, 14_000), "<P2>");
  assert.equal(telemost.speakerAt(state, 16_000 - VAD_OFF_LATENCY_MS + VAD_OFF_MARGIN_MS), "<P1>");
});

test("unknown time ranges, self slots and vadActivity never name a remote speaker", () => {
  const state = joinedHost();
  assert.equal(telemost.speakerAt(state, 5_000), null);

  telemost.ingestIncoming(state, slots([["<P_HOST>", true]]), 6_000);
  assert.equal(telemost.speakerAt(state, 6_000), null);

  const self = telemost.ingestIncoming(state, frame("in.vadActivity.self", 0), 7_000);
  assert.equal(self.vad, true);
  assert.equal(state.selfSpeaking, true);
  assert.equal(telemost.speakerAt(state, 7_000), null);
  assert.deepEqual(telemost.activeSpeakers(state).map((person) => [person.participantId, person.isSelf]), [
    ["<P_HOST>", true],
  ]);
  telemost.ingestIncoming(state, frame("in.vadActivity.self", 1), 8_000);
  assert.equal(state.selfSpeaking, false);
});

test("a departure closes the speaker's turn but keeps history for held audio", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, frame("in.upsertDescription.remote_mic_muted", 0, { '"sendAudio":false': '"sendAudio":true' }), 1_200);
  telemost.ingestIncoming(state, slots([["<P2>", true]]), 10_000);
  const leave = telemost.ingestIncoming(state, frame("in.removeDescription.leave"), 11_000);
  assert.deepEqual(leave.left, ["<P2>"]);
  assert.equal(leave.vad, true);
  assert.equal(telemost.speakerAt(state, 10_000), "<P2>", "audio held from before the leave still routes");
  assert.equal(telemost.speakerAt(state, 10_999), "<P2>", "a leave ends the turn without a VAD hangover");
  assert.equal(telemost.speakerAt(state, 11_000), null);
  assert.deepEqual(telemost.activeSpeakers(state), []);
});

test("request-states avatars and names attach to goloom participant ids", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, slots([["<P2>", true]]), 2_000);
  const initial = telemost.ingestPeerStates(state, fixture["rest.POST_request-states.initial"].response);
  assert.equal(initial.roster, true);
  assert.equal(initial.self, true);
  assert.equal(telemost.selfIdentity(state).avatarUrl, "https://avatars.example.test/p_host/islands-300");

  // A VAD-only participant (no description yet) is named from REST.
  telemost.ingestPeerStates(state, JSON.stringify(fixture["rest.POST_request-states.new_peer"].response));
  assert.deepEqual(telemost.identity(state, "<P2>"), {
    participantId: "<P2>", displayName: "<NAME_P2>", avatarUrl: null, isSelf: false, muted: null,
  });
  assert.deepEqual(
    telemost.ingestPeerStates(state, fixture["rest.POST_request-states.periodic_no_change"].response),
    { roster: false, vad: false, self: false, left: [] },
  );

  telemost.ingestPeerStates(state, { peers: [
    { peer_id: "<P1>", state: { user_data: { display_name: "Default", avatar_url: "https://avatars.example.test/default", is_default_avatar: true } } },
    { peer_id: "<P3>", state: { user_data: { display_name: "Script", avatar_url: "javascript:alert(1)" } } },
  ] });
  assert.equal(telemost.identity(state, "<P1>").avatarUrl, null);
  assert.equal(telemost.identity(state, "<P3>").avatarUrl, null);
});

test("hidden participants stay routable but out of the visible roster", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, frame("in.upsertDescription.join", 0, {
    '"hideFromParticipantsList":false': '"hideFromParticipantsList":true',
  }), 2_000);
  assert.deepEqual(telemost.roster(state).map((person) => person.participantId), ["<P_HOST>"]);
  assert.equal(telemost.identity(state, "<P1>").displayName, "<NAME_P1>");
});

test("closing the goloom session clears every speaking flag", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, slots([["<P1>", true]]), 2_000);
  telemost.ingestIncoming(state, frame("in.vadActivity.self", 0), 2_000);
  const closed = telemost.closeSession(state, 3_000);
  assert.equal(closed.vad, true);
  assert.equal(closed.roster, true);
  assert.equal(state.sessionOpen, false);
  assert.equal(state.selfSpeaking, false);
  assert.deepEqual(telemost.activeSpeakers(state), []);
});

test("unknown and malformed signalling is ignored without throwing", () => {
  const state = joinedHost();
  telemost.ingestIncoming(state, frame("in.upsertDescription.join"), 1_200);
  const unchanged = { roster: false, vad: false, self: false, left: [] };
  for (const message of [
    null, undefined, 42, "", "{", "[]", '{"slotsConfig":', new ArrayBuffer(8),
    frame("in.ack"), frame("in.slotsMeta"), frame("in.upsertParticipantsQualityReport"), frame("in.selfQualityReport"),
    { slotsConfig: 5 }, { slotsConfig: { slots: "x" } }, { slotsConfig: { slots: [null, 1, "x", { vad: true }] } },
    { slotsConfig: { slots: [{ vad: true, participant: "x" }, { vad: true, participantVideoByMid: { participantId: 7 } }] } },
    { upsertDescription: { description: "x" } }, { upsertDescription: { description: [null, 3, { id: 9 }] } },
    { updateDescription: { description: null } }, { removeDescription: { descriptionId: "<P1>" } },
    { vadActivity: null },
  ]) {
    assert.deepEqual(telemost.ingestIncoming(state, message, 2_000), unchanged, String(message));
    assert.deepEqual(telemost.ingestOutgoing(state, message, 2_000), unchanged, String(message));
  }
  for (const response of [null, "{", "[]", { peers: "x" }, { peers: [null, { peer_id: "<P1>" }, { state: {} }] }]) {
    assert.deepEqual(telemost.ingestPeerStates(state, response), unchanged);
  }
  assert.deepEqual(telemost.roster(state).map((person) => person.participantId), ["<P_HOST>", "<P1>"]);
  assert.equal(telemost.identity(state, ""), null);
});

test("signalling and avatar URLs are recognised narrowly", () => {
  assert.equal(telemost.isGoloomUrl("wss://goloom.strm.yandex.net/join"), true);
  assert.equal(telemost.isGoloomUrl("wss://example.test/goloom"), false);
  assert.equal(telemost.isGoloomUrl("https://goloom.strm.yandex.net/join"), false);
  assert.equal(telemost.isGoloomUrl("not a url"), false);
  assert.equal(telemost.isPeerStatesUrl(fixture["rest.POST_request-states.initial"].url), true);
  assert.equal(
    telemost.isPeerStatesUrl("https://cloud-api.yandex.ru/telemost_front/v2/telemost/conferences/x/connection?display_name=a"),
    false,
  );
  assert.equal(telemost.isPeerStatesUrl(undefined), false);
});

test("the Telemost microphone gate fails closed outside the call and without any mute signal", () => {
  const gate = (overrides) => capturePolicy.shouldSendTelemostMicrophone({
    trackEnabled: true, inCall: true, controlMuted: null, protocolMuted: null, ...overrides,
  });
  assert.equal(gate({ controlMuted: false }), true);
  assert.equal(gate({ controlMuted: true, protocolMuted: false }), false, "the visible toolbar wins");
  assert.equal(gate({ controlMuted: false, protocolMuted: true }), true);
  assert.equal(gate({ protocolMuted: false }), true);
  assert.equal(gate({ protocolMuted: true }), false);
  assert.equal(gate({}), false);
  assert.equal(gate({ controlMuted: false, inCall: false }), false);
  assert.equal(gate({ controlMuted: false, trackEnabled: false }), false);
});

test("Telemost media elements never name the server mix from DOM tiles", () => {
  const tile = { id: "tile", name: "Someone", avatarUrl: null, isSelf: false };
  assert.equal(capturePolicy.mediaElementIdentity("yandex_telemost", tile), null);
  assert.equal(capturePolicy.mediaElementIdentity("microsoft_teams", tile), tile);
});
