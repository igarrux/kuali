/*
 * Copyright 2026 Kuali contributors
 * SPDX-License-Identifier: Apache-2.0
 * Mixed-tab fallback adapted from Vexa's Apache-2.0 offscreen capture path.
 */

const TARGET_RATE = 16000;
let tabId = null;
let stream = null;
let captureContext = null;
let playbackContext = null;
let processor = null;

async function stopTab() {
  if (processor) processor.onaudioprocess = null;
  for (const track of stream?.getTracks?.() || []) track.stop();
  stream = null;
  await captureContext?.close?.().catch(() => {});
  await playbackContext?.close?.().catch(() => {});
  captureContext = null;
  playbackContext = null;
  processor = null;
  tabId = null;
}

async function startTab(streamId, sourceTabId) {
  await stopTab();
  tabId = sourceTabId;
  stream = await navigator.mediaDevices.getUserMedia({
    audio: {
      mandatory: {
        chromeMediaSource: "tab",
        chromeMediaSourceId: streamId,
      },
    },
    video: false,
  });

  // tabCapture mutes the original tab. A second graph at the native sample rate
  // plays it back so the user can continue hearing the meeting.
  playbackContext = new AudioContext({ latencyHint: "interactive" });
  playbackContext.createMediaStreamSource(stream).connect(playbackContext.destination);

  captureContext = new AudioContext({ sampleRate: TARGET_RATE, latencyHint: "interactive" });
  const source = captureContext.createMediaStreamSource(stream);
  processor = captureContext.createScriptProcessor(2048, 1, 1);
  source.connect(processor);
  // ScriptProcessor leaves its output buffer silent. A direct connection keeps
  // Chrome pulling tab audio without replaying it a second time; a zero-gain
  // node can be optimized away on some remote WebRTC paths.
  processor.connect(captureContext.destination);
  processor.onaudioprocess = (event) => {
    const samples = event.inputBuffer.getChannelData(0);
    let peak = 0;
    for (let i = 0; i < samples.length; i++) peak = Math.max(peak, Math.abs(samples[i]));
    if (peak < 0.0005 || tabId == null) return;
    chrome.runtime.sendMessage({
      type: "mixed-audio",
      tabId,
      ts: Date.now(),
      pcm: Array.from(samples),
    }).catch(() => {});
  };
  await captureContext.resume();
  playbackContext.resume().catch(() => {});
}

chrome.runtime.onMessage.addListener((message, _sender, reply) => {
  if (message.type === "mixed-capture-start") {
    startTab(message.streamId, message.tabId)
      .then(() => reply({ ok: true }))
      .catch((error) => reply({ ok: false, error: String(error?.message || error) }));
    return true;
  }
  if (message.type === "mixed-capture-stop") {
    stopTab().then(() => reply({ ok: true }));
    return true;
  }
  return undefined;
});
