/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */
class KualiPcmProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buffer = new Float32Array(2048);
    this.offset = 0;
    this.peak = 0;
  }

  process(inputs) {
    const input = inputs[0]?.[0];
    if (!input) return true;
    let at = 0;
    while (at < input.length) {
      const count = Math.min(input.length - at, this.buffer.length - this.offset);
      this.buffer.set(input.subarray(at, at + count), this.offset);
      for (let i = at; i < at + count; i++) this.peak = Math.max(this.peak, Math.abs(input[i]));
      this.offset += count;
      at += count;
      if (this.offset === this.buffer.length) {
        if (this.peak >= 0.0005) this.port.postMessage(this.buffer.buffer, [this.buffer.buffer]);
        this.buffer = new Float32Array(2048);
        this.offset = 0;
        this.peak = 0;
      }
    }
    return true;
  }
}

registerProcessor("kuali-pcm", KualiPcmProcessor);
