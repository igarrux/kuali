/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */
(() => {
  const decoder = new TextDecoder();

  class ProtoReader {
    constructor(bytes) {
      this.bytes = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      this.pos = 0;
    }

    get done() {
      return this.pos >= this.bytes.length;
    }

    varint() {
      let value = 0;
      let shift = 0;
      while (this.pos < this.bytes.length && shift < 35) {
        const byte = this.bytes[this.pos++];
        value += (byte & 0x7f) * (2 ** shift);
        if ((byte & 0x80) === 0) return value;
        shift += 7;
      }
      // Values such as caption timestamps can exceed Number's exact integer
      // range. Kuali does not consume them here, but the reader still needs to
      // advance over every remaining varint byte safely.
      while (this.pos < this.bytes.length && (this.bytes[this.pos++] & 0x80) !== 0) {}
      return value;
    }

    bytesField() {
      const length = this.varint();
      const end = Math.min(this.bytes.length, this.pos + length);
      const value = this.bytes.subarray(this.pos, end);
      this.pos = end;
      return value;
    }

    string() {
      return decoder.decode(this.bytesField());
    }

    skip(wireType) {
      if (wireType === 0) {
        this.varint();
      } else if (wireType === 1) {
        this.pos = Math.min(this.bytes.length, this.pos + 8);
      } else if (wireType === 2) {
        this.bytesField();
      } else if (wireType === 5) {
        this.pos = Math.min(this.bytes.length, this.pos + 4);
      } else {
        throw new Error(`Unsupported protobuf wire type ${wireType}`);
      }
    }
  }

  function fields(bytes, visit) {
    const reader = new ProtoReader(bytes);
    while (!reader.done) {
      const tag = reader.varint();
      if (!tag) break;
      const field = tag >>> 3;
      const wire = tag & 7;
      if (!visit(field, wire, reader)) reader.skip(wire);
    }
  }

  function nested(bytes, fieldNumber) {
    let result = null;
    fields(bytes, (field, wire, reader) => {
      if (field === fieldNumber && wire === 2) {
        result = reader.bytesField();
        return true;
      }
      return false;
    });
    return result;
  }

  function decodeUser(bytes) {
    const user = {
      deviceId: "",
      fullName: "",
      displayName: "",
      profilePicture: null,
      status: 0,
      parentDeviceId: null,
      isCurrentUser: false,
      isHost: false,
    };
    fields(bytes, (field, wire, reader) => {
      if (wire === 2 && [1, 2, 3, 7, 21, 29].includes(field)) {
        const value = reader.string();
        if (field === 1) user.deviceId = value;
        else if (field === 2) user.fullName = value;
        else if (field === 3) user.profilePicture = value || null;
        else if (field === 7) user.isCurrentUser = !!value;
        else if (field === 21) user.parentDeviceId = value || null;
        else if (field === 29) user.displayName = value;
        return true;
      }
      if (wire === 0 && (field === 4 || field === 34)) {
        const value = reader.varint();
        if (field === 4) user.status = value;
        else user.isHost = value !== 0;
        return true;
      }
      return false;
    });
    return user.deviceId ? user : null;
  }

  function decodeUserListWrapper(bytes) {
    const users = [];
    fields(bytes, (field, wire, reader) => {
      if (field === 2 && wire === 2) {
        const user = decodeUser(reader.bytesField());
        if (user) users.push(user);
        return true;
      }
      return false;
    });
    return users;
  }

  function decodeDeviceOutput(bytes) {
    const output = {
      deviceId: "",
      streamId: "",
      outputType: 0,
      disabled: false,
    };
    fields(bytes, (field, wire, reader) => {
      if (wire === 0 && field === 2) {
        output.outputType = reader.varint();
        return true;
      }
      if (wire === 2 && (field === 4 || field === 6 || field === 10)) {
        if (field === 4) output.streamId = reader.string();
        else if (field === 6) output.deviceId = reader.string();
        else {
          const status = reader.bytesField();
          fields(status, (statusField, statusWire, statusReader) => {
            if (statusField === 1 && statusWire === 0) {
              output.disabled = statusReader.varint() !== 0;
              return true;
            }
            return false;
          });
        }
        return true;
      }
      return false;
    });
    return output.deviceId && output.streamId ? output : null;
  }

  function decodeCollectionBytes(bytes) {
    const root = nested(bytes, 1);
    const body = root && nested(root, 2);
    if (!body) return { users: [], deviceOutputs: [] };

    const users = [];
    const deviceOutputs = [];
    fields(body, (field, wire, reader) => {
      if (wire !== 2 || (field !== 3 && field !== 13)) return false;
      const wrapper = reader.bytesField();
      if (field === 3) {
        fields(wrapper, (deviceField, deviceWire, deviceReader) => {
          if (deviceField === 2 && deviceWire === 2) {
            const output = decodeDeviceOutput(deviceReader.bytesField());
            if (output) deviceOutputs.push(output);
            return true;
          }
          return false;
        });
      } else {
        fields(wrapper, (combinedField, combinedWire, combinedReader) => {
          if (combinedField === 1 && combinedWire === 2) {
            users.push(...decodeUserListWrapper(combinedReader.bytesField()));
            return true;
          }
          return false;
        });
      }
      return true;
    });
    return { users, deviceOutputs };
  }

  async function toBytes(data) {
    if (data instanceof Uint8Array) return data;
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof Blob) return new Uint8Array(await data.arrayBuffer());
    throw new TypeError("Unsupported Meet collections payload");
  }

  async function inflate(bytes) {
    if (typeof DecompressionStream !== "function") {
      throw new Error("This Chrome version does not expose DecompressionStream");
    }
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate"));
    return new Uint8Array(await new Response(stream).arrayBuffer());
  }

  async function decodeCollectionPacket(data) {
    const compressed = await toBytes(data);
    return decodeCollectionBytes(await inflate(compressed));
  }

  function decodeSyncUsersBase64(value) {
    const binary = atob(String(value || "").trim());
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
    const wrapper = nested(bytes, 2);
    const userList = wrapper && nested(wrapper, 2);
    return userList ? decodeUserListWrapper(userList) : [];
  }

  globalThis.KualiMeetProtocol = Object.freeze({
    decodeCollectionBytes,
    decodeCollectionPacket,
    decodeSyncUsersBase64,
  });
})();
