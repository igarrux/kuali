import test from "node:test";
import assert from "node:assert/strict";

import "../src/meet-protocol.js";

function concat(...parts) {
  const size = parts.reduce((sum, part) => sum + part.length, 0);
  const result = new Uint8Array(size);
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }
  return result;
}

function varint(value) {
  const bytes = [];
  let remaining = value;
  do {
    let byte = remaining & 0x7f;
    remaining = Math.floor(remaining / 128);
    if (remaining) byte |= 0x80;
    bytes.push(byte);
  } while (remaining);
  return Uint8Array.from(bytes);
}

function bytesField(field, value) {
  const bytes = value instanceof Uint8Array ? value : new TextEncoder().encode(value);
  return concat(varint((field << 3) | 2), varint(bytes.length), bytes);
}

function numberField(field, value) {
  return concat(varint(field << 3), varint(value));
}

function fixtureUser() {
  return concat(
    bytesField(1, "devices/alice"),
    bytesField(2, "Alice Example"),
    bytesField(3, "https://example.test/alice.png"),
    numberField(4, 1),
    bytesField(29, "Alice"),
    numberField(34, 1),
  );
}

function collectionFixture() {
  const userList = bytesField(2, fixtureUser());
  const combined = bytesField(1, userList);
  const status = numberField(1, 0);
  const output = concat(
    numberField(2, 1),
    bytesField(4, "4294967001"),
    bytesField(6, "devices/alice"),
    bytesField(10, status),
  );
  const deviceInfo = bytesField(2, output);
  const collectionBody = concat(bytesField(3, deviceInfo), bytesField(13, combined));
  return bytesField(1, bytesField(2, collectionBody));
}

test("Meet collections decode participant identity and audio CSRC mapping", () => {
  const decoded = globalThis.KualiMeetProtocol.decodeCollectionBytes(collectionFixture());
  assert.deepEqual(decoded.users, [{
    deviceId: "devices/alice",
    fullName: "Alice Example",
    displayName: "Alice",
    profilePicture: "https://example.test/alice.png",
    status: 1,
    parentDeviceId: null,
    isCurrentUser: false,
    isHost: true,
  }]);
  assert.deepEqual(decoded.deviceOutputs, [{
    deviceId: "devices/alice",
    streamId: "4294967001",
    outputType: 1,
    disabled: false,
  }]);
});

test("Meet initial sync roster uses the same participant representation", () => {
  const userList = bytesField(2, fixtureUser());
  const response = bytesField(2, bytesField(2, userList));
  const encoded = Buffer.from(response).toString("base64");
  const [user] = globalThis.KualiMeetProtocol.decodeSyncUsersBase64(encoded);
  assert.equal(user.deviceId, "devices/alice");
  assert.equal(user.displayName, "Alice");
  assert.equal(user.status, 1);
});
