/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */

/**
 * Only the top document owns the authoritative Meet roster. Once it has seen
 * the current user, that user's persistent disappearance means the call UI was
 * left; the background worker adds a short grace period before stopping.
 */
export function meetingPresence(frameId, hadSelf, participants) {
  if (frameId !== 0 || !Array.isArray(participants)) return null;
  const selfPresent = participants.some((participant) => participant?.isSelf);
  return {
    selfPresent,
    hadSelf: hadSelf || selfPresent,
    shouldScheduleStop: hadSelf && !selfPresent,
  };
}
