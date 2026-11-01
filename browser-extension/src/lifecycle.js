/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */

import { mapFrameChannel } from "./protocol.js";

/**
 * Only the primary frame owns the authoritative roster: the top document for
 * Meet, the /private-join/ iframe for Telemost. Once it has seen the current
 * user, that user's persistent disappearance means the call UI was left; the
 * background worker adds a short grace period before stopping.
 */
export function meetingPresence(frameId, hadSelf, participants, primaryFrameId = 0) {
  if (frameId !== primaryFrameId || !Array.isArray(participants)) return null;
  const selfPresent = participants.some((participant) => participant?.isSelf);
  return {
    selfPresent,
    hadSelf: hadSelf || selfPresent,
    shouldScheduleStop: hadSelf && !selfPresent,
  };
}

/**
 * Frame-slot bookkeeping for `frame-ready`. A new top document means every
 * child frame is new too; the primary frame owns slot 0 so its reserved
 * 999/1000 channels keep their meaning. `displaced` is slot 0's last owner.
 */
export function applyFrameReady(slots, frameId, { topDocument = false, primary = false } = {}) {
  if (frameId === 0 && topDocument) {
    slots.frames = new Map([[0, 0]]);
    slots.nextFrameSlot = 1;
    slots.primaryFrameId = 0;
  }
  let displaced = null;
  if (primary && slots.primaryFrameId !== frameId) {
    displaced = slots.primaryFrameId;
    slots.frames.set(displaced, slots.nextFrameSlot++);
    slots.frames.set(frameId, 0);
    slots.primaryFrameId = frameId;
  }
  if (!slots.frames.has(frameId)) slots.frames.set(frameId, slots.nextFrameSlot++);
  return { displaced };
}

/** Drop wire-channel entries that belong to one frame slot. */
export function forgetFrameSlot(entries, slot) {
  const first = mapFrameChannel(slot, 0);
  const end = mapFrameChannel(slot + 1, 0);
  for (const channel of [...entries.keys()]) {
    if (channel >= first && channel < end) entries.delete(channel);
  }
}
