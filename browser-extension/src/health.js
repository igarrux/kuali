/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */

export function healthUrl(port) {
  const value = Number(port);
  if (!Number.isInteger(value) || value < 1 || value > 65535) {
    throw new RangeError("invalid Kuali port");
  }
  return `ws://127.0.0.1:${value}/health?client=kuali-extension`;
}

export function isKualiHealthMessage(data) {
  try {
    const message = typeof data === "string" ? JSON.parse(data) : data;
    return message?.type === "health"
      && message?.service === "kuali"
      && message?.status === "ready"
      && message?.protocol === "capture.v1";
  } catch {
    return false;
  }
}
