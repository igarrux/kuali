import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

import "../src/capture-policy.js";
import "../src/telemost-protocol.js";

const pageCaptureSource = readFileSync(new URL("../src/page-capture.js", import.meta.url), "utf8");
const fixture = JSON.parse(readFileSync(new URL("./fixtures/telemost-signalling.json", import.meta.url), "utf8"));
const GOLOOM_URL = "wss://goloom.strm.yandex.net/join";
const STATES_URL = fixture["rest.POST_request-states.initial"].url;
// Objects built inside the vm context have foreign prototypes.
const plain = (value) => JSON.parse(JSON.stringify(value));

function frame(key, index = 0, replacements = {}) {
  let text = JSON.stringify(fixture[key][index].msg);
  for (const [from, to] of Object.entries(replacements)) text = text.split(from).join(to);
  return text;
}

function slots(speaking) {
  return JSON.stringify({
    uid: "slots",
    slotsConfig: {
      key: 1,
      slots: [
        { selfView: {}, vad: false },
        ...speaking.map(([participantId, vad]) => ({ participant: { participantId }, vad })),
      ],
    },
  });
}

function createTelemostHarness() {
  let now = 1_000;
  let nextInterval = 1;
  const intervals = new Map();
  const listeners = new Map();
  const posts = [];
  const captureStateWaiters = [];
  const worklets = [];
  const dom = { inCall: true, mic: "on" };

  class FakeDate extends Date {
    static now() {
      return now;
    }
  }

  class FakeTrack {
    constructor(id) {
      this.id = id;
      this.kind = "audio";
      this.label = id;
      this.enabled = true;
      this.muted = false;
      this.readyState = "live";
      this.listeners = new Map();
    }

    addEventListener(type, listener) {
      this.listeners.set(type, [...(this.listeners.get(type) || []), listener]);
    }

    stop() {
      this.readyState = "ended";
      for (const listener of this.listeners.get("ended") || []) listener();
    }
  }

  class FakeMediaStream {
    constructor(tracks = []) {
      this.id = `stream-${tracks.map((track) => track.id).join("-")}`;
      this.tracks = tracks;
    }

    getAudioTracks() {
      return this.tracks.filter((track) => track.kind === "audio");
    }

    getTracks() {
      return this.tracks;
    }
  }

  class FakeAudioWorkletNode {
    constructor() {
      this.port = { onmessage: null };
      this.stream = null;
      worklets.push(this);
    }

    connect() {}

    disconnect() {}
  }

  class FakeAudioContext {
    constructor() {
      this.state = "running";
      this.destination = {};
      this.audioWorklet = { addModule: () => Promise.resolve() };
    }

    createMediaStreamSource(stream) {
      return {
        connect(processor) {
          processor.stream = stream;
        },
        disconnect() {},
      };
    }

    resume() {
      return Promise.resolve();
    }

    close() {
      this.state = "closed";
      return Promise.resolve();
    }
  }

  class FakePeerConnection {
    constructor() {
      this.listeners = new Map();
    }

    addEventListener(type, listener) {
      this.listeners.set(type, listener);
    }

    createDataChannel() {
      return { label: "default", addEventListener() {} };
    }

    getSenders() {
      return [];
    }

    emitTrack(track) {
      this.listeners.get("track")?.({ track, receiver: { track }, streams: [] });
    }
  }

  class FakeWebSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 1;
      this.sent = [];
      this.listeners = new Map();
    }

    addEventListener(type, listener) {
      this.listeners.set(type, [...(this.listeners.get(type) || []), listener]);
    }

    send(data) {
      if (data === "throw") throw new Error("InvalidStateError");
      this.sent.push(data);
    }

    receive(data) {
      for (const listener of this.listeners.get("message") || []) listener({ data });
    }

    close() {
      this.readyState = 3;
      for (const listener of this.listeners.get("close") || []) listener({ code: 1000 });
    }
  }
  FakeWebSocket.OPEN = 1;

  const fetchResponses = new Map();
  function fakeFetch(url) {
    const body = fetchResponses.get(url);
    const response = {
      url,
      bodyUsed: false,
      clone: () => ({ json: () => Promise.resolve(JSON.parse(body)) }),
      json() {
        response.bodyUsed = true;
        return Promise.resolve(JSON.parse(body));
      },
    };
    return Promise.resolve(response);
  }

  const element = { getAttribute: () => null, getClientRects: () => [{}] };
  const document = {
    title: "Telemost",
    querySelector(selector) {
      if (selector.includes("end-call-alt-button")) return dom.inCall ? element : null;
      return null;
    },
    querySelectorAll(selector) {
      if (selector === '[data-testid="call-toolbar"] [data-testid="turn-on-mic-button"]') {
        return dom.mic === "off" ? [element] : [];
      }
      if (selector === '[data-testid="call-toolbar"] [data-testid="turn-off-mic-button"]') {
        return dom.mic === "on" ? [element] : [];
      }
      return [];
    },
  };

  const window = {
    top: {},
    addEventListener(type, listener) {
      listeners.set(type, [...(listeners.get(type) || []), listener]);
    },
    postMessage(message) {
      posts.push(message);
      if (message?.type !== "capture-state") return;
      for (let index = captureStateWaiters.length - 1; index >= 0; index -= 1) {
        if (captureStateWaiters[index].state !== message.state) continue;
        captureStateWaiters.splice(index, 1)[0].resolve(message);
      }
    },
    RTCPeerConnection: FakePeerConnection,
    WebSocket: FakeWebSocket,
    fetch: fakeFetch,
  };
  const sandbox = {
    AbortController,
    AudioContext: FakeAudioContext,
    AudioWorkletNode: FakeAudioWorkletNode,
    Date: FakeDate,
    KualiCapturePolicy: globalThis.KualiCapturePolicy,
    KualiTelemostProtocol: globalThis.KualiTelemostProtocol,
    MediaStream: FakeMediaStream,
    URL,
    clearInterval: (id) => intervals.delete(id),
    document,
    getComputedStyle: () => ({ display: "block", visibility: "visible" }),
    location: { hostname: "telemost.yandex.ru", pathname: "/private-join/12345678901234" },
    navigator: {
      mediaDevices: {
        getUserMedia: () => Promise.resolve(new FakeMediaStream([new FakeTrack("local-mic")])),
      },
    },
    performance: { now: () => now },
    queueMicrotask,
    setInterval(callback) {
      const id = nextInterval++;
      intervals.set(id, callback);
      return id;
    },
    window,
  };
  vm.runInNewContext(pageCaptureSource, sandbox);

  const flush = async () => {
    for (let index = 0; index < 6; index += 1) await Promise.resolve();
  };
  const controlAndWait = async (command) => {
    const settled = new Promise((resolve) => {
      captureStateWaiters.push({ state: command === "start" ? "capturing" : "idle", resolve });
    });
    for (const listener of listeners.get("message") || []) {
      listener({ source: window, data: { protocol: "kuali.control.v1", command, workletUrl: "chrome-extension://test/pcm.js" } });
    }
    await settled;
    await flush();
  };
  const worklet = (trackId) => {
    const node = worklets.findLast((candidate) => candidate.stream?.getAudioTracks()[0]?.id === trackId);
    assert(node, `a PCM worklet must capture ${trackId}`);
    return {
      pcm(value, at = now) {
        now = at;
        node.port.onmessage?.({ data: new Float32Array(2048).fill(value).buffer });
      },
    };
  };
  return {
    FakeTrack,
    FakeWebSocket,
    controlAndWait,
    dom,
    fetchResponses,
    flush,
    posts,
    window,
    worklet,
    events: (kind) => posts.filter((message) => message.type === "meeting-event" && message.kind === kind),
    audio: () => posts.filter((message) => message.type === "audio"),
    runIntervals() {
      for (const callback of [...intervals.values()]) callback();
    },
    setNow(value) {
      now = value;
    },
  };
}

async function joinedCapture({ receivers = ["remote-mix"] } = {}) {
  const harness = createTelemostHarness();
  const socket = new harness.window.WebSocket(GOLOOM_URL);
  socket.send(frame("out.hello"));
  socket.receive(frame("in.updateDescription.snapshot_on_join_alone"));
  socket.receive(frame("in.upsertDescription.join"));
  socket.receive(frame("in.upsertDescription.remote_mic_muted", 0, { '"sendAudio":false': '"sendAudio":true' }));
  const peer = new harness.window.RTCPeerConnection();
  const tracks = receivers.map((id) => new harness.FakeTrack(id));
  for (const track of tracks) peer.emitTrack(track);
  await harness.controlAndWait("start");
  return {
    harness,
    socket,
    tracks,
    remote: harness.worklet(receivers[0]),
    worklets: receivers.map((id) => harness.worklet(id)),
    mic: harness.worklet("local-mic"),
  };
}

// Release everything held for at least the routing delay.
function settle(harness, at) {
  harness.setNow(at);
  harness.runIntervals();
}

// Speaker channels come from the page's shared counter, so read them back.
function speakerChannel(harness, participantId) {
  const upsert = harness.events("participant-upsert").find((event) => event.detail.participantId === participantId);
  assert(upsert, `${participantId} must be announced`);
  return upsert.detail.channel;
}

test("one Telemost mix is split into stable per-speaker channels by goloom VAD", async () => {
  const { harness, socket, remote } = await joinedCapture();

  remote.pcm(0.1, 5_000);
  remote.pcm(0.15, 9_800);
  harness.setNow(10_000);
  socket.receive(slots([["<P1>", true], ["<P2>", false]]));
  remote.pcm(0.2, 10_100);
  remote.pcm(0.2, 10_228);
  harness.setNow(12_000);
  socket.receive(slots([["<P1>", false], ["<P2>", true]]));
  remote.pcm(0.3, 12_300);
  harness.setNow(13_000);
  harness.runIntervals();

  const p1 = speakerChannel(harness, "<P1>");
  const p2 = speakerChannel(harness, "<P2>");
  assert.notEqual(p1, p2);
  for (const channel of [p1, p2]) {
    assert(channel > 0 && channel < 999, `speaker channel ${channel} stays below the reserved channels`);
  }
  assert.deepEqual(harness.audio().map(({ channel, ts, pcm }) => [channel, ts, pcm[0]]), [
    [999, 5_000, 0.10000000149011612],
    [p1, 9_800, 0.15000000596046448],
    [p1, 10_100, 0.20000000298023224],
    [p1, 10_228, 0.20000000298023224],
    [p2, 12_300, 0.30000001192092896],
  ], "the delay buffer relabels speech that preceded its VAD edge");
  const upserts = harness.events("participant-upsert").filter((event) => event.detail.channel !== 1000);
  assert.deepEqual(plain(upserts.map((event) => event.detail)), [
    {
      channel: 999, participantId: "yandex_telemost:mixed", displayName: "Sala",
      avatarUrl: null, isSelf: false, audioKind: "mixed", platform: "yandex_telemost",
    },
    {
      channel: p1, participantId: "<P1>", displayName: "<NAME_P1>",
      avatarUrl: null, isSelf: false, audioKind: "separate", platform: "yandex_telemost",
    },
    {
      channel: p2, participantId: "<P2>", displayName: "<NAME_P2>",
      avatarUrl: null, isSelf: false, audioKind: "separate", platform: "yandex_telemost",
    },
  ]);
  for (const upsert of upserts) {
    const firstAudio = harness.posts.findIndex((message) => message.type === "audio" && message.channel === upsert.detail.channel);
    assert(harness.posts.indexOf(upsert) < firstAudio, `channel ${upsert.detail.channel} is named before its audio`);
  }
  assert.equal(harness.events("track-connected").length, 0, "the transport lane is never announced");

  const active = harness.events("active-speakers").map((event) => event.detail.participants.map((person) => person.participantId));
  assert.deepEqual(active.slice(-2), [["<P1>"], ["<P2>"]]);
});

test("Telemost roster-state, active speakers and departures come from goloom", async () => {
  const { harness, socket, remote } = await joinedCapture();
  const roster = harness.events("roster-state").at(-1).detail;
  assert.equal(roster.platform, "yandex_telemost");
  assert.equal(roster.participantCount, 3);
  assert.deepEqual(roster.participants.map(({ participantId, isSelf }) => [participantId, isSelf]), [
    ["<P_HOST>", true], ["<P1>", false], ["<P2>", false],
  ]);
  assert.equal(roster.protocolBacked, true);
  assert.equal(roster.inCall, true);
  assert.equal(roster.selfPresentInDom, true);
  const rosterSends = harness.events("roster-state").length;
  socket.receive(frame("in.upsertDescription.remote_mic_muted"));
  assert.equal(harness.events("roster-state").length, rosterSends, "a remote mute toggle sends no roster-state");

  harness.setNow(10_000);
  socket.receive(slots([["<P2>", true]]));
  remote.pcm(0.2, 10_100);
  harness.setNow(11_000);
  harness.runIntervals();
  const p2 = speakerChannel(harness, "<P2>");
  socket.receive(frame("in.removeDescription.leave"));
  assert.deepEqual(
    harness.events("roster-state").at(-1).detail.participants.map((person) => person.participantId),
    ["<P_HOST>", "<P1>"],
  );
  assert.deepEqual(plain(harness.events("active-speakers").at(-1).detail.participants), []);
  assert.equal(harness.events("participant-left").length, 0, "held audio may still belong to the leaver");
  harness.setNow(11_700);
  harness.runIntervals();
  assert.deepEqual(plain(harness.events("participant-left").map((event) => event.detail)), [
    { channel: p2, participantId: "<P2>" },
  ]);

  // A rejoin with the same id reuses its channel
  // and is re-announced before new audio.
  socket.receive(frame("in.upsertDescription.remote_mic_muted", 0, { '"sendAudio":false': '"sendAudio":true' }));
  harness.setNow(20_000);
  socket.receive(slots([["<P2>", true]]));
  remote.pcm(0.25, 20_100);
  harness.setNow(21_000);
  harness.runIntervals();
  assert.equal(harness.audio().at(-1).channel, p2);
  assert.equal(harness.events("participant-upsert").at(-1).detail.channel, p2);

  socket.close();
  const left = harness.events("roster-state").at(-1).detail;
  harness.dom.inCall = false;
  socket.receive(frame("in.upsertDescription.join", 0, { '"<NAME_P1>"': '"Renamed"' }));
  assert.equal(left.inCall, true, "the call DOM still counts while goloom reconnects");
  assert.equal(harness.events("roster-state").at(-1).detail.inCall, false);
});

test("a quick rejoin cancels the pending departure", async () => {
  const { harness, socket, remote } = await joinedCapture();
  harness.setNow(10_000);
  socket.receive(slots([["<P2>", true]]));
  remote.pcm(0.2, 10_100);
  harness.setNow(11_000);
  harness.runIntervals();
  socket.receive(frame("in.removeDescription.leave"));
  socket.receive(frame("in.upsertDescription.remote_mic_muted", 0, { '"sendAudio":false': '"sendAudio":true' }));
  harness.setNow(12_000);
  harness.runIntervals();
  assert.equal(harness.events("participant-left").length, 0);
});

test("the Telemost microphone follows the toolbar, then the client's own mute, and fails closed", async () => {
  const { harness, socket, mic } = await joinedCapture();
  const micAudio = () => harness.audio().filter((message) => message.channel === 1000).length;
  const self = harness.events("participant-upsert").find((event) => event.detail.channel === 1000).detail;
  assert.deepEqual(plain(self), {
    channel: 1000, participantId: "<P_HOST>", displayName: "<NAME_HOST>",
    avatarUrl: null, isSelf: true, audioKind: "separate", platform: "yandex_telemost",
  });

  mic.pcm(0.2, 20_000);
  assert.equal(micAudio(), 0, "the mic lane is held as long as the mix");
  settle(harness, 20_600);
  assert.equal(micAudio(), 1, "toolbar shows the mic on");
  harness.dom.mic = "off";
  mic.pcm(0.2, 20_800);
  harness.dom.mic = "on";
  settle(harness, 21_500);
  assert.equal(micAudio(), 1, "a block captured while muted is never sent later");

  harness.dom.mic = null;
  socket.send(frame("out.updateMe.mic_off"));
  mic.pcm(0.2, 21_550);
  settle(harness, 22_200);
  assert.equal(micAudio(), 1, "without a visible control the client's updateMe is the authority");
  socket.send(frame("out.updateMe.camera_off"));
  mic.pcm(0.2, 22_210);
  settle(harness, 22_900);
  assert.equal(micAudio(), 2);

  harness.dom.inCall = false;
  harness.dom.mic = "on";
  socket.close();
  mic.pcm(0.2, 23_000);
  settle(harness, 23_700);
  assert.equal(micAudio(), 2, "outside the call nothing leaves the page");
});

test("the held mic and the routed mix leave in capture order", async () => {
  const { harness, socket, remote, mic } = await joinedCapture();
  harness.setNow(10_000);
  socket.receive(slots([["<P1>", true]]));
  remote.pcm(0.2, 10_100);
  mic.pcm(0.3, 10_150);
  remote.pcm(0.2, 10_228);
  assert.equal(harness.audio().length, 0);
  settle(harness, 11_000);
  const p1 = speakerChannel(harness, "<P1>");
  assert.deepEqual(harness.audio().map(({ channel, ts }) => [channel, ts]), [
    [p1, 10_100], [1000, 10_150], [p1, 10_228],
  ]);
});

test("the mix is leased to one receiver so an idle receiver cannot interleave", async () => {
  const { harness, socket, tracks, worklets: [mix, idle] } = await joinedCapture({
    receivers: ["remote-mix", "remote-idle"],
  });
  harness.setNow(9_000);
  socket.receive(slots([["<P1>", true]]));
  const routed = () => harness.audio().filter((message) => message.channel !== 1000).map(({ ts, pcm }) => [ts, pcm[0]]);

  idle.pcm(0.0006, 9_900);
  mix.pcm(0.25, 10_000);
  idle.pcm(0.0006, 10_050);
  mix.pcm(0.25, 10_128);
  idle.pcm(0.5, 10_150);
  settle(harness, 11_000);
  assert.deepEqual(routed(), [[10_000, 0.25], [10_128, 0.25]], "only the lease holder reaches the router");

  idle.pcm(0.75, 11_700);
  mix.pcm(0.0001, 11_750);
  settle(harness, 12_400);
  assert.deepEqual(routed().slice(2), [[11_700, 0.75]], "a silent holder hands over to a receiver with signal");

  tracks[1].stop();
  mix.pcm(0.25, 12_500);
  settle(harness, 13_200);
  assert.deepEqual(routed().slice(3), [[12_500, 0.25]], "an ended holder releases the lease at once");
});

test("held audio of a departed speaker keeps the channel's last known name", async () => {
  const { harness, socket, remote } = await joinedCapture();
  harness.setNow(10_000);
  socket.receive(slots([["<P2>", true]]));
  remote.pcm(0.2, 10_100);
  remote.pcm(0.2, 10_228);
  settle(harness, 10_750);
  socket.receive(frame("in.removeDescription.leave"));
  settle(harness, 10_900);
  const p2 = speakerChannel(harness, "<P2>");
  assert.deepEqual(harness.audio().map(({ channel, ts }) => [channel, ts]), [[p2, 10_100], [p2, 10_228]]);
  settle(harness, 11_500);
  const channelEvents = harness.posts.filter((message) => message.type === "meeting-event" && message.detail?.channel === p2);
  assert.deepEqual(plain(channelEvents.map((event) => [event.kind, event.detail.displayName ?? event.speaker])), [
    ["participant-upsert", "<NAME_P2>"],
    ["participant-left", "<NAME_P2>"],
  ]);
});

test("a late send on a closing goloom socket cannot revive the session", async () => {
  const harness = createTelemostHarness();
  harness.dom.inCall = false;
  const socket = new harness.window.WebSocket(GOLOOM_URL);
  socket.send(frame("out.hello"));
  assert.equal(harness.events("roster-state").at(-1).detail.inCall, true);
  socket.readyState = 2;
  socket.send(frame("out.updateMe.mic_off"));
  socket.close();
  assert.equal(harness.events("roster-state").at(-1).detail.inCall, false);
  socket.send(frame("out.hello"));
  assert.equal(harness.events("roster-state").at(-1).detail.inCall, false, "a closed socket never reopens the session");
});

test("goloom observation leaves the page's sockets untouched", async () => {
  const harness = createTelemostHarness();
  const { FakeWebSocket } = harness;
  assert.equal(harness.window.WebSocket, FakeWebSocket, "the constructor is not replaced");
  const socket = new harness.window.WebSocket(GOLOOM_URL);
  assert(socket instanceof harness.window.WebSocket);
  const binary = new Uint8Array([1, 2, 3]).buffer;
  socket.send(frame("out.hello"));
  socket.send(binary);
  socket.send("{not json");
  assert.deepEqual(socket.sent.slice(1), [binary, "{not json"]);
  assert.equal(socket.sent[0], frame("out.hello"));
  assert.throws(() => socket.send("throw"), /InvalidStateError/, "the page still sees native send errors");
  assert.doesNotThrow(() => socket.receive("{broken"));
  assert.doesNotThrow(() => socket.receive(binary));

  const other = new harness.window.WebSocket("wss://example.test/chat");
  other.send(JSON.stringify({ hello: { participantId: "intruder" } }));
  assert.equal(other.listeners.size, 0, "only goloom sockets are observed");
  assert.equal(harness.events("roster-state").at(-1).detail.participants[0].participantId, "<P_HOST>");
});

test("request-states avatars reach the roster and the self lane through fetch", async () => {
  const { harness } = await joinedCapture();
  harness.fetchResponses.set(STATES_URL, JSON.stringify(fixture["rest.POST_request-states.initial"].response));
  const response = await harness.window.fetch(STATES_URL, { method: "POST" });
  assert.equal(response.bodyUsed, false, "the page keeps its own response body");
  await harness.flush();
  const selfAvatar = "https://avatars.example.test/p_host/islands-300";
  assert.equal(harness.events("roster-state").at(-1).detail.participants[0].avatarUrl, selfAvatar);
  assert.equal(harness.events("participant-upsert").at(-1).detail.channel, 1000);
  assert.equal(harness.events("participant-upsert").at(-1).detail.avatarUrl, selfAvatar);
});

test("stop flushes the held mix in order and a restart starts clean", async () => {
  const { harness, socket, remote } = await joinedCapture();
  harness.setNow(10_000);
  socket.receive(slots([["<P1>", true]]));
  remote.pcm(0.2, 10_100);
  remote.pcm(0.2, 10_228);
  assert.equal(harness.audio().length, 0);
  await harness.controlAndWait("stop");
  const p1 = speakerChannel(harness, "<P1>");
  assert.deepEqual(harness.audio().map(({ channel, ts }) => [channel, ts]), [[p1, 10_100], [p1, 10_228]]);
  const idle = harness.posts.findIndex((message) => message.type === "capture-state" && message.state === "idle");
  assert(harness.posts.findLastIndex((message) => message.type === "audio") < idle);
  assert.deepEqual(plain(harness.events("active-speakers").at(-1).detail.participants), []);

  await harness.controlAndWait("start");
  const restarted = harness.posts.length;
  const remoteAgain = harness.worklet("remote-mix");
  remoteAgain.pcm(0.2, 10_400);
  harness.setNow(11_100);
  harness.runIntervals();
  const replay = harness.posts.slice(restarted);
  assert.deepEqual(
    replay.filter((message) => message.type === "meeting-event" && message.kind === "participant-upsert" && message.detail.channel === p1).length,
    1,
    "a new capture re-announces the stable channel",
  );
  assert.equal(replay.filter((message) => message.type === "audio").at(-1).channel, p1);
});

test("Telemost speakers fall back to the mix once the shared counter reaches 999", async () => {
  // The receivers take channels 1 to 997, which leaves 998 for one speaker.
  const receivers = Array.from({ length: 997 }, (_, index) => `remote-${index}`);
  const { harness, socket, worklets: [remote] } = await joinedCapture({ receivers });
  harness.setNow(10_000);
  socket.receive(slots([["<P1>", true]]));
  remote.pcm(0.2, 10_100);
  harness.setNow(12_000);
  socket.receive(slots([["<P1>", false], ["<P2>", true]]));
  remote.pcm(0.3, 12_300);
  settle(harness, 13_000);
  const routed = harness.audio().filter((message) => message.channel !== 1000);
  assert.deepEqual(routed.map(({ channel, ts }) => [channel, ts]), [[998, 10_100], [999, 12_300]]);
  const upserts = harness.events("participant-upsert").filter((event) => event.detail.channel !== 1000);
  assert.deepEqual(upserts.map(({ detail }) => [detail.channel, detail.participantId]), [
    [998, "<P1>"], [999, "yandex_telemost:mixed"],
  ]);
});
