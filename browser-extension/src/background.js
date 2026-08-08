/*
 * Copyright 2026 Kuali contributors
 * SPDX-License-Identifier: Apache-2.0
 * Kuali implementation of the Vexa-derived capture.v1 wire contract.
 */
import { encodeAudio, encodeMeetingEvent, mapFrameChannel } from "./protocol.js";
import { healthUrl, isKualiHealthMessage } from "./health.js";
import { meetingPresence } from "./lifecycle.js";

const DEFAULT_PORT = 9099;
const HEALTH_TIMEOUT_MS = 900;
const HEALTH_CACHE_MS = 3_000;
const MEETING_END_GRACE_MS = 3_500;
const sessions = new Map();
let healthCache = { port: null, checkedAt: 0, available: false, pending: null };
const translated = (key, fallback, substitutions) => (
  chrome.i18n?.getMessage(key, substitutions) || fallback
);

function stateFor(tabId) {
  let state = sessions.get(tabId);
  if (!state) {
    state = {
      status: "idle",
      socket: null,
      keepAlive: null,
      info: null,
      error: null,
      frames: new Map([[0, 0]]),
      nextFrameSlot: 1,
      channels: new Map(),
      tracks: new Map(),
      participantCount: 0,
      participantCountsByFrame: new Map(),
      selfPresent: false,
      hadSelf: false,
      meetingEndTimer: null,
      fallbackStreamId: null,
      fallbackPending: [],
      fallbackTimer: null,
      fallbackPromoted: false,
    };
    sessions.set(tabId, state);
  }
  return state;
}

function cancelAutomaticStop(state) {
  clearTimeout(state.meetingEndTimer);
  state.meetingEndTimer = null;
}

function scheduleAutomaticStop(tabId, state) {
  if (state.meetingEndTimer || !state.hadSelf || state.selfPresent) return;
  state.meetingEndTimer = setTimeout(() => {
    state.meetingEndTimer = null;
    if (state.status === "capturing" && state.hadSelf && !state.selfPresent) {
      stop(tabId);
    }
  }, MEETING_END_GRACE_MS);
}

function needsMixedFallback(platform) {
  return platform === "zoom" || platform === "microsoft_teams";
}

function mintTabStream(tabId) {
  return new Promise((resolve) => {
    chrome.tabCapture.getMediaStreamId({ targetTabId: tabId }, (streamId) => {
      resolve(chrome.runtime.lastError ? null : streamId || null);
    });
  });
}

async function ensureOffscreen() {
  const url = chrome.runtime.getURL("offscreen.html");
  const contexts = await chrome.runtime.getContexts({
    contextTypes: ["OFFSCREEN_DOCUMENT"],
    documentUrls: [url],
  });
  if (contexts.length) return;
  await chrome.offscreen.createDocument({
    url: "offscreen.html",
    reasons: ["USER_MEDIA", "AUDIO_PLAYBACK"],
    justification: "Capture and replay the mixed meeting tab when no individual WebRTC tracks are exposed",
  });
}

async function startMixedFallback(tabId, state) {
  if (!state.fallbackStreamId) return;
  try {
    await ensureOffscreen();
    const result = await chrome.runtime.sendMessage({
      type: "mixed-capture-start",
      tabId,
      streamId: state.fallbackStreamId,
    });
    if (result?.ok === false) throw new Error(result.error);
  } catch (error) {
    const detail = String(error?.message || error);
    state.error = translated("mixedCaptureError", `Could not capture mixed audio: ${detail}`, [detail]);
    publish(tabId);
  }
}

function stopMixedFallback(state) {
  clearTimeout(state.fallbackTimer);
  state.fallbackTimer = null;
  state.fallbackPending.length = 0;
  state.fallbackStreamId = null;
  chrome.runtime.sendMessage({ type: "mixed-capture-stop" }).catch(() => {});
}

function promoteMixedFallback(tabId, state) {
  state.fallbackTimer = null;
  if (state.fallbackPromoted || state.status !== "capturing") return;
  const socket = state.socket;
  if (!socket || socket.readyState !== WebSocket.OPEN) return;
  state.fallbackPromoted = true;
  const detail = {
    channel: 999,
    participantId: `${state.info.platform}:mixed`,
    displayName: "Sala",
    avatarUrl: null,
    isSelf: false,
    audioKind: "mixed",
    platform: state.info.platform,
  };
  state.channels.set(999, detail);
  socket.send(encodeMeetingEvent({
    kind: "participant-upsert",
    ts: Date.now(),
    speaker: "Sala",
    detail,
  }));
  for (const frame of state.fallbackPending) {
    socket.send(encodeAudio(999, frame.ts, frame.pcm));
  }
  state.fallbackPending.length = 0;
  publish(tabId);
}

function preferPageAudio(tabId, state) {
  if (!state.fallbackStreamId && !state.fallbackPromoted) return;
  const socket = state.socket;
  if (state.fallbackPromoted && socket?.readyState === WebSocket.OPEN) {
    socket.send(encodeMeetingEvent({
      kind: "participant-left",
      ts: Date.now(),
      speaker: "Sala",
      detail: { channel: 999, participantId: `${state.info.platform}:mixed` },
    }));
    state.channels.delete(999);
  }
  state.fallbackPromoted = false;
  stopMixedFallback(state);
  publish(tabId);
}

function rememberFrame(state, frameId = 0) {
  if (!state.frames.has(frameId)) state.frames.set(frameId, state.nextFrameSlot++);
}

function wireChannel(state, frameId, localChannel) {
  rememberFrame(state, frameId);
  // Each frame receives a block of 2,048 channels. The top frame retains
  // reserved channels 999 (mixed) and 1000 (microphone) for compatibility.
  return mapFrameChannel(state.frames.get(frameId), localChannel);
}

function sendControl(tabId, state, command) {
  for (const frameId of state.frames.keys()) {
    chrome.tabs.sendMessage(
      tabId,
      { type: "capture-control", command },
      { frameId },
    ).catch(() => {});
  }
}

function publish(tabId) {
  const state = stateFor(tabId);
  const message = {
    type: "capture-status",
    tabId,
    status: state.status,
    error: state.error,
    platform: state.info?.platform,
    participantCount: state.participantCount,
    connectedTracks: state.tracks.size,
    separateChannels: [...state.channels.values()].filter((channel) => channel.audioKind !== "mixed").length,
    mixedChannels: [...state.channels.values()].filter((channel) => channel.audioKind === "mixed").length,
  };
  chrome.runtime.sendMessage(message).catch(() => {});
  chrome.tabs.sendMessage(tabId, message).catch(() => {});
  chrome.action.setBadgeBackgroundColor({ tabId, color: "#be123c" }).catch(() => {});
  chrome.action.setBadgeText({ tabId, text: state.status === "capturing" ? "REC" : "" }).catch(() => {});
  chrome.action.setTitle({
    tabId,
    title: state.status === "capturing"
      ? translated("recordingIndicator", "Kuali is recording and transcribing")
      : "Kuali",
  }).catch(() => {});
}

async function port() {
  const stored = await chrome.storage.local.get({ kualiPort: DEFAULT_PORT });
  const value = Number(stored.kualiPort);
  return Number.isInteger(value) && value > 0 && value <= 65535 ? value : DEFAULT_PORT;
}

async function kualiAvailable() {
  const wsPort = await port();
  const now = Date.now();
  if (healthCache.port === wsPort && healthCache.pending) return healthCache.pending;
  if (healthCache.port === wsPort && now - healthCache.checkedAt < HEALTH_CACHE_MS) {
    return healthCache.available;
  }

  const pending = new Promise((resolve) => {
    let settled = false;
    let socket = null;
    const finish = (available) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      healthCache = {
        port: wsPort,
        checkedAt: Date.now(),
        available,
        pending: null,
      };
      try { socket?.close(1000, "health checked"); } catch (_) {}
      resolve(available);
    };
    const timer = setTimeout(() => finish(false), HEALTH_TIMEOUT_MS);
    try {
      socket = new WebSocket(healthUrl(wsPort));
      socket.onmessage = (event) => finish(isKualiHealthMessage(event.data));
      socket.onerror = () => finish(false);
      socket.onclose = () => finish(false);
    } catch (_) {
      finish(false);
    }
  });
  healthCache = { port: wsPort, checkedAt: 0, available: false, pending };
  return pending;
}

async function start(tabId) {
  const state = stateFor(tabId);
  if (!state.info) {
    state.error = translated("unsupportedMeetingError", "This tab is not a supported meeting.");
    publish(tabId);
    return;
  }
  stop(tabId, false);
  state.hadSelf = false;
  state.selfPresent = false;
  state.error = null;
  if (needsMixedFallback(state.info.platform)) {
    // Request this while the popup click still carries user activation.
    state.fallbackStreamId = await mintTabStream(tabId);
    if (!state.fallbackStreamId) {
      state.error = translated(
        "mixedCaptureDeniedError",
        "Chrome could not prepare mixed capture. Try again from this tab.",
      );
    }
  }
  state.status = "connecting";
  publish(tabId);

  const wsPort = await port();
  const query = new URLSearchParams({
    platform: state.info.platform,
    native_meeting_id: state.info.meetingId || state.info.title || translated("meetingFallbackName", "Meeting"),
    client: "kuali-extension",
    protocol: "capture.v1+participants",
  });
  const socket = new WebSocket(`ws://127.0.0.1:${wsPort}/ingest?${query}`);
  state.socket = socket;

  socket.onopen = () => {
    if (state.socket !== socket) return socket.close();
    state.status = "waiting";
    publish(tabId);
  };
  socket.onmessage = (message) => {
    if (state.socket !== socket) return;
    try {
      const response = JSON.parse(message.data);
      if (response?.type === "error") {
        state.error = response.message || translated("captureRejectedError", "Kuali rejected this capture.");
        socket.close(1000, "capture rejected");
        publish(tabId);
        return;
      }
      if (response?.type !== "ready") return;
      state.status = "capturing";
      clearInterval(state.keepAlive);
      // Chrome 116+ preserves the service worker while its WebSocket exchanges
      // data. A meeting can easily stay silent beyond the 30-second idle limit.
      state.keepAlive = setInterval(() => {
        if (socket.readyState === WebSocket.OPEN) {
          socket.send(encodeMeetingEvent({
            kind: "keepalive",
            ts: Date.now(),
            detail: { client: "kuali-extension" },
          }));
        }
      }, 20_000);
      publish(tabId);
      sendControl(tabId, state, "start");
      if (needsMixedFallback(state.info.platform)) startMixedFallback(tabId, state);
    } catch (_) {}
  };
  socket.onerror = () => {
    if (state.socket !== socket) return;
    state.error = translated(
      "connectionError",
      "Could not connect to Kuali on this computer. Open the app and try again.",
    );
  };
  socket.onclose = () => {
    if (state.socket !== socket) return;
    clearInterval(state.keepAlive);
    state.keepAlive = null;
    state.socket = null;
    state.channels.clear();
    state.tracks.clear();
    state.participantCount = 0;
    state.participantCountsByFrame.clear();
    state.selfPresent = false;
    state.hadSelf = false;
    cancelAutomaticStop(state);
    stopMixedFallback(state);
    if (state.status !== "idle") {
      state.status = "idle";
      sendControl(tabId, state, "stop");
    }
    publish(tabId);
  };
}

function stop(tabId, notify = true) {
  const state = stateFor(tabId);
  const socket = state.socket;
  clearInterval(state.keepAlive);
  state.keepAlive = null;
  state.socket = null;
  state.status = "idle";
  state.channels.clear();
  state.tracks.clear();
  state.participantCount = 0;
  state.participantCountsByFrame.clear();
  state.selfPresent = false;
  state.hadSelf = false;
  cancelAutomaticStop(state);
  state.fallbackPromoted = false;
  stopMixedFallback(state);
  if (socket) socket.close(1000, "capture stopped");
  if (notify) sendControl(tabId, state, "stop");
  publish(tabId);
}

chrome.runtime.onMessage.addListener((message, sender, reply) => {
  // The offscreen document consumes this message. Do not leave a phantom reply
  // channel open from the service worker itself.
  if (message.type === "mixed-capture-start") return undefined;
  const tabId = message.tabId ?? sender.tab?.id;
  if (tabId == null) return;
  const state = stateFor(tabId);
  switch (message.type) {
    case "frame-ready":
      {
        const frameId = sender.frameId ?? 0;
        rememberFrame(state, frameId);
        if (message.platform) state.info = {
          platform: message.platform,
          meetingId: message.meetingId,
          title: message.title,
        };
        if (state.status === "capturing") {
          chrome.tabs.sendMessage(
            tabId,
            { type: "capture-control", command: "start" },
            { frameId },
          ).catch(() => {});
        }
        publish(tabId);
      }
      break;
    case "capture-start":
      start(tabId);
      break;
    case "capture-stop":
      stop(tabId);
      break;
    case "capture-state":
      kualiAvailable().then((available) => reply({
        status: state.status,
        error: state.error,
        platform: state.info?.platform,
        participantCount: state.participantCount,
        connectedTracks: state.tracks.size,
        separateChannels: [...state.channels.values()].filter((channel) => channel.audioKind !== "mixed").length,
        mixedChannels: [...state.channels.values()].filter((channel) => channel.audioKind === "mixed").length,
        kualiAvailable: available,
      })).catch(() => reply({
        status: state.status,
        error: state.error,
        platform: state.info?.platform,
        kualiAvailable: false,
      }));
      break;
    case "capture-event": {
      const socket = state.socket;
      if (!socket || socket.readyState !== WebSocket.OPEN || state.status !== "capturing") break;
      const event = message.event;
      const frameId = sender.frameId ?? 0;
      if (event.type === "audio" && Number.isInteger(event.channel) && Array.isArray(event.pcm)) {
        socket.send(encodeAudio(wireChannel(state, frameId, event.channel), event.ts || Date.now(), event.pcm));
      } else if (event.type === "meeting-event") {
        const detail = event.detail ? { ...event.detail } : null;
        if (detail && Number.isInteger(detail.channel)) {
          detail.channel = wireChannel(state, frameId, detail.channel);
        }
        if (detail && Number.isInteger(detail.index)) {
          detail.index = wireChannel(state, frameId, detail.index);
        }
        if (event.kind === "track-connected" && detail && Number.isInteger(detail.channel)) {
          // Transport lanes are not people. Meet currently exposes a reusable
          // three-lane remote pool even when only one remote participant exists.
          state.tracks.set(detail.channel, detail);
          publish(tabId);
        } else if (event.kind === "participant-upsert" && detail && Number.isInteger(detail.channel)) {
          state.channels.set(detail.channel, detail);
          if (!detail.isSelf) preferPageAudio(tabId, state);
          publish(tabId);
        } else if (event.kind === "participant-left" && detail && Number.isInteger(detail.channel)) {
          state.channels.delete(detail.channel);
          state.tracks.delete(detail.channel);
          publish(tabId);
        } else if (event.kind === "roster-state" && detail) {
          if (Number.isInteger(detail.participantCount)) {
            // Meet runs Kuali in the top document and a few child frames. A
            // child may only see the current user and must not overwrite the
            // complete four-person roster seen by the top document.
            state.participantCountsByFrame.set(frameId, Math.max(0, detail.participantCount));
            state.participantCount = Math.max(0, ...state.participantCountsByFrame.values());
          }
          const presenceParticipants = detail.inCall === false
            ? []
            : typeof detail.selfPresentInDom === "boolean"
              ? (detail.selfPresentInDom ? [{ isSelf: true }] : [])
            : detail.participants;
          const presence = meetingPresence(frameId, state.hadSelf, presenceParticipants);
          if (presence) {
            state.selfPresent = presence.selfPresent;
            state.hadSelf = presence.hadSelf;
            if (presence.selfPresent) {
              cancelAutomaticStop(state);
            } else if (presence.shouldScheduleStop) {
              scheduleAutomaticStop(tabId, state);
            }
          }
          publish(tabId);
        }
        socket.send(encodeMeetingEvent({ ...event, detail }));
      }
      break;
    }
    case "mixed-audio": {
      const socket = state.socket;
      if (!socket || socket.readyState !== WebSocket.OPEN || state.status !== "capturing") break;
      if (state.fallbackPromoted) {
        socket.send(encodeAudio(999, message.ts || Date.now(), message.pcm || []));
        break;
      }
      state.fallbackPending.push({ ts: message.ts || Date.now(), pcm: message.pcm || [] });
      if (state.fallbackPending.length > 40) state.fallbackPending.shift();
      if (!state.fallbackTimer) {
        state.fallbackTimer = setTimeout(() => promoteMixedFallback(tabId, state), 1800);
      }
      break;
    }
  }
  return true;
});

chrome.tabs.onRemoved.addListener((tabId) => {
  if (sessions.has(tabId)) stop(tabId, false);
  sessions.delete(tabId);
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (!changeInfo.url || !sessions.has(tabId)) return;
  const state = stateFor(tabId);
  if (!["connecting", "waiting", "capturing"].includes(state.status)) return;
  try {
    const next = new URL(changeInfo.url);
    const stillOnPlatform = state.info?.platform === "google_meet"
      ? next.hostname === "meet.google.com"
      : state.info?.platform === "zoom"
        ? next.hostname === "zoom.us" || next.hostname.endsWith(".zoom.us")
        : next.hostname === "teams.microsoft.com";
    if (!stillOnPlatform) stop(tabId, false);
  } catch (_) {
    stop(tabId, false);
  }
});
