/* Copyright 2026 Kuali contributors · SPDX-License-Identifier: Apache-2.0 */
(() => {
  const MIC_CHANNEL = 1000;

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
    localMeetAudioDisabled,
    meetMicrophoneMuted,
    shouldSendMeetMicrophone,
  });
})();
