import assert from "node:assert/strict";
import test from "node:test";

import { applyFrameReady, forgetFrameSlot, meetingPresence } from "../src/lifecycle.js";
import { mapFrameChannel } from "../src/protocol.js";

const freshSlots = () => ({ frames: new Map([[0, 0]]), nextFrameSlot: 1, primaryFrameId: 0 });

test("a Telemost call iframe takes slot 0 and a later Meet page in the same tab gets it back", () => {
  const slots = freshSlots();
  assert.deepEqual(applyFrameReady(slots, 0, { topDocument: true }), { displaced: null });
  assert.deepEqual(applyFrameReady(slots, 7, { primary: true }), { displaced: 0 });
  assert.equal(slots.primaryFrameId, 7);
  assert.equal(slots.frames.get(7), 0);
  assert.equal(slots.frames.get(0), 1, "the messenger shell moves to its own block");

  // Navigating the tab to Meet is a new top
  // document: every old child frame is gone.
  assert.deepEqual(applyFrameReady(slots, 0, { topDocument: true, primary: true }), { displaced: null });
  assert.equal(slots.primaryFrameId, 0);
  assert.deepEqual([...slots.frames], [[0, 0]]);
  assert.equal(slots.nextFrameSlot, 1);
  assert.equal(mapFrameChannel(slots.frames.get(0), 999), 999);
  assert.equal(mapFrameChannel(slots.frames.get(0), 1000), 1000);
  assert.notEqual(meetingPresence(0, true, [], slots.primaryFrameId), null, "Meet auto-stop listens to the top frame again");
  assert.equal(meetingPresence(7, true, [], slots.primaryFrameId), null);
});

test("Meet child frames keep stable non-zero slots and never displace the top frame", () => {
  const slots = freshSlots();
  applyFrameReady(slots, 0, { topDocument: true, primary: true });
  assert.deepEqual(applyFrameReady(slots, 3), { displaced: null });
  assert.deepEqual(applyFrameReady(slots, 3), { displaced: null });
  assert.deepEqual(applyFrameReady(slots, 4), { displaced: null });
  assert.deepEqual([...slots.frames], [[0, 0], [3, 1], [4, 2]]);
  assert.equal(slots.primaryFrameId, 0);
});

test("a re-created Telemost call iframe replaces the stale slot-0 lanes", () => {
  const slots = freshSlots();
  applyFrameReady(slots, 0, { topDocument: true });
  applyFrameReady(slots, 7, { primary: true });
  const channels = new Map([[999, "mix"], [1000, "self"], [1001, "p1"], [mapFrameChannel(2, 5), "other frame"]]);
  const { displaced } = applyFrameReady(slots, 9, { primary: true });
  assert.equal(displaced, 7);
  assert.equal(slots.frames.get(9), 0);
  assert.notEqual(slots.frames.get(7), 0);
  forgetFrameSlot(channels, 0);
  assert.deepEqual([...channels.keys()], [mapFrameChannel(2, 5)]);
});
