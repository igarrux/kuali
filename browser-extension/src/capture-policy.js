/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */
(() => {
  const MIC_CHANNEL = 1000;
  const MEET_IDENTITY_VOTE_MAX_GAP_MS = 400;
  const MEET_IDENTITY_VOTE_MIN_SAMPLES = 6;
  const MEET_IDENTITY_VOTE_MIN_MS = 250;
  const MEET_CURRENT_SOURCE_WINDOW_MS = 50;
  const MEET_RECEIVER_LEASE_MS = 250;

  /**
   * A direct DOM/MediaStream identity belongs to that track. The microphone is
   * even stricter: its reserved channel can never be renamed from speaker-glow
   * heuristics, including while the local Meet tile is still rendering.
   */
  function shouldCorrelateIdentity(channel, directIdentity) {
    return channel !== MIC_CHANNEL && !directIdentity;
  }

  /**
   * Meet's hidden audio pool is not co-located with the participant tile that
   * owns its current track. A broad closest() lookup can therefore find the
   * local tile and incorrectly label a remote lane as "You". Remote Meet lanes
   * must be named from audio/activity correlation; the reserved microphone is
   * still given its explicit self identity by startMic().
   */
  function mediaElementIdentity(platform, identity) {
    return platform === "google_meet" ? null : identity;
  }

  function rosterDetail(roster, platform) {
    const participants = [];
    const seen = new Set();
    for (const entry of roster || []) {
      const identity = entry?.identity || entry;
      if (!identity) continue;
      const name = String(identity.name || "").trim();
      const id = String(identity.id || "").trim();
      if (!name && !id) continue;
      // The Meet scanner already collapses visual copies by its stable
      // data-participant-id. Keep names only as a fallback so two people with
      // the same display name still count separately when IDs are available.
      const key = id ? `id:${id}` : `name:${name.toLocaleLowerCase()}`;
      if (seen.has(key)) continue;
      seen.add(key);
      participants.push({
        participantId: id || null,
        displayName: name || "Participante",
        avatarUrl: identity.avatarUrl || null,
        isSelf: !!identity.isSelf,
      });
    }
    return { platform, participantCount: participants.length, participants };
  }

  function connectedTrackDetail(channel, platform) {
    return {
      channel,
      audioKind: "separate",
      platform,
      identityPending: true,
    };
  }

  /**
   * Meet occasionally exposes implementation labels such as `devices` while it
   * repaints or virtualizes participant tiles in a background tab. Those values
   * are identifiers, not human names, and must never replace a name already
   * learned from Meet's collection protocol.
   */
  function usableMeetParticipantName(value, deviceId = "") {
    const name = String(value || "").replace(/\s+/g, " ").trim();
    if (!name || name.length > 100) return "";
    const normalized = name.toLocaleLowerCase();
    const normalizedId = String(deviceId || "").trim().toLocaleLowerCase();
    if (normalizedId && normalized === normalizedId) return "";
    if (/^(?:spaces\/[^/]+\/)?devices(?:\/[^/]+)?$/i.test(name)) return "";
    if (/^(?:device|devices|participant|participants|participante|participantes|participante sin identificar|unknown participant)$/i.test(name)) return "";
    return name;
  }

  /** Keep the best metadata learned for one stable Meet device ID. */
  function mergeMeetParticipantIdentity(previous, incoming) {
    const id = String(incoming?.id || previous?.id || "").trim();
    const incomingName = usableMeetParticipantName(incoming?.name, id);
    const previousName = usableMeetParticipantName(previous?.name, id);
    return {
      ...previous,
      ...incoming,
      id,
      name: incomingName || previousName || "Participante sin identificar",
      avatarUrl: incoming?.avatarUrl || previous?.avatarUrl || null,
      isSelf: !!incoming?.isSelf || !!previous?.isSelf,
    };
  }

  /** Resolve a Meet identity with protocol metadata as the authority. */
  function meetParticipantIdentity({ deviceId = "", source = "", user = null, domIdentity = null } = {}) {
    const id = String(deviceId || domIdentity?.id || `google_meet:csrc:${source}`);
    const name = usableMeetParticipantName(user?.displayName, id)
      || usableMeetParticipantName(user?.fullName, id)
      || usableMeetParticipantName(domIdentity?.name, id)
      || "Participante sin identificar";
    return {
      id,
      name,
      avatarUrl: user?.profilePicture || domIdentity?.avatarUrl || null,
      isSelf: user ? !!user.isCurrentUser : !!domIdentity?.isSelf,
    };
  }

  /**
   * Merge Meet's complete protocol roster with visible DOM tiles. Meet only
   * renders a subset of tiles in some layouts, especially in background tabs.
   */
  function mergeMeetRoster(domRoster, users) {
    const domById = new Map(
      (domRoster || [])
        .filter((entry) => entry?.identity?.id)
        .map((entry) => [entry.identity.id, entry]),
    );
    const merged = new Map();
    const usersById = new Map(
      [...(users || [])]
        .filter((user) => user?.deviceId)
        .map((user) => [user.deviceId, user]),
    );

    for (const user of usersById.values()) {
      // Status 1 is an active participant. Screen-share devices belong to their
      // parent participant and must not inflate the headcount.
      if (user.status !== 1 || user.parentDeviceId) continue;
      const domEntry = domById.get(user.deviceId);
      merged.set(user.deviceId, {
        tile: domEntry?.tile || null,
        identity: meetParticipantIdentity({
          deviceId: user.deviceId,
          user,
          domIdentity: domEntry?.identity || null,
        }),
      });
    }

    for (const entry of domRoster || []) {
      const id = entry?.identity?.id;
      if (!id || merged.has(id)) continue;
      const known = usersById.get(id);
      if (known && (known.status !== 1 || known.parentDeviceId)) continue;
      merged.set(id, {
        ...entry,
        identity: meetParticipantIdentity({
          deviceId: id,
          user: known || null,
          domIdentity: entry.identity,
        }),
      });
    }
    return [...merged.values()];
  }

  /**
   * Meet publishes the disabled state of every audio device in its collection
   * protocol. Reading the current user's entry is safer than trusting the raw
   * MediaStreamTrack: Meet can mute the RTP sender while leaving that source
   * track live for its own level meter.
   */
  function localMeetAudioDisabled(users, outputs) {
    const self = [...(users || [])].find((user) => user?.isCurrentUser && user?.deviceId);
    if (!self) return null;
    const audio = [...(outputs || [])].filter(
      (output) => output?.deviceId === self.deviceId && output?.outputType === 1,
    );
    if (audio.length === 0) return null;
    return audio.every((output) => !!output.disabled);
  }

  /**
   * A remote participant's collection `disabled` flag is roster metadata, not
   * a capture gate. Meet can leave that flag stale across mute/unmute updates
   * even though decoded PCM and the active-speaker UI have already resumed.
   * The local microphone is gated separately from its visible control; here we
   * only reject a remote route that Meet identifies as another copy of self.
   */
  function shouldSendMeetRemoteAudio({ isSelf = false } = {}) {
    return !isSelf;
  }

  /** Keep only a contiguous run of audio/activity agreement for one device. */
  function nextMeetIdentityVote(previous, deviceId, now) {
    const continuing = previous?.deviceId === deviceId
      && Number.isFinite(previous.lastAt)
      && now >= previous.lastAt
      && now - previous.lastAt <= MEET_IDENTITY_VOTE_MAX_GAP_MS;
    return continuing
      ? { ...previous, samples: previous.samples + 1, lastAt: now }
      : { deviceId, samples: 1, firstAt: now, lastAt: now };
  }

  function meetIdentityVoteReady(vote) {
    return !!vote
      && vote.samples >= MEET_IDENTITY_VOTE_MIN_SAMPLES
      && vote.lastAt - vote.firstAt >= MEET_IDENTITY_VOTE_MIN_MS;
  }

  function meetIdentityVoteFresh(vote, now) {
    return !!vote
      && Number.isFinite(vote.lastAt)
      && now >= vote.lastAt
      && now - vote.lastAt <= MEET_IDENTITY_VOTE_MAX_GAP_MS;
  }

  /** Select only a current, clearly dominant CSRC from TrackProcessor metadata. */
  function selectMeetContributingSource(sources) {
    const participants = [...(sources || [])]
      .filter((source) => {
        const id = String(source?.source ?? "");
        return id && id !== "42";
      })
      .sort((left, right) => Number(right.timestamp || 0) - Number(left.timestamp || 0));
    const newestTimestamp = Number(participants[0]?.timestamp);
    const current = Number.isFinite(newestTimestamp) && newestTimestamp > 0
      ? participants.filter(
        (source) => newestTimestamp - Number(source.timestamp || 0) <= MEET_CURRENT_SOURCE_WINDOW_MS,
      )
      : (participants.length === 1 ? participants : []);
    current.sort((left, right) => Number(right.audioLevel || 0) - Number(left.audioLevel || 0));
    const selected = current[0];
    if (!selected) return null;
    if (current.length === 1) return selected;
    const strongestLevel = Number(selected.audioLevel);
    const secondLevel = Number(current[1]?.audioLevel);
    const clearWinner = Number.isFinite(strongestLevel)
      && Number.isFinite(secondLevel)
      && strongestLevel >= 0.01
      && strongestLevel >= secondLevel * 2;
    return clearWinner ? selected : null;
  }

  /** Lease a CSRC to one receiver while allowing handoff/reclock after a gap. */
  function updateMeetReceiverLease(previous, receiver, timestamp, now) {
    const frameTimestamp = Number(timestamp);
    if (previous) {
      const sameReceiver = previous.receiver === receiver;
      const freshLease = now >= previous.seenAt
        && now - previous.seenAt <= MEET_RECEIVER_LEASE_MS;
      if ((!sameReceiver && freshLease)
        || (sameReceiver && Number.isFinite(frameTimestamp)
          && frameTimestamp <= previous.timestamp && freshLease)) {
        return { accepted: false, lease: previous };
      }
    }
    return {
      accepted: true,
      lease: {
        receiver,
        timestamp: Number.isFinite(frameTimestamp) ? frameTimestamp : Number.NEGATIVE_INFINITY,
        seenAt: now,
      },
    };
  }

  /**
   * Activity-confirmed ownership wins while Meet keeps publishing the exact
   * protocol mapping it disproved. As soon as the protocol mapping changes, it
   * is new evidence and becomes authoritative again.
   */
  function resolveMeetRouteIdentity({
    protocolRawDeviceId = "",
    protocolDeviceId = "",
    currentDeviceId = "",
    activityDeviceId = "",
    overriddenProtocolRawDeviceId = "",
    protocolStale = false,
  } = {}) {
    const activityStillOverrides = !!activityDeviceId && (
      !protocolRawDeviceId
      || protocolStale
      || (!!overriddenProtocolRawDeviceId && protocolRawDeviceId === overriddenProtocolRawDeviceId)
    );
    if (activityStillOverrides) {
      return {
        deviceId: activityDeviceId,
        activityDeviceId,
        overriddenProtocolRawDeviceId,
      };
    }
    return {
      deviceId: protocolDeviceId || (protocolStale ? currentDeviceId : ""),
      activityDeviceId: "",
      overriddenProtocolRawDeviceId: "",
    };
  }

  /**
   * The mute button is the closest thing Meet exposes to an authoritative
   * public state. Prefer its boolean data attribute; accessible labels are the
   * fallback and describe the action that clicking the button would perform.
   */
  function meetMicrophoneMuted({ mutedAttribute = null, label = "", icon = "" } = {}) {
    const value = String(mutedAttribute ?? "").trim().toLocaleLowerCase();
    if (["true", "1"].includes(value)) return true;
    if (["false", "0"].includes(value)) return false;

    const accessible = String(label || "").replace(/\s+/g, " ").trim().toLocaleLowerCase();
    const glyph = String(icon || "").replace(/\s+/g, " ").trim().toLocaleLowerCase();
    if (/(microphone is off|microphone (?:is )?muted|micrófono (?:está )?(?:desactivado|apagado|silenciado)|microfono (?:esta )?(?:desactivado|apagado|silenciado)|microfone (?:está )?(?:desativado|silenciado))/i.test(accessible)) return true;
    if (/(microphone is on|microphone (?:is )?(?:active|unmuted)|micrófono (?:está )?(?:activado|encendido)|microfono (?:esta )?(?:activado|encendido)|microfone (?:está )?(?:ativado|ligado))/i.test(accessible)) return false;
    // Meet labels the button with its next action: “Turn off” means that the
    // microphone is currently transmitting, while “Turn on” means muted.
    if (/(turn off|\bmute\b|desactivar|apagar|silenciar|desativar)/i.test(accessible)) return false;
    if (/(turn on|\bunmute\b|\bactivar\b|encender|quitar silencio|reactivar|\bativar\b|ligar)/i.test(accessible)) return true;
    if (/\bmic_off\b/.test(glyph)) return true;
    if (/^mic$/.test(glyph)) return false;
    return null;
  }

  /**
   * The visible Meet control is the authority for the local microphone. Its
   * data-is-muted value follows the in-call button immediately, whereas the
   * source track and collection protocol may remain live for Meet's own meter.
   * Fail closed if that control cannot be read.
   */
  function shouldSendMeetMicrophone({ trackEnabled, controlMuted }) {
    return !!trackEnabled && controlMuted === false;
  }

  globalThis.KualiCapturePolicy = Object.freeze({
    MIC_CHANNEL,
    shouldCorrelateIdentity,
    mediaElementIdentity,
    rosterDetail,
    connectedTrackDetail,
    usableMeetParticipantName,
    mergeMeetParticipantIdentity,
    meetParticipantIdentity,
    mergeMeetRoster,
    localMeetAudioDisabled,
    shouldSendMeetRemoteAudio,
    nextMeetIdentityVote,
    meetIdentityVoteReady,
    meetIdentityVoteFresh,
    selectMeetContributingSource,
    updateMeetReceiverLease,
    resolveMeetRouteIdentity,
    meetMicrophoneMuted,
    shouldSendMeetMicrophone,
  });
})();
