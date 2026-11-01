/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */
(() => {
  // Server VAD turns a remote speaker on about this long after speech starts.
  const VAD_ON_LATENCY_MS = 350;
  // Server VAD hangover: the flag turns off about this long after speech ends.
  const VAD_OFF_LATENCY_MS = 1_500;
  // Slack for the shortest measured hangover (1.45 s) and timer jitter.
  const VAD_OFF_MARGIN_MS = 250;
  // VAD edges older than this cannot affect audio still held for routing.
  const VAD_HISTORY_MS = 30_000;
  const VAD_HISTORY_MAX_EDGES = 64;
  // Cheap prefilters: SDP and telemetry frames are never parsed.
  const INCOMING_TYPES = /"(?:updateDescription|upsertDescription|removeDescription|slotsConfig|vadActivity)"\s*:/;
  const OUTGOING_TYPES = /"(?:hello|updateMe)"\s*:/;
  const PEER_STATES_PATH = /\/telemost_front\/v\d+\/telemost\/conferences\/[^?#]+\/request-states(?:[?#]|$)/;

  function createState() {
    return {
      selfId: "",
      selfName: "",
      selfMuted: null,
      selfSpeaking: false,
      // A goloom session is open between the
      // client's hello and the socket close.
      sessionOpen: false,
      // True once the server sent its roster snapshot (updateDescription).
      rosterKnown: false,
      // Remote participants by goloom participantId: { name, muted, hidden }.
      participants: new Map(),
      // REST request-states profiles by
      // peer_id (== participantId): { name, avatarUrl }.
      profiles: new Map(),
      // Remote VAD intervals by participantId:
      // [{ on, off, cut }], off is null while speaking.
      vad: new Map(),
    };
  }

  function emptyChange() {
    return { roster: false, vad: false, self: false, left: [] };
  }

  function isObject(value) {
    return !!value && typeof value === "object" && !Array.isArray(value);
  }

  function text(value) {
    return typeof value === "string" ? value.replace(/\s+/g, " ").trim() : "";
  }

  function safeUrl(value) {
    const url = text(value);
    return /^https:\/\//i.test(url) ? url : "";
  }

  function parse(message, filter) {
    if (typeof message === "string") {
      if (!filter.test(message)) return null;
      try {
        const value = JSON.parse(message);
        return isObject(value) ? value : null;
      } catch (_) {
        return null;
      }
    }
    return isObject(message) ? message : null;
  }

  // `cut`: the turn ended by leaving or vanishing
  // from the slots, so no VAD hangover applies.
  function closeVad(state, id, nowMs, change, cut) {
    const last = state.vad.get(id)?.at(-1);
    if (last && last.off === null) {
      last.off = nowMs;
      last.cut = cut;
      change.vad = true;
    }
  }

  function removeParticipant(state, id, nowMs, change) {
    const known = state.participants.delete(id);
    if (known) change.roster = true;
    if (known || state.vad.has(id)) change.left.push(id);
    closeVad(state, id, nowMs, change, true);
  }

  function applySelf(state, update, change) {
    const meta = isObject(update.participantMeta) ? update.participantMeta : {};
    const name = text(meta.name) || text(update.participantAttributes?.name);
    if (name && name !== state.selfName) {
      state.selfName = name;
      change.self = true;
    }
    const sendAudio = typeof update.sendAudio === "boolean" ? update.sendAudio : meta.sendAudio;
    if (typeof sendAudio === "boolean" && state.selfMuted !== !sendAudio) {
      state.selfMuted = !sendAudio;
      change.self = true;
    }
  }

  function applyHello(state, hello, change) {
    const id = text(hello.participantId);
    if (id && id !== state.selfId) {
      state.selfId = id;
      change.self = true;
    }
    if (id && state.participants.delete(id)) change.roster = true;
    if (!state.sessionOpen) {
      state.sessionOpen = true;
      change.roster = true;
    }
    if (state.selfSpeaking) {
      state.selfSpeaking = false;
      change.vad = true;
    }
    applySelf(state, hello, change);
  }

  function applyDescriptions(state, list, snapshot, nowMs, change) {
    if (!Array.isArray(list)) return;
    const seen = new Set();
    for (const description of list) {
      if (!isObject(description)) continue;
      const id = text(description.id);
      if (!id) continue;
      seen.add(id);
      // Self state comes only from the client's own hello/updateMe.
      if (id === state.selfId) continue;
      const meta = isObject(description.meta) ? description.meta : {};
      const previous = state.participants.get(id);
      const sendAudio = typeof description.sendAudio === "boolean" ? description.sendAudio : meta.sendAudio;
      const next = {
        name: text(meta.name) || text(description.participantAttributes?.name) || previous?.name || "",
        muted: typeof sendAudio === "boolean" ? !sendAudio : (previous?.muted ?? null),
        hidden: description.hideFromParticipantsList === true,
      };
      if (previous?.name === next.name && previous?.muted === next.muted && previous?.hidden === next.hidden) continue;
      state.participants.set(id, next);
      change.roster = true;
    }
    if (!snapshot) return;
    for (const id of [...state.participants.keys()]) {
      if (!seen.has(id)) removeParticipant(state, id, nowMs, change);
    }
    if (!state.rosterKnown) {
      state.rosterKnown = true;
      change.roster = true;
    }
  }

  function pruneVad(state, nowMs) {
    for (const [id, intervals] of state.vad) {
      while (intervals.length && intervals[0].off !== null && intervals[0].off < nowMs - VAD_HISTORY_MS) intervals.shift();
      if (intervals.length > VAD_HISTORY_MAX_EDGES) intervals.splice(0, intervals.length - VAD_HISTORY_MAX_EDGES);
      if (!intervals.length && !state.participants.has(id)) state.vad.delete(id);
    }
  }

  function applySlots(state, slots, nowMs, change) {
    if (!Array.isArray(slots)) return;
    // Every VAD change resends the whole slot
    // list; unlisted speakers are silent.
    const listed = new Set();
    const speaking = new Set();
    for (const slot of slots) {
      if (!isObject(slot)) continue;
      const id = text(slot.participant?.participantId) || text(slot.participantVideoByMid?.participantId);
      if (!id || id === state.selfId) continue;
      listed.add(id);
      if (slot.vad === true) speaking.add(id);
    }
    for (const id of state.vad.keys()) {
      if (!speaking.has(id)) closeVad(state, id, nowMs, change, !listed.has(id));
    }
    for (const id of speaking) {
      let intervals = state.vad.get(id);
      if (!intervals) {
        intervals = [];
        state.vad.set(id, intervals);
      }
      const last = intervals.at(-1);
      if (last && last.off === null) continue;
      intervals.push({ on: nowMs, off: null, cut: false });
      change.vad = true;
    }
    pruneVad(state, nowMs);
  }

  /** Server-to-client goloom frame (string or parsed object). */
  function ingestIncoming(state, message, nowMs = Date.now()) {
    const change = emptyChange();
    try {
      const frame = parse(message, INCOMING_TYPES);
      if (!frame) return change;
      if (isObject(frame.updateDescription)) {
        applyDescriptions(state, frame.updateDescription.description, true, nowMs, change);
      }
      if (isObject(frame.upsertDescription)) {
        applyDescriptions(state, frame.upsertDescription.description, false, nowMs, change);
      }
      if (isObject(frame.removeDescription) && Array.isArray(frame.removeDescription.descriptionId)) {
        for (const value of frame.removeDescription.descriptionId) {
          const id = text(value);
          if (id && id !== state.selfId) removeParticipant(state, id, nowMs, change);
        }
      }
      if (isObject(frame.slotsConfig)) applySlots(state, frame.slotsConfig.slots, nowMs, change);
      if (isObject(frame.vadActivity)) {
        const active = frame.vadActivity.active === true;
        if (active !== state.selfSpeaking) {
          state.selfSpeaking = active;
          change.vad = true;
        }
      }
    } catch (_) {}
    if (change.self) change.roster = true;
    return change;
  }

  /**
   * Client-to-server goloom frame: hello
   * names self, updateMe carries self mute.
   */
  function ingestOutgoing(state, message, nowMs = Date.now()) {
    const change = emptyChange();
    try {
      const frame = parse(message, OUTGOING_TYPES);
      if (!frame) return change;
      if (isObject(frame.hello)) applyHello(state, frame.hello, change);
      if (isObject(frame.updateMe)) applySelf(state, frame.updateMe, change);
    } catch (_) {}
    if (change.self) change.roster = true;
    return change;
  }

  /** REST request-states response: display names and avatars by peer_id. */
  function ingestPeerStates(state, response) {
    const change = emptyChange();
    try {
      const value = typeof response === "string" ? JSON.parse(response) : response;
      if (!isObject(value) || !Array.isArray(value.peers)) return change;
      for (const peer of value.peers) {
        const id = text(peer?.peer_id);
        const data = peer?.state?.user_data;
        if (!id || !isObject(data)) continue;
        const next = {
          name: text(data.display_name),
          avatarUrl: (data.is_default_avatar === true ? "" : safeUrl(data.avatar_url)) || null,
        };
        const previous = state.profiles.get(id);
        if (previous?.name === next.name && previous?.avatarUrl === next.avatarUrl) continue;
        state.profiles.set(id, next);
        change.roster = true;
        if (id === state.selfId) change.self = true;
      }
    } catch (_) {}
    return change;
  }

  function closeSession(state, nowMs = Date.now()) {
    const change = emptyChange();
    for (const id of state.vad.keys()) closeVad(state, id, nowMs, change, true);
    if (state.selfSpeaking) {
      state.selfSpeaking = false;
      change.vad = true;
    }
    if (state.sessionOpen) {
      state.sessionOpen = false;
      change.roster = true;
    }
    return change;
  }

  /**
   * Remote participant audible in the server mix at `timestampMs`, compensating
   * the VAD on latency; the off hangover is subtracted only once the off edge
   * is known, so audio routed earlier stays with a still-flagged
   * speaker. Overlap goes to the most recent "on" edge.
   */
  function speakerAt(state, timestampMs) {
    const t = Number(timestampMs);
    if (!Number.isFinite(t)) return null;
    let speaker = null;
    let latestOn = Number.NEGATIVE_INFINITY;
    for (const [id, intervals] of state.vad) {
      if (id === state.selfId) continue;
      for (let index = intervals.length - 1; index >= 0; index -= 1) {
        const { on, off, cut } = intervals[index];
        if (on > t + VAD_ON_LATENCY_MS) continue;
        const end = off === null
          ? Number.POSITIVE_INFINITY
          : (cut ? off : off - VAD_OFF_LATENCY_MS + VAD_OFF_MARGIN_MS);
        if (t >= end) continue;
        if (on > latestOn) {
          speaker = id;
          latestOn = on;
        }
        break;
      }
    }
    return speaker;
  }

  function selfIdentity(state) {
    const profile = state.profiles.get(state.selfId);
    return {
      participantId: state.selfId || null,
      displayName: state.selfName || profile?.name || "",
      avatarUrl: profile?.avatarUrl || null,
      isSelf: true,
      muted: state.selfMuted,
    };
  }

  function identity(state, id) {
    const key = text(id);
    if (!key) return null;
    if (key === state.selfId) return selfIdentity(state);
    const person = state.participants.get(key);
    const profile = state.profiles.get(key);
    return {
      participantId: key,
      displayName: person?.name || profile?.name || "",
      avatarUrl: profile?.avatarUrl || null,
      isSelf: false,
      muted: person?.muted ?? null,
    };
  }

  function roster(state) {
    const people = state.selfId ? [selfIdentity(state)] : [];
    for (const [id, person] of state.participants) {
      if (!person.hidden) people.push(identity(state, id));
    }
    return people;
  }

  function activeSpeakers(state) {
    const people = [];
    for (const [id, intervals] of state.vad) {
      if (id !== state.selfId && intervals.at(-1)?.off === null) people.push(identity(state, id));
    }
    if (state.selfSpeaking) people.push(selfIdentity(state));
    return people;
  }

  function isGoloomUrl(value) {
    try {
      const url = new URL(String(value));
      return (url.protocol === "wss:" || url.protocol === "ws:") && /(?:^|\.)goloom\./i.test(url.hostname);
    } catch (_) {
      return false;
    }
  }

  function isPeerStatesUrl(value) {
    return PEER_STATES_PATH.test(String(value || ""));
  }

  globalThis.KualiTelemostProtocol = Object.freeze({
    VAD_ON_LATENCY_MS,
    VAD_OFF_LATENCY_MS,
    VAD_OFF_MARGIN_MS,
    createState,
    ingestIncoming,
    ingestOutgoing,
    ingestPeerStates,
    closeSession,
    speakerAt,
    selfIdentity,
    identity,
    roster,
    activeSpeakers,
    isGoloomUrl,
    isPeerStatesUrl,
  });
})();
