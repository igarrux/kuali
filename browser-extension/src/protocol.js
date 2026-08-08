/*
 * Copyright 2026 Kuali contributors
 * SPDX-License-Identifier: Apache-2.0
 * Kuali implementation of the Vexa-derived capture.v1 wire contract.
 */

export function encodeAudio(channel, timestamp, pcm) {
  const samples = Float32Array.from(pcm);
  const out = new ArrayBuffer(12 + samples.byteLength);
  const view = new DataView(out);
  view.setUint32(0, channel >>> 0, true);
  view.setFloat64(4, timestamp, true);
  new Float32Array(out, 12).set(samples);
  return out;
}

export function encodeMeetingEvent(event) {
  return JSON.stringify({
    kind: event.kind,
    ts: event.ts || Date.now(),
    speaker: event.speaker || null,
    text: event.text || null,
    detail: event.detail || null,
  });
}

export function mapFrameChannel(frameSlot, localChannel) {
  if (!Number.isInteger(frameSlot) || frameSlot < 0) throw new RangeError("invalid frame slot");
  if (!Number.isInteger(localChannel) || localChannel < 0 || localChannel >= 2048) {
    throw new RangeError("invalid local channel");
  }
  return frameSlot * 2048 + localChannel;
}
