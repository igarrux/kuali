import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

import "../src/capture-policy.js";

const pageCaptureSource = readFileSync(new URL("../src/page-capture.js", import.meta.url), "utf8");

function createHarness({
  withPeerConnection = false,
  autoDecode = true,
  deferAudioContextClose = false,
  deferMicrophone = false,
  rejectAudioWorklet = false,
  throwAudioWorklet = false,
  throwDecoderDecodeOnce = false,
  decodedSampleValue = 0,
  topLevel = false,
} = {}) {
  let now = 0;
  let nextInterval = 1;
  const listeners = new Map();
  const posts = [];
  const captureStateWaiters = [];
  const transforms = [];
  const decoders = [];
  const trackProcessors = [];
  const audioContexts = [];
  const audioContextCloseResolvers = [];
  const microphoneResolvers = [];
  let microphoneRequestCount = 0;
  let decoderDecodeThrowsRemaining = throwDecoderDecodeOnce ? 1 : 0;
  let notifyAudioContextCloseRequested;
  let notifyMicrophoneRequested;
  const audioContextCloseRequested = new Promise((resolve) => {
    notifyAudioContextCloseRequested = resolve;
  });
  const microphoneRequested = new Promise((resolve) => {
    notifyMicrophoneRequested = resolve;
  });

  class FakeTrack {
    constructor(id) {
      this.id = id;
      this.kind = "audio";
      this.label = "remote";
      this.enabled = true;
      this.muted = false;
      this.readyState = "live";
      this.listeners = new Map();
    }

    addEventListener(type, listener) {
      const current = this.listeners.get(type) || [];
      current.push(listener);
      this.listeners.set(type, current);
    }

    end() {
      this.readyState = "ended";
      for (const listener of this.listeners.get("ended") || []) listener();
    }

    stop() {
      this.end();
    }
  }

  class FakeReceiver {
    constructor(track) {
      this.track = track;
      this.codecs = [{ payloadType: 109, mimeType: "audio/opus", clockRate: 48_000, channels: 2 }];
      this.contributingSources = [];
    }

    getParameters() {
      return { codecs: this.codecs };
    }

    getContributingSources() {
      return this.contributingSources;
    }

    createEncodedStreams() {
      const readable = {
        pipeThrough(transform) {
          transforms.push(transform);
          return readable;
        },
      };
      return { readable, writable: {} };
    }
  }

  class FakeTransformStream {
    constructor(transformer) {
      this.transformer = transformer;
    }
  }

  class FakeAudioData {
    constructor(timestamp, samples = new Float32Array(960)) {
      this.timestamp = timestamp;
      this.samples = samples;
      this.numberOfFrames = samples.length;
      this.numberOfChannels = 1;
      this.sampleRate = 48_000;
      this.closed = false;
    }

    copyTo(target) {
      target.set(this.samples);
    }

    close() {
      this.closed = true;
    }
  }

  class FakeAudioDecoder {
    constructor(callbacks) {
      this.callbacks = callbacks;
      this.state = "unconfigured";
      this.decodeCount = 0;
      this.pending = [];
      decoders.push(this);
    }

    configure() {
      this.state = "configured";
    }

    decode(chunk) {
      this.decodeCount += 1;
      if (decoderDecodeThrowsRemaining > 0) {
        decoderDecodeThrowsRemaining -= 1;
        throw new Error("decoder rejected the chunk synchronously");
      }
      this.pending.push(chunk);
      if (autoDecode) this.emitNext(new Float32Array(960).fill(decodedSampleValue));
    }

    emitNext(samples = new Float32Array(960)) {
      const chunk = this.pending.shift();
      assert(chunk, "the decoder must have a pending chunk");
      return this.emit(chunk.timestamp, samples);
    }

    emit(timestamp, samples = new Float32Array(960)) {
      const audioData = new FakeAudioData(timestamp, samples);
      this.callbacks.output(audioData);
      return audioData;
    }

    close() {
      this.state = "closed";
    }
  }

  class FakeEncodedAudioChunk {
    constructor(init) {
      Object.assign(this, init);
    }
  }

  class FakeAudioContext {
    constructor() {
      this.state = "running";
      this.destination = {};
      this.audioWorklet = {
        addModule: () => {
          if (throwAudioWorklet) throw new Error("worklet setup threw synchronously");
          return rejectAudioWorklet
            ? Promise.reject(new Error("worklet setup failed"))
            : Promise.resolve();
        },
      };
      audioContexts.push(this);
    }

    resume() {
      return Promise.resolve();
    }

    close() {
      this.state = "closed";
      notifyAudioContextCloseRequested();
      if (deferAudioContextClose) {
        return new Promise((resolve) => audioContextCloseResolvers.push(resolve));
      }
      return Promise.resolve();
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

  class FakeMediaStreamTrackProcessor {
    constructor({ track }) {
      const queued = [];
      const waiters = [];
      const reader = {
        cancelled: false,
        read() {
          if (queued.length) return Promise.resolve({ value: queued.shift(), done: false });
          if (reader.cancelled) return Promise.resolve({ value: undefined, done: true });
          return new Promise((resolve) => waiters.push(resolve));
        },
        cancel() {
          reader.cancelled = true;
          while (waiters.length) waiters.shift()({ value: undefined, done: true });
          return Promise.resolve();
        },
      };
      this.track = track;
      this.reader = reader;
      this.push = (audioData) => {
        const resolve = waiters.shift();
        if (resolve) resolve({ value: audioData, done: false });
        else queued.push(audioData);
      };
      this.readable = { getReader: () => reader };
      trackProcessors.push(this);
    }
  }

  class FakePeerConnection {
    constructor() {
      this.listeners = new Map();
      this.senders = [];
    }

    addEventListener(type, listener) {
      this.listeners.set(type, listener);
    }

    createDataChannel() {
      return { label: "unused", addEventListener() {} };
    }

    getSenders() {
      return this.senders;
    }

    getReceivers() {
      return [];
    }

    getTransceivers() {
      return [];
    }

    getStats() {
      return Promise.resolve(new Map());
    }

    emitTrack(track, receiver) {
      this.listeners.get("track")?.({ track, receiver, streams: [] });
    }

    emitDataChannel(channel) {
      this.listeners.get("datachannel")?.({ channel });
    }
  }

  const window = {
    top: null,
    addEventListener(type, listener) {
      const current = listeners.get(type) || [];
      current.push(listener);
      listeners.set(type, current);
    },
    postMessage(message) {
      posts.push(message);
      if (message?.type === "capture-state") {
        for (let index = captureStateWaiters.length - 1; index >= 0; index -= 1) {
          const waiter = captureStateWaiters[index];
          if (waiter.state !== message.state) continue;
          captureStateWaiters.splice(index, 1);
          waiter.resolve(message);
        }
      }
    },
    RTCPeerConnection: withPeerConnection ? FakePeerConnection : undefined,
  };
  window.top = topLevel ? window : {};
  const document = {
    title: "Test Meet",
    querySelectorAll() {
      return [];
    },
  };
  const sandbox = {
    AbortController,
    AudioContext: FakeAudioContext,
    AudioData: FakeAudioData,
    AudioDecoder: FakeAudioDecoder,
    EncodedAudioChunk: FakeEncodedAudioChunk,
    KualiCapturePolicy: globalThis.KualiCapturePolicy,
    KualiMeetProtocol: {
      decodeCollectionPacket: async (value) => value,
      indexDeviceOutput(byDevice, bySource, output) {
        byDevice.set(output.deviceId, output);
        bySource.set(String(output.streamId), output);
        return { staleSource: "" };
      },
      orderedAsyncHandler: (handler) => handler,
    },
    MediaStream: FakeMediaStream,
    MediaStreamTrackProcessor: FakeMediaStreamTrackProcessor,
    RTCRtpReceiver: FakeReceiver,
    TransformStream: FakeTransformStream,
    clearInterval() {},
    document,
    location: { hostname: "meet.google.com" },
    navigator: {
      mediaDevices: {
        getUserMedia() {
          microphoneRequestCount += 1;
          notifyMicrophoneRequested();
          if (deferMicrophone) {
            return new Promise((resolve, reject) => microphoneResolvers.push({ resolve, reject }));
          }
          return Promise.resolve(new FakeMediaStream([new FakeTrack("local-mic")]));
        },
      },
    },
    performance: { now: () => now },
    queueMicrotask,
    setInterval() {
      return nextInterval++;
    },
    window,
  };
  vm.runInNewContext(pageCaptureSource, sandbox);

  const sendControl = (command) => {
    for (const listener of listeners.get("message") || []) {
      listener({
        source: window,
        data: {
          protocol: "kuali.control.v1",
          command,
          workletUrl: "chrome-extension://test/src/pcm-worklet.js",
        },
      });
    }
  };
  const pushEncodedFrame = (metadata = {}, transformIndex = -1) => {
    const transform = transforms.at(transformIndex)?.transformer;
    assert(transform, "the receiver encoded stream must be observed");
    const frame = {
      data: Uint8Array.from([1, 2, 3]).buffer,
      getMetadata: () => ({
        mimeType: "audio/opus",
        payloadType: 109,
        contributingSources: [{ source: 4242, audioLevel: 0.08 }],
        audioLevel: 0.08,
        ...metadata,
      }),
    };
    const forwarded = [];
    transform.transform(frame, { enqueue: (value) => forwarded.push(value) });
    assert.equal(forwarded[0], frame, "Kuali must not modify Meet's original encoded frame");
  };
  const flush = async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  };
  const pushTrackFrame = (receiver, {
    source = 4242,
    timestamp = now,
    audioLevel = 0.08,
    samples = new Float32Array(960).fill(0.08),
  } = {}) => {
    receiver.contributingSources = [{ source, timestamp, audioLevel }];
    const processor = trackProcessors.find((candidate) => candidate.track === receiver.track);
    assert(processor, "the receiver track processor must be active");
    processor.push(new FakeAudioData(timestamp, samples));
  };
  const waitForCaptureState = (state) => new Promise((resolve) => {
    captureStateWaiters.push({ state, resolve });
  });
  const controlAndWait = (command) => {
    const state = command === "start" ? "capturing" : "idle";
    const settled = waitForCaptureState(state);
    sendControl(command);
    return settled;
  };
  const createCollectionChannel = () => {
    const channelListeners = new Map();
    return {
      label: "collections",
      addEventListener(type, listener) {
        channelListeners.set(type, listener);
      },
      emit(value) {
        return channelListeners.get("message")?.({ data: value });
      },
    };
  };

  return {
    FakeReceiver,
    FakeTrack,
    audioContexts,
    decoders,
    createCollectionChannel,
    controlAndWait,
    flush,
    posts,
    pushEncodedFrame,
    pushTrackFrame,
    sendControl,
    resolveAudioContextClose() {
      const resolve = audioContextCloseResolvers.shift();
      assert(resolve, "an AudioContext close must be pending");
      resolve();
    },
    resolveMicrophone(stream = new FakeMediaStream([new FakeTrack("local-mic")])) {
      const pending = microphoneResolvers.shift();
      assert(pending, "a microphone request must be pending");
      pending.resolve(stream);
      return stream;
    },
    rejectMicrophone(error = new Error("microphone request failed")) {
      const pending = microphoneResolvers.shift();
      assert(pending, "a microphone request must be pending");
      pending.reject(error);
    },
    setNow(value) {
      now = value;
    },
    waitForAudioContextClose: () => audioContextCloseRequested,
    waitForCaptureState,
    waitForMicrophoneRequest: () => microphoneRequested,
    get microphoneRequestCount() {
      return microphoneRequestCount;
    },
    trackProcessors,
    transforms,
    window,
  };
}

async function prepareRoutedMeetReceiver(harness, trackId = "routed-remote-audio") {
  const peer = new harness.window.RTCPeerConnection();
  const channel = harness.createCollectionChannel();
  peer.emitDataChannel(channel);
  const track = new harness.FakeTrack(trackId);
  const receiver = new harness.FakeReceiver(track);
  peer.emitTrack(track, receiver);
  receiver.createEncodedStreams();
  await channel.emit({
    users: [{
      deviceId: "devices/remote",
      displayName: "Remote",
      status: 1,
      isCurrentUser: false,
    }],
    deviceOutputs: [{
      deviceId: "devices/remote",
      streamId: "4242",
      outputType: 1,
      disabled: false,
    }],
  });
  await harness.flush();
  return { peer, receiver, track };
}

test("the native Meet processor exclusively owns PCM while both receiver paths advance", async () => {
  const harness = createHarness({ withPeerConnection: true });
  const { receiver } = await prepareRoutedMeetReceiver(harness);
  await harness.controlAndWait("start");

  for (let index = 0; index < 7; index += 1) {
    harness.setNow(index * 20);
    harness.pushEncodedFrame();
    harness.pushTrackFrame(receiver);
    await harness.flush();
  }

  const audio = harness.posts.filter((message) => message.type === "audio");
  assert.equal(audio.length, 1, "the same receiver/CSRC must publish one PCM block, not two");
  assert.equal(harness.decoders.length, 0, "the auxiliary decoder stays cold while native PCM advances");
});

test("the encoded Meet decoder takes over after native progress stalls and yields on recovery", async () => {
  const harness = createHarness({ withPeerConnection: true, decodedSampleValue: 0.08 });
  const { receiver } = await prepareRoutedMeetReceiver(harness, "recoverable-remote-audio");
  await harness.controlAndWait("start");

  for (let index = 0; index < 7; index += 1) {
    harness.setNow(index * 20);
    harness.pushEncodedFrame();
    harness.pushTrackFrame(receiver);
    await harness.flush();
  }
  const nativeAudioCount = harness.posts.filter((message) => message.type === "audio").length;
  assert.equal(nativeAudioCount, 1);

  for (let now = 140; now <= 1_600; now += 20) {
    harness.setNow(now);
    harness.pushEncodedFrame();
  }
  assert.equal(harness.decoders.length, 0, "short native gaps keep the auxiliary path disabled");
  for (let now = 1_620; now <= 1_760; now += 20) {
    harness.setNow(now);
    harness.pushEncodedFrame();
  }
  const fallbackAudioCount = harness.posts.filter((message) => message.type === "audio").length;
  assert.equal(harness.decoders.length, 1, "the auxiliary decoder starts only after sustained native stall");
  assert(fallbackAudioCount > nativeAudioCount, "buffered encoded PCM must resume the audio flow");

  const decodedBeforeRecovery = harness.decoders[0].decodeCount;
  for (let index = 0; index < 6; index += 1) {
    harness.setNow(1_780 + index * 20);
    harness.pushEncodedFrame();
    harness.pushTrackFrame(receiver);
    await harness.flush();
  }
  assert(
    harness.decoders[0].decodeCount > decodedBeforeRecovery,
    "native subframes must not take ownership before they form a publishable PCM block",
  );
  const decodedBeforeNativeBlock = harness.decoders[0].decodeCount;
  const audioBeforeNativeBlock = harness.posts.filter((message) => message.type === "audio").length;
  harness.setNow(1_900);
  harness.pushTrackFrame(receiver);
  await harness.flush();
  harness.pushEncodedFrame();
  assert.equal(
    harness.decoders[0].decodeCount,
    decodedBeforeNativeBlock,
    "the first useful native block reclaims the CSRC before encoded can duplicate it",
  );
  assert.equal(
    harness.posts.filter((message) => message.type === "audio").length,
    audioBeforeNativeBlock + 1,
    "recovery publishes exactly the one native PCM block",
  );
});

test("continuous silent native AudioData cannot suppress an audible encoded fallback", async () => {
  const harness = createHarness({ withPeerConnection: true, decodedSampleValue: 0.08 });
  const { receiver } = await prepareRoutedMeetReceiver(harness, "silent-native-lane");
  await harness.controlAndWait("start");
  const silence = new Float32Array(960);

  for (let now = 0; now <= 1_760; now += 20) {
    harness.setNow(now);
    harness.pushEncodedFrame();
    harness.pushTrackFrame(receiver, { samples: silence });
    await harness.flush();
  }

  assert.equal(harness.decoders.length, 1, "silent native frames must not renew useful-PCM progress");
  assert(
    harness.posts.some((message) => message.type === "audio"),
    "audible encoded PCM must flow after the native lane proves silent",
  );
});

test("one global CSRC lease prevents cross-receiver PCM overlap and permits a real handoff", async () => {
  const harness = createHarness({ withPeerConnection: true, decodedSampleValue: 0.08 });
  const peer = new harness.window.RTCPeerConnection();
  const channel = harness.createCollectionChannel();
  peer.emitDataChannel(channel);
  const nativeTrack = new harness.FakeTrack("healthy-native-lane");
  const fallbackTrack = new harness.FakeTrack("silent-fallback-lane");
  const nativeReceiver = new harness.FakeReceiver(nativeTrack);
  const fallbackReceiver = new harness.FakeReceiver(fallbackTrack);
  peer.emitTrack(nativeTrack, nativeReceiver);
  peer.emitTrack(fallbackTrack, fallbackReceiver);
  nativeReceiver.createEncodedStreams();
  fallbackReceiver.createEncodedStreams();
  await channel.emit({
    users: [{
      deviceId: "devices/remote",
      displayName: "Remote",
      status: 1,
      isCurrentUser: false,
    }],
    deviceOutputs: [{
      deviceId: "devices/remote",
      streamId: "4242",
      outputType: 1,
      disabled: false,
    }],
  });
  await harness.flush();
  await harness.controlAndWait("start");
  const nativePcm = new Float32Array(960).fill(0.2);
  const silence = new Float32Array(960);

  for (let now = 0; now <= 1_760; now += 20) {
    harness.setNow(now);
    harness.pushEncodedFrame({}, 1);
    harness.pushTrackFrame(nativeReceiver, { samples: nativePcm });
    harness.pushTrackFrame(fallbackReceiver, { samples: silence });
    await harness.flush();
  }

  const beforeHandoff = harness.posts.filter((message) => message.type === "audio");
  assert(beforeHandoff.length > 0);
  assert.equal(harness.decoders.length, 1, "the silent receiver may prepare its fallback decoder");
  assert(
    beforeHandoff.every((message) => Math.max(...message.pcm.map(Math.abs)) > 0.15),
    "a fresh native owner must suppress encoded PCM from another receiver",
  );

  for (let now = 1_780; now <= 2_300; now += 20) {
    harness.setNow(now);
    harness.pushEncodedFrame({}, 1);
    harness.pushTrackFrame(fallbackReceiver, { samples: silence });
    await harness.flush();
  }
  const afterHandoff = harness.posts.filter((message) => message.type === "audio").slice(beforeHandoff.length);
  assert(
    afterHandoff.some((message) => {
      const peak = Math.max(...message.pcm.map(Math.abs));
      return peak > 0.05 && peak < 0.1;
    }),
    "encoded PCM may acquire the source only after the native owner really stops",
  );
});

test("audible remote RTP without PCM rebuilds only the auxiliary Meet decoder", async () => {
  const harness = createHarness();
  await harness.controlAndWait("start");

  const receiver = new harness.FakeReceiver(new harness.FakeTrack("remote-audio"));
  receiver.createEncodedStreams();
  for (let now = 0; now <= 4_000; now += 100) {
    harness.setNow(now);
    harness.pushEncodedFrame();
  }

  assert.equal(harness.decoders.length, 2);
  assert.equal(harness.decoders[0].state, "closed");
  assert.equal(harness.decoders[1].state, "configured");
  assert.equal(harness.transforms.length, 1, "the receiver tap stays attached during recovery");
});

test("a synchronous decoder.decode failure schedules a fresh auxiliary decoder", async () => {
  const harness = createHarness({ throwDecoderDecodeOnce: true });
  await harness.controlAndWait("start");

  const receiver = new harness.FakeReceiver(new harness.FakeTrack("throwing-decoder-lane"));
  receiver.createEncodedStreams();
  harness.pushEncodedFrame();
  const failedDecoder = harness.decoders[0];
  await harness.flush();

  assert.equal(failedDecoder.state, "closed");
  harness.pushEncodedFrame();
  assert.equal(harness.decoders.length, 2);
  assert.equal(harness.decoders[1].state, "configured");
  assert(harness.decoders[1].decodeCount > 0, "queued audio resumes on the replacement decoder");
});

test("pooled Meet lanes recover even when their audible CSRC alternates", async () => {
  const harness = createHarness();
  await harness.controlAndWait("start");

  const receiver = new harness.FakeReceiver(new harness.FakeTrack("pooled-remote-audio"));
  receiver.createEncodedStreams();
  for (let now = 0; now <= 4_000; now += 100) {
    harness.setNow(now);
    const source = Math.floor(now / 1_000) % 2 === 0 ? 4242 : 4343;
    harness.pushEncodedFrame({
      contributingSources: [{ source, audioLevel: 0.08 }],
      audioLevel: 0.08,
    });
  }
  assert.equal(harness.decoders.length, 2);
  assert.equal(harness.decoders[0].state, "closed");
  assert.equal(harness.decoders[1].state, "configured");
});

test("PCM on one receiver cannot hide a stalled receiver carrying the same CSRC", async () => {
  const harness = createHarness({ autoDecode: false });
  await harness.controlAndWait("start");

  const healthy = new harness.FakeReceiver(new harness.FakeTrack("healthy-lane"));
  const stalled = new harness.FakeReceiver(new harness.FakeTrack("stalled-lane"));
  healthy.createEncodedStreams();
  stalled.createEncodedStreams();
  const audiblePcm = new Float32Array(960).fill(0.08);

  for (let now = 0; now <= 4_000; now += 100) {
    harness.setNow(now);
    harness.pushEncodedFrame({}, 0);
    harness.decoders[0].emitNext(audiblePcm);
    harness.pushEncodedFrame({}, 1);
  }

  assert.equal(harness.decoders[0].state, "configured");
  assert.equal(harness.decoders[1].state, "closed");
  assert.equal(harness.decoders[2].state, "configured");
});

test("mixed-CSRC frames never trigger a false single-speaker decoder recovery", async () => {
  const harness = createHarness();
  await harness.controlAndWait("start");

  const receiver = new harness.FakeReceiver(new harness.FakeTrack("mixed-remote-audio"));
  receiver.createEncodedStreams();
  for (let now = 0; now <= 6_000; now += 250) {
    harness.setNow(now);
    harness.pushEncodedFrame({
      contributingSources: [
        { source: 4242, audioLevel: 0.08 },
        { source: 4343, audioLevel: 0.06 },
      ],
      audioLevel: 0.08,
    });
  }
  assert.equal(harness.decoders.length, 0);
});

test("stop and start discard a stale Meet decoder without reloading the page", async () => {
  const harness = createHarness();
  await harness.controlAndWait("start");

  const receiver = new harness.FakeReceiver(new harness.FakeTrack("remote-audio"));
  receiver.createEncodedStreams();
  harness.pushEncodedFrame();
  const firstDecoder = harness.decoders[0];

  await harness.controlAndWait("stop");
  assert.equal(firstDecoder.state, "closed");

  await harness.controlAndWait("start");
  harness.pushEncodedFrame();
  assert.equal(harness.decoders.length, 2);
  assert.notEqual(harness.decoders[1], firstDecoder);
  assert.equal(harness.transforms.length, 1, "restart reuses the attached tap, not its decoder state");
});

test("a Meet route generation change drops the old queue and resumes on a fresh decoder", async () => {
  const harness = createHarness({ withPeerConnection: true, autoDecode: false });
  const peer = new harness.window.RTCPeerConnection();
  const channel = harness.createCollectionChannel();
  peer.emitDataChannel(channel);
  await harness.controlAndWait("start");

  const track = new harness.FakeTrack("remote-audio");
  const receiver = new harness.FakeReceiver(track);
  receiver.createEncodedStreams();
  harness.pushEncodedFrame();
  const oldDecoder = harness.decoders[0];

  await channel.emit({
    users: [{
      deviceId: "devices/remote",
      displayName: "Remote",
      status: 1,
      isCurrentUser: false,
    }],
    deviceOutputs: [{
      deviceId: "devices/remote",
      streamId: "4242",
      outputType: 1,
      disabled: false,
    }],
  });
  await harness.flush();
  oldDecoder.emitNext();
  await harness.flush();
  assert.equal(oldDecoder.state, "closed");

  harness.pushEncodedFrame();
  assert.equal(harness.decoders.length, 2);
  const newDecoder = harness.decoders[1];
  assert.equal(newDecoder.state, "configured");
  assert.equal(newDecoder.pending.length, 1);

  oldDecoder.emit(0);
  newDecoder.emitNext();
  await harness.flush();
  assert.equal(newDecoder.state, "configured", "an old decoder callback cannot consume the new epoch");
});

test("a replacement Meet track with the same ID supersedes the old object safely", async () => {
  const harness = createHarness({ withPeerConnection: true });
  const peer = new harness.window.RTCPeerConnection();
  const firstTrack = new harness.FakeTrack("pooled-remote-lane");
  const secondTrack = new harness.FakeTrack("pooled-remote-lane");

  peer.emitTrack(firstTrack, new harness.FakeReceiver(firstTrack));
  await harness.flush();
  assert.equal(harness.trackProcessors.length, 1);

  peer.emitTrack(secondTrack, new harness.FakeReceiver(secondTrack));
  await harness.flush();
  assert.equal(harness.trackProcessors.length, 2);
  assert.equal(harness.trackProcessors[0].reader.cancelled, true);
  assert.equal(harness.trackProcessors[1].reader.cancelled, false);

  firstTrack.end();
  await harness.flush();
  assert.equal(
    harness.trackProcessors[1].reader.cancelled,
    false,
    "the ended event from the old object must not release its replacement",
  );
});

test("a reused RTCRtpReceiver can decode again after its track object changes", async () => {
  const harness = createHarness({ withPeerConnection: true });
  const peer = new harness.window.RTCPeerConnection();
  await harness.controlAndWait("start");

  const firstTrack = new harness.FakeTrack("pooled-remote-lane");
  const receiver = new harness.FakeReceiver(firstTrack);
  peer.emitTrack(firstTrack, receiver);
  receiver.createEncodedStreams();
  harness.pushEncodedFrame({}, 0);
  harness.setNow(750);
  harness.pushEncodedFrame({}, 0);
  harness.setNow(1_500);
  harness.pushEncodedFrame({}, 0);
  harness.setNow(1_520);
  harness.pushEncodedFrame({}, 0);
  const firstDecoder = harness.decoders[0];

  const secondTrack = new harness.FakeTrack("pooled-remote-lane");
  receiver.track = secondTrack;
  peer.emitTrack(secondTrack, receiver);
  await harness.flush();
  assert.equal(firstDecoder.state, "closed");

  harness.pushEncodedFrame({}, 0);
  harness.setNow(2_250);
  harness.pushEncodedFrame({}, 0);
  harness.setNow(3_000);
  harness.pushEncodedFrame({}, 0);
  harness.setNow(3_020);
  harness.pushEncodedFrame({}, 0);
  assert.equal(harness.decoders.length, 2);
  assert.equal(harness.decoders[1].state, "configured");
});

test("a superseded receiver cannot recreate a duplicate tap for the replacement track ID", async () => {
  const harness = createHarness({ withPeerConnection: true });
  const peer = new harness.window.RTCPeerConnection();
  await harness.controlAndWait("start");

  const firstTrack = new harness.FakeTrack("pooled-remote-lane");
  const firstReceiver = new harness.FakeReceiver(firstTrack);
  peer.emitTrack(firstTrack, firstReceiver);
  firstReceiver.createEncodedStreams();
  harness.pushEncodedFrame({}, 0);
  harness.setNow(750);
  harness.pushEncodedFrame({}, 0);
  harness.setNow(1_500);
  harness.pushEncodedFrame({}, 0);
  harness.setNow(1_520);
  harness.pushEncodedFrame({}, 0);
  const firstDecoder = harness.decoders[0];

  const secondTrack = new harness.FakeTrack("pooled-remote-lane");
  const secondReceiver = new harness.FakeReceiver(secondTrack);
  peer.emitTrack(secondTrack, secondReceiver);
  secondReceiver.createEncodedStreams();
  await harness.flush();
  assert.equal(firstDecoder.state, "closed");

  harness.pushEncodedFrame({}, 0);
  assert.equal(harness.decoders.length, 1, "the old transform stays ignored");
  harness.pushEncodedFrame({}, 1);
  harness.setNow(2_250);
  harness.pushEncodedFrame({}, 1);
  harness.setNow(3_000);
  harness.pushEncodedFrame({}, 1);
  harness.setNow(3_020);
  harness.pushEncodedFrame({}, 1);
  assert.equal(harness.decoders.length, 2);
  assert.equal(harness.decoders[1].state, "configured");
});

test("stop and a queued restart cannot let an old context clobber the new capture", async () => {
  const harness = createHarness({ deferAudioContextClose: true });
  await harness.controlAndWait("start");
  const receiver = new harness.FakeReceiver(new harness.FakeTrack("remote-audio"));
  receiver.createEncodedStreams();
  harness.pushEncodedFrame();
  const oldDecoder = harness.decoders[0];

  const stopped = harness.controlAndWait("stop");
  const restarted = harness.controlAndWait("start");
  await harness.waitForAudioContextClose();
  assert.equal(oldDecoder.state, "closed");
  harness.pushEncodedFrame();
  assert.equal(harness.decoders.length, 1, "capture is gated while the old context closes");

  harness.resolveAudioContextClose();
  await stopped;
  await restarted;
  harness.pushEncodedFrame();
  assert.equal(harness.decoders.length, 2);
  assert.equal(harness.decoders[1].state, "configured");
});

test("stop cancels a pending microphone start and closes the late stream", async () => {
  const harness = createHarness({ deferMicrophone: true, topLevel: true });
  harness.sendControl("start");
  await harness.waitForMicrophoneRequest();

  const stopped = harness.controlAndWait("stop");
  await stopped;
  assert.equal(
    harness.posts.some((message) => message.type === "capture-state" && message.state === "capturing"),
    false,
  );

  const microphoneTrack = new harness.FakeTrack("late-local-mic");
  harness.resolveMicrophone({
    getAudioTracks: () => [microphoneTrack],
    getTracks: () => [microphoneTrack],
  });
  await harness.flush();

  assert.equal(microphoneTrack.readyState, "ended");
});

test("a cancelled exact-device request never falls through to a second microphone prompt", async () => {
  const harness = createHarness({
    deferMicrophone: true,
    topLevel: true,
    withPeerConnection: true,
  });
  const peer = new harness.window.RTCPeerConnection();
  const senderTrack = new harness.FakeTrack("meet-microphone");
  senderTrack.label = "Microphone";
  senderTrack.getSettings = () => ({ deviceId: "selected-device" });
  peer.senders.push({
    track: senderTrack,
    getParameters: () => ({ encodings: [{ active: true }] }),
  });

  harness.sendControl("start");
  await harness.waitForMicrophoneRequest();
  await harness.controlAndWait("stop");
  harness.rejectMicrophone();
  await harness.flush();

  assert.equal(harness.microphoneRequestCount, 1);
});

test("stop and immediate restart reuse one pending microphone request", async () => {
  const harness = createHarness({ deferMicrophone: true, topLevel: true });
  harness.sendControl("start");
  await harness.waitForMicrophoneRequest();

  const stopped = harness.controlAndWait("stop");
  const restarted = harness.controlAndWait("start");
  await stopped;
  await harness.flush();
  assert.equal(harness.microphoneRequestCount, 1, "restart must not open a duplicate browser prompt");

  const stream = harness.resolveMicrophone();
  await restarted;
  assert.equal(harness.microphoneRequestCount, 1);
  assert.equal(stream.getTracks()[0].readyState, "live", "the restarted capture claims the late stream");
});

test("a failed audio graph performs full cleanup before reporting idle", async () => {
  const harness = createHarness({ rejectAudioWorklet: true });
  const idle = harness.waitForCaptureState("idle");
  harness.sendControl("start");
  await idle;
  await harness.flush();

  assert.equal(harness.audioContexts.length, 1);
  assert.equal(harness.audioContexts[0].state, "closed");
  assert.equal(
    harness.posts.some((message) => message.type === "capture-state" && message.state === "capturing"),
    false,
  );
  assert.equal(
    harness.posts.some((message) => message.kind === "warning" && message.detail?.code === "capture-start-failed"),
    true,
  );
});

test("a synchronous audioWorklet.addModule throw still closes its AudioContext", async () => {
  const harness = createHarness({ throwAudioWorklet: true });
  const idle = harness.waitForCaptureState("idle");
  harness.sendControl("start");
  await idle;
  await harness.flush();

  assert.equal(harness.audioContexts.length, 1);
  assert.equal(harness.audioContexts[0].state, "closed");
  assert.equal(
    harness.posts.some((message) => message.type === "capture-state" && message.state === "capturing"),
    false,
  );
  assert.equal(
    harness.posts.some((message) => message.kind === "warning" && message.detail?.code === "capture-start-failed"),
    true,
  );
});
