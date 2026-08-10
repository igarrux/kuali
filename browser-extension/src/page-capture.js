/*
 * Copyright 2026 Kuali contributors
 * SPDX-License-Identifier: Apache-2.0
 * Kuali capture implementation. WebRTC interception, activity correlation and
 * selected platform DOM selectors were adapted from Vexa and then modified.
 */
(() => {
  if (window.__kualiCaptureInstalled) return;
  window.__kualiCaptureInstalled = true;

  const FROM_PAGE = "kuali.capture.v1";
  const TO_PAGE = "kuali.control.v1";
  const TARGET_RATE = 16000;
  const capturePolicy = globalThis.KualiCapturePolicy;
  if (!capturePolicy) throw new Error("Kuali capture policy was not loaded");
  const meetProtocol = globalThis.KualiMeetProtocol;
  const { MIC_CHANNEL } = capturePolicy;
  const host = location.hostname;
  const platform = host === "meet.google.com"
    ? "google_meet"
    : (host === "zoom.us" || host.endsWith(".zoom.us") ? "zoom" : "microsoft_teams");

  let running = false;
  let context = null;
  let workletReady = null;
  let workletUrl = null;
  let micStream = null;
  let micStreamOwned = false;
  let localIdentity = null;
  let scanTimer = null;
  let activityTimer = null;
  let nextChannel = 1;
  const knownTracks = new Map();
  const remoteTracks = new Map();
  const observedPeers = new Set();
  const peerConstructorOptions = new WeakMap();
  const activatingTracks = new Set();
  const bindings = new Map();
  const identityVotes = new Map();
  const sentIdentity = new Map();
  let cachedRoster = [];
  let cachedDomRoster = [];
  let cachedActive = [];
  let lastIdentityScan = 0;
  let lastRosterFingerprint = "";
  let lastMeetProbeAt = 0;
  let meetProbeInFlight = false;
  let lastActiveFingerprint = "";
  const observedActive = new Map();
  const meetUsers = new Map();
  const meetIdentities = new Map();
  const meetOutputsBySource = new Map();
  const meetRoutesBySource = new Map();
  const meetChannelsByDevice = new Map();
  const meetSourceIdentityVotes = new Map();
  const meetAnnouncedChannels = new Set();
  const meetLastFrameTimestamp = new Map();
  const observedMeetDataChannels = new WeakSet();
  const meetEncodedTaps = new Set();
  const meetEncodedTapByReceiver = new WeakMap();
  let meetEncodedStreamHookCalls = 0;
  let meetProtocolWarningSent = false;
  let meetProtocolMicMuted = null;
  let meetMicAllowed = false;
  let meetMicCheckedAt = 0;

  function post(type, payload = {}) {
    window.postMessage({ protocol: FROM_PAGE, type, ...payload }, "*");
  }

  function meetingEvent(kind, detail, speaker = null) {
    post("meeting-event", { kind, ts: Date.now(), detail, speaker });
  }

  function clean(value) {
    return String(value || "").replace(/\s+/g, " ").trim();
  }

  function meetSourceKey(value) {
    if (value === null || value === undefined || value === "") return "";
    return String(value);
  }

  function rememberMeetIdentity(identity) {
    if (platform !== "google_meet" || !identity) return identity;
    const id = clean(identity.id);
    if (!id) return identity;
    const remembered = capturePolicy.mergeMeetParticipantIdentity(meetIdentities.get(id), identity);
    meetIdentities.set(id, remembered);
    return remembered;
  }

  function meetIdentityForDevice(deviceId, source) {
    const user = meetUsers.get(deviceId);
    const rosterIdentity = identitySnapshot().roster
      .map(({ identity }) => identity)
      .find((identity) => identity.id === deviceId);
    return rememberMeetIdentity(capturePolicy.meetParticipantIdentity({
      deviceId,
      source,
      user,
      domIdentity: rosterIdentity,
    }));
  }

  function canonicalMeetDeviceId(deviceId) {
    return meetUsers.get(deviceId)?.parentDeviceId || deviceId;
  }

  function announceMeetRoute(route) {
    if (!running) return;
    if (!meetAnnouncedChannels.has(route.channel)) {
      meetAnnouncedChannels.add(route.channel);
      meetingEvent("track-connected", capturePolicy.connectedTrackDetail(route.channel, platform));
    }
    // A CSRC is available a few hundred milliseconds before Meet's UI tells us
    // which device owns it. Sending PCM during that gap makes the receiver
    // create a permanent placeholder participant. Keep the first blocks on the
    // route and release them as soon as the stable device ID is known.
    if (!route.deviceId) return;
    bind(route.channel, route.identity);
    if (route.pendingAudio?.length) {
      for (const buffered of route.pendingAudio) post("audio", buffered);
      route.pendingAudio.length = 0;
    }
  }

  function updateMeetRouteIdentity(route) {
    const output = meetOutputsBySource.get(route.source);
    const nextDeviceId = canonicalMeetDeviceId(output?.deviceId || route.deviceId || "");
    if (nextDeviceId && nextDeviceId !== route.deviceId) {
      if (route.deviceId && meetChannelsByDevice.get(route.deviceId) === route.channel) {
        meetChannelsByDevice.delete(route.deviceId);
      }
      const existingChannel = meetChannelsByDevice.get(nextDeviceId);
      if (existingChannel !== undefined && existingChannel !== route.channel) {
        route.channel = existingChannel;
      } else {
        meetChannelsByDevice.set(nextDeviceId, route.channel);
      }
      route.deviceId = nextDeviceId;
    }
    route.disabled = !!output?.disabled;
    route.identity = meetIdentityForDevice(route.deviceId, route.source);
    announceMeetRoute(route);
  }

  function meetRouteForSource(sourceValue) {
    const source = meetSourceKey(sourceValue);
    if (!source || source === "42") return null;
    let route = meetRoutesBySource.get(source);
    if (!route) {
      const output = meetOutputsBySource.get(source);
      const deviceId = canonicalMeetDeviceId(output?.deviceId || "");
      const existingChannel = deviceId ? meetChannelsByDevice.get(deviceId) : undefined;
      const channel = existingChannel ?? nextChannel++;
      route = {
        source,
        deviceId,
        channel,
        disabled: !!output?.disabled,
        identity: meetIdentityForDevice(deviceId, source),
        pendingAudio: [],
      };
      meetRoutesBySource.set(source, route);
      if (deviceId) meetChannelsByDevice.set(deviceId, channel);
    } else {
      updateMeetRouteIdentity(route);
    }
    announceMeetRoute(route);
    return route;
  }

  function correlateMeetRouteWithActiveSpeaker(route, peak) {
    if (!route || route.deviceId || peak < 0.01) return;
    const candidates = identitySnapshot().active.filter((identity) => {
      if (identity.isSelf) return false;
      return !meetUsers.get(identity.id)?.isCurrentUser;
    });
    if (candidates.length !== 1) return;
    const candidate = candidates[0];
    const now = performance.now();
    const previous = meetSourceIdentityVotes.get(route.source);
    const vote = previous?.deviceId === candidate.id
      ? { ...previous, samples: previous.samples + 1, lastAt: now }
      : { deviceId: candidate.id, identity: candidate, samples: 1, firstAt: now, lastAt: now };
    meetSourceIdentityVotes.set(route.source, vote);
    // Require sustained decoded speech, not a single glow edge. Once bound,
    // Meet's CSRC remains stable for the participant's entire session.
    if (vote.samples < 6 || vote.lastAt - vote.firstAt < 250) return;
    const existingChannel = meetChannelsByDevice.get(candidate.id);
    if (existingChannel !== undefined && existingChannel !== route.channel) return;
    route.deviceId = candidate.id;
    meetChannelsByDevice.set(candidate.id, route.channel);
    route.identity = meetIdentityForDevice(candidate.id, route.source);
    meetSourceIdentityVotes.delete(route.source);
    announceMeetRoute(route);
  }

  function applyMeetUsers(users) {
    for (const user of users || []) {
      if (!user?.deviceId) continue;
      meetUsers.set(user.deviceId, { ...meetUsers.get(user.deviceId), ...user });
    }
    for (const route of meetRoutesBySource.values()) updateMeetRouteIdentity(route);
    const currentUser = [...meetUsers.values()].find((user) => user.isCurrentUser);
    if (currentUser) {
      const rosterIdentity = identitySnapshot(true).roster
        .map(({ identity }) => identity)
        .find((identity) => identity.id === currentUser.deviceId);
      localIdentity = rememberMeetIdentity({
        ...capturePolicy.meetParticipantIdentity({
          deviceId: currentUser.deviceId,
          user: currentUser,
          domIdentity: rosterIdentity,
        }),
        isSelf: true,
      });
      if (running && bindings.has(MIC_CHANNEL)) bind(MIC_CHANNEL, localIdentity);
      sendRosterState(identitySnapshot(true));
    }
    refreshMeetMicrophoneState();
  }

  function applyMeetDeviceOutputs(outputs) {
    for (const output of outputs || []) {
      if (!output?.streamId || !output?.deviceId || output.outputType !== 1) continue;
      meetOutputsBySource.set(meetSourceKey(output.streamId), output);
    }
    for (const route of meetRoutesBySource.values()) updateMeetRouteIdentity(route);
    refreshMeetMicrophoneState();
  }

  function refreshMeetMicrophoneState() {
    meetProtocolMicMuted = capturePolicy.localMeetAudioDisabled(
      meetUsers.values(),
      meetOutputsBySource.values(),
    );
    meetMicCheckedAt = 0;
  }

  function meetMicrophoneMutedFromControls() {
    if (platform !== "google_meet") return null;
    const controls = document.querySelectorAll("button,[role='button']");
    for (const control of controls) {
      if (control.disabled || control.getAttribute?.("aria-hidden") === "true") continue;
      const style = getComputedStyle(control);
      if (style.display === "none" || style.visibility === "hidden") continue;
      if (control.getClientRects?.().length === 0) continue;
      const stateNode = control.matches?.("[data-is-muted],[data-muted]")
        ? control
        : control.querySelector?.("[data-is-muted],[data-muted]");
      const label = clean([
        control.getAttribute?.("aria-label"),
        control.getAttribute?.("data-tooltip"),
        control.getAttribute?.("data-tooltip-text"),
        control.getAttribute?.("title"),
        stateNode?.getAttribute?.("aria-label"),
      ].filter(Boolean).join(" ")).toLocaleLowerCase();
      const icon = clean(control.textContent).toLocaleLowerCase();
      if (
        !/(microphone|micrófono|microfono|microfone)/i.test(label)
        && !/\bmic(?:_off)?\b/.test(icon)
      ) continue;
      const mutedAttribute = stateNode?.getAttribute?.("data-is-muted")
        ?? stateNode?.getAttribute?.("data-muted")
        ?? null;
      const muted = capturePolicy.meetMicrophoneMuted({ mutedAttribute, label, icon });
      if (muted !== null) return muted;
    }
    return null;
  }

  function shouldSendLocalMicrophone(track) {
    if (platform !== "google_meet") return track.enabled && !track.muted;
    const now = performance.now();
    if (now - meetMicCheckedAt < 100) return meetMicAllowed;
    meetMicCheckedAt = now;
    const controlState = meetMicrophoneMutedFromControls();
    meetMicAllowed = capturePolicy.shouldSendMeetMicrophone({
      trackEnabled: track.enabled,
      controlMuted: controlState,
    });
    return meetMicAllowed;
  }

  async function handleMeetCollectionMessage(data) {
    if (!meetProtocol) return;
    try {
      const update = await meetProtocol.decodeCollectionPacket(data);
      applyMeetUsers(update.users);
      applyMeetDeviceOutputs(update.deviceOutputs);
    } catch (error) {
      if (!meetProtocolWarningSent) {
        meetProtocolWarningSent = true;
        meetingEvent("warning", {
          code: "meet-collections-decode-failed",
          message: String(error?.message || error),
        });
      }
    }
  }

  function observeMeetDataChannel(channel) {
    if (platform !== "google_meet" || !channel || observedMeetDataChannels.has(channel)) return;
    observedMeetDataChannels.add(channel);
    if (channel.label !== "collections") return;
    channel.binaryType = "arraybuffer";
    channel.addEventListener("message", (event) => {
      handleMeetCollectionMessage(event.data);
    });
  }

  function primaryOpusPayload(data, metadata) {
    const bytes = new Uint8Array(data);
    const mimeType = clean(metadata?.mimeType).toLocaleLowerCase();
    if (mimeType !== "audio/red" && metadata?.payloadType !== 63) return bytes;
    let offset = 0;
    let redundantLength = 0;
    let primaryPayloadType = null;
    while (offset < bytes.length) {
      const header = bytes[offset];
      const follows = (header & 0x80) !== 0;
      const payloadType = header & 0x7f;
      if (!follows) {
        primaryPayloadType = payloadType;
        offset += 1;
        break;
      }
      if (offset + 4 > bytes.length) return bytes;
      redundantLength += ((bytes[offset + 2] & 0x03) << 8) | bytes[offset + 3];
      offset += 4;
    }
    const payloadOffset = offset + redundantLength;
    if (primaryPayloadType !== 111 || payloadOffset >= bytes.length) return bytes;
    return bytes.subarray(payloadOffset);
  }

  function closeMeetEncodedTap(tap) {
    if (tap.closed) return;
    tap.closed = true;
    try { tap.decoder?.close(); } catch (_) {}
    tap.decoder = null;
    tap.routesByTimestamp.clear();
    tap.pcmBuffers.clear();
    meetEncodedTaps.delete(tap);
  }

  function ensureMeetAudioDecoder(tap) {
    if (tap.decoder || typeof AudioDecoder !== "function" || typeof EncodedAudioChunk !== "function") {
      return tap.decoder;
    }
    tap.decoder = new AudioDecoder({
      output(audioData) {
        try {
          tap.decodedFrames += 1;
          const route = tap.routesByTimestamp.get(audioData.timestamp) || null;
          tap.routesByTimestamp.delete(audioData.timestamp);
          if (!running || !route) return;
          const mono = new Float32Array(audioData.numberOfFrames);
          const plane = new Float32Array(audioData.numberOfFrames);
          for (let channel = 0; channel < audioData.numberOfChannels; channel += 1) {
            audioData.copyTo(plane, { planeIndex: channel, format: "f32-planar" });
            for (let index = 0; index < mono.length; index += 1) mono[index] += plane[index];
          }
          if (audioData.numberOfChannels > 1) {
            for (let index = 0; index < mono.length; index += 1) mono[index] /= audioData.numberOfChannels;
          }
          let peak = 0;
          for (const sample of mono) peak = Math.max(peak, Math.abs(sample));
          correlateMeetRouteWithActiveSpeaker(route, peak);
          pushTrackProcessorPcm(tap, resampleMono(mono, audioData.sampleRate), route);
        } finally {
          audioData.close();
        }
      },
      error(error) {
        tap.decodeErrors += 1;
        tap.lastDecodeError = String(error?.message || error);
      },
    });
    tap.decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
    return tap.decoder;
  }

  function meetSourceFromEncodedMetadata(metadata) {
    const sources = Array.isArray(metadata?.contributingSources) ? metadata.contributingSources : [];
    const source = sources
      .map((value) => typeof value === "object" ? value?.source : value)
      .map(meetSourceKey)
      .find((value) => value && value !== "42");
    return source || "";
  }

  function observeMeetEncodedFrame(receiver, frame) {
    let tap = meetEncodedTapByReceiver.get(receiver);
    if (!tap) {
      tap = {
        receiver,
        track: receiver.track,
        channel: null,
        virtualMeetLane: true,
        released: false,
        closed: false,
        encodedFrames: 0,
        decodedFrames: 0,
        decodeErrors: 0,
        lastDecodeError: null,
        lastMetadata: null,
        nextTimestamp: 0,
        routesByTimestamp: new Map(),
        pcmBuffers: new Map(),
        pcmFrames: 0,
        peak: 0,
      };
      meetEncodedTapByReceiver.set(receiver, tap);
      meetEncodedTaps.add(tap);
      receiver.track?.addEventListener?.("ended", () => closeMeetEncodedTap(tap), { once: true });
    }
    const metadata = frame.getMetadata?.() || {};
    tap.encodedFrames += 1;
    tap.lastMetadata = {
      mimeType: metadata.mimeType || null,
      payloadType: metadata.payloadType ?? null,
      synchronizationSource: metadata.synchronizationSource ?? null,
      contributingSources: metadata.contributingSources || [],
      audioLevel: metadata.audioLevel ?? null,
    };
    if (!running) return;
    const source = meetSourceFromEncodedMetadata(metadata);
    const route = meetRouteForSource(source);
    if (!route || route.disabled || route.identity?.isSelf) return;
    const decoder = ensureMeetAudioDecoder(tap);
    if (!decoder || decoder.state !== "configured") return;
    const payload = primaryOpusPayload(frame.data, metadata);
    const timestamp = tap.nextTimestamp;
    tap.nextTimestamp += 20_000;
    tap.routesByTimestamp.set(timestamp, route);
    try {
      decoder.decode(new EncodedAudioChunk({
        type: "key",
        timestamp,
        duration: 20_000,
        data: payload.slice().buffer,
      }));
    } catch (error) {
      tap.routesByTimestamp.delete(timestamp);
      tap.decodeErrors += 1;
      tap.lastDecodeError = String(error?.message || error);
    }
  }

  function firstAttribute(element, names) {
    for (const name of names) {
      const value = clean(element?.getAttribute?.(name));
      if (value) return value;
    }
    return "";
  }

  function participantTile(element) {
    if (platform === "google_meet") {
      return element?.closest?.("[data-participant-id]") || null;
    }
    return element?.closest?.([
      "[data-participant-id]",
      "[data-requested-participant-id]",
      "[data-user-id]",
      "[data-tid*='participant']",
      "[data-tid*='video-tile']",
      "[class*='participant']",
      "[class*='video-avatar']",
      "[class*='video-tile']",
    ].join(",")) || null;
  }

  function avatarFrom(tile) {
    const direct = firstAttribute(tile, ["data-avatar-url", "data-avatar-src"]);
    if (direct) return direct;
    const image = tile?.querySelector?.("img[src]");
    if (image?.src && !image.src.startsWith("data:image/svg")) return image.src;
    const styled = tile?.querySelector?.("[style*='background-image']");
    const match = styled?.style?.backgroundImage?.match(/url\(["']?(.*?)["']?\)/);
    return match?.[1] || null;
  }

  function nameFrom(tile) {
    if (!tile) return "";
    const attribute = firstAttribute(tile, ["data-self-name", "data-display-name", "data-participant-name"]);
    if (attribute && attribute.length <= 100) return attribute.replace(/\s*\([^)]*(?:you|tú|usted)[^)]*\)\s*/i, "").trim();
    const selfMarker = tile.querySelector?.("[data-self-name]");
    const selfName = clean(selfMarker?.getAttribute?.("data-self-name"));
    if (selfName) return selfName;
    const selectors = platform === "google_meet"
      ? ["span.notranslate"]
      : platform === "zoom"
        ? [".video-avatar__avatar-footer span", ".video-avatar__avatar-footer", "[class*='display-name']"]
        : ["div[class*='___2u340f0']", "[data-tid*='display-name']", "[data-tid*='participant-name']", "[data-tid*='user-name']", "[class*='participant-name']", "[class*='display-name']", "span[title]"];
    for (const selector of selectors) {
      const node = tile.querySelector?.(selector);
      const value = clean(node?.getAttribute?.("title") || node?.textContent);
      if (value && value.length <= 100) return value;
    }
    const aria = clean(tile.getAttribute?.("aria-label"));
    if (aria && aria.length <= 100) return aria;
    return "";
  }

  function idFrom(tile, name) {
    const id = firstAttribute(tile, [
      "data-participant-id",
      "data-requested-participant-id",
      "data-user-id",
      "data-member-id",
      "data-uid",
      "data-acc-element-id",
      "data-object-id",
      "id",
    ]);
    return id || (name ? `${platform}:name:${name.toLocaleLowerCase()}` : "");
  }

  function isSelf(tile, candidateName = "") {
    if (!tile) return false;
    if (tile.matches?.("[data-self-name],[data-is-self='true']")
      || tile.querySelector?.("[data-self-name],[data-is-self='true']")) return true;
    if (platform !== "google_meet") return false;
    const tileId = clean(tile.getAttribute?.("data-participant-id"));
    if (tileId && meetUsers.get(tileId)?.isCurrentUser) return true;
    for (const selfMarker of document.querySelectorAll("[data-self-name]")) {
      const selfTile = selfMarker.closest?.("[data-participant-id]");
      const selfId = clean(selfTile?.getAttribute?.("data-participant-id"));
      if (selfId && selfId === tileId) return true;
      // Meet currently renders data-self-name outside the participant tile in
      // some layouts. The value is still Meet's own structural self marker; use
      // it to resolve the matching roster tile when no common ancestor exists.
      const selfName = clean(selfMarker.getAttribute?.("data-self-name"));
      if (selfName && candidateName && selfName === candidateName) return true;
    }
    return false;
  }

  function identityFrom(tile) {
    const rawName = nameFrom(tile);
    const id = idFrom(tile, rawName);
    const name = platform === "google_meet"
      ? capturePolicy.usableMeetParticipantName(rawName, id)
      : rawName;
    if (!name && !id) return null;
    const identity = { id, name: name || "Participante", avatarUrl: avatarFrom(tile), isSelf: isSelf(tile, name) };
    return platform === "google_meet" ? rememberMeetIdentity(identity) : identity;
  }

  function participantTiles() {
    if (platform === "google_meet") {
      const tiles = [];
      const seenIds = new Set();
      for (const tile of document.querySelectorAll("[data-participant-id]")) {
        const id = clean(tile.getAttribute("data-participant-id"));
        if (!id || seenIds.has(id)) continue;
        seenIds.add(id);
        const identity = identityFrom(tile);
        if (identity) tiles.push({ tile, identity });
      }
      return tiles;
    }
    const selectors = platform === "google_meet"
      ? ["[data-participant-id]"]
      : platform === "zoom"
        ? ["[data-user-id]", ".video-avatar__avatar-footer"]
        : ["[data-participant-id]", "[data-user-id]", "[data-tid*='participant']", "[data-tid*='roster']", "[data-tid*='video-tile']", "[role='listitem']"];
    const tiles = [];
    const seen = new Set();
    for (const node of document.querySelectorAll(selectors.join(","))) {
      const tile = participantTile(node) || node;
      if (seen.has(tile)) continue;
      seen.add(tile);
      const identity = identityFrom(tile);
      if (identity) tiles.push({ tile, identity });
    }
    return tiles;
  }

  function readActiveIdentities(includeSelf = false) {
    const candidates = [];
    if (platform === "zoom") {
      for (const node of document.querySelectorAll(".speaker-active-container__video-frame,.speaker-bar-container__video-frame--active")) {
        const identity = identityFrom(participantTile(node) || node);
        if (identity) candidates.push(identity);
      }
    } else if (platform === "microsoft_teams") {
      for (const outline of document.querySelectorAll('[data-tid="voice-level-stream-outline"]')) {
        const tile = participantTile(outline);
        if (!tile) continue;
        const style = getComputedStyle(outline);
        if (style.display === "none" || style.visibility === "hidden") continue;
        let speaking = false;
        for (let node = outline; node && node !== tile.parentElement; node = node.parentElement) {
          if (node.classList?.contains("vdi-frame-occlusion")) {
            speaking = true;
            break;
          }
        }
        if (!speaking) continue;
        const identity = identityFrom(tile);
        if (identity) candidates.push(identity);
      }
    } else {
      const activeSelector = [
        "[data-is-speaking='true']",
        "[data-speaking='true']",
        "[class*='speaking']",
        "[class*='talking']",
        "[aria-label*='speaking' i]",
        ".Oaajhc",
        ".HX2H7",
        ".wEsLMd",
        ".OgVli",
      ].join(",");
      for (const marker of document.querySelectorAll(activeSelector)) {
        const identity = identityFrom(participantTile(marker));
        if (identity) candidates.push(identity);
      }
    }
    const unique = new Map(
      candidates
        .filter((person) => includeSelf || !person.isSelf)
        .map((person) => [person.id || person.name, person]),
    );
    return [...unique.values()];
  }

  function identitySnapshot(force = false) {
    const now = performance.now();
    if (force || now - lastIdentityScan >= 250) {
      cachedDomRoster = participantTiles();
      cachedRoster = platform === "google_meet"
        ? capturePolicy.mergeMeetRoster(cachedDomRoster, meetUsers.values())
        : cachedDomRoster;
      if (platform === "google_meet") {
        cachedRoster = cachedRoster.map((entry) => ({
          ...entry,
          identity: rememberMeetIdentity(entry.identity),
        }));
      }
      cachedActive = readActiveIdentities();
      const rosterById = new Map(cachedRoster.map(({ identity }) => [identity.id, identity]));
      cachedActive = cachedActive.map((identity) => rosterById.get(identity.id) || identity);
      lastIdentityScan = now;
    }
    return { roster: cachedRoster, domRoster: cachedDomRoster, active: cachedActive };
  }

  function sendRosterState(snapshot) {
    if (window !== window.top || platform !== "google_meet") return;
    const roster = [...snapshot.roster];
    if (localIdentity && roster.length === 0) {
      roster.push({ tile: null, identity: localIdentity });
    }
    const detail = capturePolicy.rosterDetail(roster, platform);
    // `participants` may contain the local fallback above so the UI keeps a
    // stable identity while Meet repaints. Preserve the raw DOM presence too:
    // its persistent disappearance is how the extension knows the user hung up.
    detail.selfPresentInDom = snapshot.domRoster.some(({ identity }) => identity.isSelf);
    detail.inCall = platform !== "google_meet" || meetMicrophoneMutedFromControls() !== null;
    detail.protocolBacked = [...meetUsers.values()].some(
      (user) => user.status === 1 && user.isCurrentUser,
    );
    const fingerprint = JSON.stringify(detail);
    if (fingerprint === lastRosterFingerprint) return;
    lastRosterFingerprint = fingerprint;
    meetingEvent("roster-state", detail);
  }

  function sendActiveSpeakerState(snapshot) {
    if (!running || window !== window.top || platform !== "google_meet") return;
    // Track correlation deliberately excludes the local user. Keep that user in
    // the UI roster so their tile lights up when Meet marks the microphone active.
    const rosterById = new Map(snapshot.roster.map(({ identity }) => [identity.id, identity]));
    const participants = readActiveIdentities(true).map((identity) => rosterById.get(identity.id) || identity).map((identity) => ({
      participantId: identity.id || null,
      displayName: identity.name || "Participante",
      avatarUrl: identity.avatarUrl || null,
      isSelf: !!identity.isSelf,
    }));
    const fingerprint = JSON.stringify(participants.map((person) => person.participantId || person.displayName).sort());
    if (fingerprint === lastActiveFingerprint) return;
    lastActiveFingerprint = fingerprint;
    meetingEvent("active-speakers", { platform, participants });
  }

  async function sendMeetProbe(snapshot) {
    if (!running || meetProbeInFlight || window !== window.top || platform !== "google_meet") return;
    const now = Date.now();
    if (now - lastMeetProbeAt < 2_000) return;
    lastMeetProbeAt = now;
    meetProbeInFlight = true;
    for (const identity of snapshot.active) {
      observedActive.set(identity.id || identity.name, {
        participantId: identity.id || null,
        displayName: identity.name || null,
        isSelf: !!identity.isSelf,
      });
    }
    const media = [...document.querySelectorAll("audio,video")]
      .map((element, index) => {
        const stream = element.srcObject instanceof MediaStream ? element.srcObject : null;
        const tracks = stream?.getAudioTracks?.() || [];
        if (!tracks.length) return null;
        const ancestry = [];
        for (let node = element; node && ancestry.length < 8; node = node.parentElement) {
          const attributes = {};
          for (const attribute of node.attributes || []) {
            if (/participant|device|audio|stream|request|self/i.test(attribute.name)) {
              attributes[attribute.name] = attribute.value;
            }
          }
          if (Object.keys(attributes).length) ancestry.push({ tag: node.tagName, attributes });
        }
        return {
          index,
          tag: element.tagName,
          paused: element.paused,
          muted: element.muted,
          streamId: stream.id,
          tracks: tracks.map((track) => ({
            id: track.id,
            label: track.label,
            enabled: track.enabled,
            muted: track.muted,
            readyState: track.readyState,
          })),
          ancestry,
        };
      })
      .filter(Boolean);
    try {
      const peerConnections = [];
      for (const [index, peer] of [...observedPeers].entries()) {
        const report = await peer.getStats().catch(() => null);
        const inboundAudio = [];
        const codecs = [];
        if (report) {
          for (const stat of report.values()) {
            if (stat.type === "codec" && String(stat.mimeType || "").toLowerCase().startsWith("audio/")) {
              codecs.push({
                id: stat.id,
                mimeType: stat.mimeType,
                clockRate: stat.clockRate ?? null,
                channels: stat.channels ?? null,
                sdpFmtpLine: stat.sdpFmtpLine ?? null,
              });
            }
            if (stat.type !== "inbound-rtp" || (stat.kind || stat.mediaType) !== "audio") continue;
            inboundAudio.push({
              id: stat.id,
              ssrc: stat.ssrc ?? null,
              mid: stat.mid ?? null,
              trackIdentifier: stat.trackIdentifier ?? null,
              codecId: stat.codecId ?? null,
              packetsReceived: stat.packetsReceived ?? null,
              bytesReceived: stat.bytesReceived ?? null,
              audioLevel: stat.audioLevel ?? null,
              totalAudioEnergy: stat.totalAudioEnergy ?? null,
              totalSamplesReceived: stat.totalSamplesReceived ?? null,
              jitterBufferEmittedCount: stat.jitterBufferEmittedCount ?? null,
              jitterBufferDelay: stat.jitterBufferDelay ?? null,
              concealedSamples: stat.concealedSamples ?? null,
              silentConcealedSamples: stat.silentConcealedSamples ?? null,
            });
          }
        }
        peerConnections.push({
          index,
          connectionState: peer.connectionState,
          iceConnectionState: peer.iceConnectionState,
          constructorOptions: (() => {
            const options = peerConstructorOptions.get(peer) || {};
            return {
              encodedInsertableStreams: options.encodedInsertableStreams ?? null,
              forceEncodedAudioInsertableStreams: options.forceEncodedAudioInsertableStreams ?? null,
              bundlePolicy: options.bundlePolicy ?? null,
              rtcpMuxPolicy: options.rtcpMuxPolicy ?? null,
            };
          })(),
          inboundAudio,
          codecs,
          receivers: peer.getReceivers().filter((receiver) => receiver.track?.kind === "audio").map((receiver) => ({
            trackId: receiver.track.id,
            enabled: receiver.track.enabled,
            muted: receiver.track.muted,
            readyState: receiver.track.readyState,
            isolated: receiver.track.isolated ?? null,
            contentHint: receiver.track.contentHint || null,
            hasCreateEncodedStreams: typeof receiver.createEncodedStreams === "function",
            hasTransform: "transform" in receiver,
            transform: receiver.transform ? receiver.transform.constructor?.name || "present" : null,
            parameters: (() => {
              try {
                const parameters = receiver.getParameters?.() || {};
                return {
                  codecs: (parameters.codecs || []).map((codec) => ({
                    payloadType: codec.payloadType,
                    mimeType: codec.mimeType,
                    clockRate: codec.clockRate,
                    channels: codec.channels ?? null,
                    sdpFmtpLine: codec.sdpFmtpLine || null,
                  })),
                  headerExtensions: (parameters.headerExtensions || []).map((extension) => ({
                    id: extension.id,
                    uri: extension.uri,
                    encrypted: extension.encrypted ?? null,
                  })),
                };
              } catch (error) {
                return { error: clean(error?.name || error?.message || error) };
              }
            })(),
            sources: receiver.getSynchronizationSources?.().map((source) => ({
              source: source.source,
              audioLevel: source.audioLevel ?? null,
              rtpTimestamp: source.rtpTimestamp ?? null,
              timestamp: source.timestamp,
            })) || [],
            contributingSources: receiver.getContributingSources?.().map((source) => ({
              source: source.source,
              audioLevel: source.audioLevel ?? null,
              rtpTimestamp: source.rtpTimestamp ?? null,
              timestamp: source.timestamp,
              mappedDeviceId: meetOutputsBySource.get(meetSourceKey(source.source))?.deviceId || null,
            })) || [],
          })),
          transceivers: peer.getTransceivers().filter((transceiver) => transceiver.receiver.track?.kind === "audio")
            .map((transceiver) => ({ mid: transceiver.mid, trackId: transceiver.receiver.track.id })),
          remoteAudioSdp: (() => {
            const sdp = peer.remoteDescription?.sdp || "";
            const audioSection = sdp.split(/\r?\nm=/).find((section) => section.startsWith("audio ")) || "";
            return audioSection.split(/\r?\n/).filter((line) => /^(audio |a=(?:mid|msid|rtpmap|fmtp|ssrc|identity|extmap):)/.test(line)).slice(0, 80);
          })(),
        });
      }
      meetingEvent("meet-probe", {
        capabilities: {
          mediaStreamTrackProcessor: typeof MediaStreamTrackProcessor === "function",
          audioData: typeof AudioData === "function",
        },
        roster: capturePolicy.rosterDetail(snapshot.roster, platform),
        activeNow: snapshot.active.map((identity) => ({
          participantId: identity.id || null,
          displayName: identity.name || null,
          isSelf: !!identity.isSelf,
        })),
        observedActive: [...observedActive.values()],
        selfMarkers: [...document.querySelectorAll("[data-self-name]")].slice(0, 12).map((marker) => ({
          value: clean(marker.getAttribute("data-self-name")),
          participantId: clean(marker.closest?.("[data-participant-id]")?.getAttribute?.("data-participant-id")) || null,
          tag: marker.tagName,
        })),
        requestedParticipantIds: [...new Set([...document.querySelectorAll("[data-requested-participant-id]")]
          .map((node) => clean(node.getAttribute("data-requested-participant-id")))
          .filter(Boolean))],
        media,
        peerConnections,
        captureLanes: [...remoteTracks.values()].map((entry) => ({
          channel: entry.channel,
          trackId: entry.track.id,
          trackEnabled: entry.track.enabled,
          trackMuted: entry.track.muted,
          trackReadyState: entry.track.readyState,
          processorTrackId: entry.processorTrack?.id || null,
          processorTrackMuted: entry.processorTrack?.muted ?? null,
          streamId: entry.stream.id,
          captureMethod: entry.captureMethod,
          playbackState: entry.playbackState || null,
          playbackPaused: entry.playbackElement?.paused ?? null,
          playbackReadyState: entry.playbackElement?.readyState ?? null,
          sourceFrames: entry.sourceFrames,
          pcmFrames: entry.pcmFrames,
          peak: entry.peak,
          blockedFrames: entry.blockedFrames || 0,
          participantId: bindings.get(entry.channel)?.id || null,
          displayName: bindings.get(entry.channel)?.name || null,
          isSelf: !!bindings.get(entry.channel)?.isSelf,
        })),
        meetSources: [...meetRoutesBySource.values()].map((route) => ({
          csrc: route.source,
          deviceId: route.deviceId || null,
          channel: route.channel,
          disabled: route.disabled,
          displayName: route.identity?.name || null,
        })),
        meetUsers: [...meetUsers.values()].map((user) => ({
          deviceId: user.deviceId,
          displayName: user.displayName || user.fullName || null,
          isCurrentUser: !!user.isCurrentUser,
          status: user.status ?? null,
        })),
        encodedAudio: {
          createEncodedStreamsCalls: meetEncodedStreamHookCalls,
          webCodecsAudioDecoder: typeof AudioDecoder === "function",
          taps: [...meetEncodedTaps].map((tap) => ({
            trackId: tap.receiver.track?.id || null,
            encodedFrames: tap.encodedFrames,
            decodedFrames: tap.decodedFrames,
            decodeErrors: tap.decodeErrors,
            lastDecodeError: tap.lastDecodeError,
            metadata: tap.lastMetadata,
            pcmFrames: tap.pcmFrames,
            peak: tap.peak,
          })),
        },
        microphoneGate: {
          controlMuted: meetMicrophoneMutedFromControls(),
          protocolMuted: meetProtocolMicMuted,
          selfSpeaking: readActiveIdentities(true).some((identity) => identity.isSelf),
          allowed: meetMicAllowed,
        },
      });
    } finally {
      meetProbeInFlight = false;
    }
  }

  function sendIdentity(channel, identity, audioKind = "separate") {
    const detail = {
      channel,
      participantId: identity?.id || null,
      displayName: identity?.name || (audioKind === "mixed" ? "Sala" : "Participante"),
      avatarUrl: identity?.avatarUrl || null,
      isSelf: !!identity?.isSelf,
      audioKind,
      platform,
    };
    const fingerprint = JSON.stringify(detail);
    if (sentIdentity.get(channel) === fingerprint) return;
    sentIdentity.set(channel, fingerprint);
    meetingEvent("participant-upsert", detail, detail.displayName);
  }

  function bind(channel, identity) {
    const previous = bindings.get(channel);
    if (
      previous?.id === identity.id
      && previous?.name === identity.name
      && previous?.avatarUrl === identity.avatarUrl
      && previous?.isSelf === identity.isSelf
    ) return;
    bindings.set(channel, identity);
    sendIdentity(channel, identity, "separate");
    const entry = [...remoteTracks.values()].find((track) => track.channel === channel);
    if (entry?.pending?.length) {
      for (const buffered of entry.pending) post("audio", buffered);
      entry.pending.length = 0;
    }
  }

  function bindMixed(channel) {
    const identity = { id: `${platform}:mixed`, name: "Sala", avatarUrl: null, isSelf: false };
    bindings.set(channel, identity);
    sendIdentity(channel, identity, "mixed");
    const entry = [...remoteTracks.values()].find((track) => track.channel === channel);
    if (entry?.pending?.length) {
      for (const buffered of entry.pending) post("audio", buffered);
      entry.pending.length = 0;
    }
  }

  function voteForIdentity(channel) {
    const people = identitySnapshot().active;
    if (people.length !== 1) return bindings.has(channel);
    const candidate = people[0];
    const bound = bindings.get(channel);
    if (bound?.id === candidate.id) {
      identityVotes.delete(channel);
      return true;
    }
    const previous = identityVotes.get(channel);
    const votes = previous?.id === candidate.id ? previous.votes + 1 : 1;
    identityVotes.set(channel, { id: candidate.id, votes, identity: candidate });
    if (votes >= 3) {
      bind(channel, candidate);
      identityVotes.delete(channel);
      return true;
    }
    // Repeated matches against a new tile indicate that Meet or Teams is likely
    // reassigning an owned channel. Hold those milliseconds until confirmed so
    // they are not attributed to the previous speaker.
    return false;
  }

  async function ensureAudioGraph() {
    if (!context || context.state === "closed") {
      context = new AudioContext({ sampleRate: TARGET_RATE, latencyHint: "interactive" });
      workletReady = context.audioWorklet.addModule(workletUrl);
    }
    await workletReady;
    await context.resume();
  }

  function resampleMono(samples, sourceRate) {
    if (sourceRate === TARGET_RATE) return samples;
    const ratio = sourceRate / TARGET_RATE;
    const length = Math.max(1, Math.floor(samples.length / ratio));
    const output = new Float32Array(length);
    for (let index = 0; index < length; index += 1) {
      const position = index * ratio;
      const left = Math.floor(position);
      const right = Math.min(samples.length - 1, left + 1);
      const fraction = position - left;
      output[index] = samples[left] * (1 - fraction) + samples[right] * fraction;
    }
    return output;
  }

  function pushTrackProcessorPcm(entry, samples, route = null) {
    if (!running) return;
    const bufferKey = route?.source || "default";
    let state = entry.pcmBuffers.get(bufferKey);
    if (!state) {
      state = { buffer: new Float32Array(2048), offset: 0 };
      entry.pcmBuffers.set(bufferKey, state);
    }
    let offset = 0;
    while (offset < samples.length) {
      const count = Math.min(samples.length - offset, state.buffer.length - state.offset);
      state.buffer.set(samples.subarray(offset, offset + count), state.offset);
      state.offset += count;
      offset += count;
      if (state.offset !== state.buffer.length) continue;
      let peak = 0;
      for (const sample of state.buffer) peak = Math.max(peak, Math.abs(sample));
      if (peak >= 0.0005 && !entry.released) onTrackPcm(entry, state.buffer, route);
      state.buffer = new Float32Array(2048);
      state.offset = 0;
    }
  }

  function meetRouteForAudioFrame(entry, audioData) {
    const sources = entry.receiver?.getContributingSources?.() || [];
    const participantSource = sources
      .filter((source) => meetSourceKey(source.source) !== "42")
      .sort((left, right) => (right.timestamp || 0) - (left.timestamp || 0))[0];
    if (!participantSource) return null;
    const route = meetRouteForSource(participantSource.source);
    if (!route || route.disabled || route.identity?.isSelf) return null;

    // During a virtual-lane hand-off the same source can briefly be visible on
    // two receivers. AudioData timestamps originate in the decoded stream, so
    // an exact/non-increasing timestamp is a duplicate rather than new speech.
    const frameTimestamp = Number(audioData.timestamp);
    if (Number.isFinite(frameTimestamp)) {
      const previous = meetLastFrameTimestamp.get(route.source);
      if (previous !== undefined && frameTimestamp <= previous) return null;
      meetLastFrameTimestamp.set(route.source, frameTimestamp);
    }
    return route;
  }

  async function consumeTrackProcessor(entry) {
    while (!entry.released) {
      const { value: audioData, done } = await entry.reader.read();
      if (done || !audioData) break;
      try {
        entry.sourceFrames += 1;
        if (!running) continue;
        const route = entry.virtualMeetLane ? meetRouteForAudioFrame(entry, audioData) : null;
        if (entry.virtualMeetLane && !route) continue;
        const mono = new Float32Array(audioData.numberOfFrames);
        const plane = new Float32Array(audioData.numberOfFrames);
        for (let channel = 0; channel < audioData.numberOfChannels; channel += 1) {
          audioData.copyTo(plane, { planeIndex: channel, format: "f32-planar" });
          for (let index = 0; index < mono.length; index += 1) mono[index] += plane[index];
        }
        if (audioData.numberOfChannels > 1) {
          for (let index = 0; index < mono.length; index += 1) mono[index] /= audioData.numberOfChannels;
        }
        pushTrackProcessorPcm(entry, resampleMono(mono, audioData.sampleRate), route);
      } finally {
        audioData.close();
      }
    }
  }

  function captureTrack(track, directIdentity = null, forcedChannel = null, sourceStream = null, receiver = null) {
    if (!track || track.kind !== "audio") return;
    if (forcedChannel === MIC_CHANNEL && localIdentity) directIdentity = localIdentity;
    const known = knownTracks.get(track.id);
    const nextKnown = {
      track,
      directIdentity: directIdentity || known?.directIdentity || null,
      forcedChannel: forcedChannel ?? known?.forcedChannel ?? null,
      sourceStream: sourceStream || known?.sourceStream || null,
      receiver: receiver || known?.receiver || null,
    };
    knownTracks.set(track.id, nextKnown);
    if (!known) track.addEventListener("ended", () => releaseTrack(track.id, true), { once: true });
    const active = remoteTracks.get(track.id);
    if (active) {
      // RTCPeerConnection can reveal a receiver track before Meet attaches its
      // real element MediaStream. If we initially had to wrap that track, swap
      // the source node as soon as the live element stream becomes available.
      if (active.captureMethod === "webaudio" && sourceStream instanceof MediaStream && active.stream !== sourceStream) {
        nextKnown.forcedChannel = active.channel;
        releaseTrack(track.id, false);
        captureTrack(track, nextKnown.directIdentity, nextKnown.forcedChannel, sourceStream);
        return;
      }
      if (directIdentity) {
        active.directIdentity = directIdentity;
        bind(active.channel, directIdentity);
      }
      return;
    }
    const canPrewarmMeetLane = platform === "google_meet"
      && forcedChannel !== MIC_CHANNEL
      && typeof MediaStreamTrackProcessor === "function"
      && typeof AudioData === "function";
    if ((!running && !canPrewarmMeetLane) || activatingTracks.has(track.id)) return;
    activatingTracks.add(track.id);
    activateTrack(track).catch((error) => {
      meetingEvent("warning", { code: "track-capture-failed", message: String(error?.message || error) });
    }).finally(() => activatingTracks.delete(track.id));
  }

  async function activateTrack(track) {
    const known = knownTracks.get(track.id);
    const canPrewarmMeetLane = platform === "google_meet"
      && known?.forcedChannel !== MIC_CHANNEL
      && typeof MediaStreamTrackProcessor === "function"
      && typeof AudioData === "function";
    if ((!running && !canPrewarmMeetLane) || remoteTracks.has(track.id) || track.readyState === "ended") return;
    if (!known) return;
    const virtualMeetLane = canPrewarmMeetLane && !!known.receiver;
    const channel = virtualMeetLane ? null : (known.forcedChannel ?? nextChannel++);
    // Keep the page's original MediaStream whenever available. Meet's receiver
    // lanes have been observed to expose live PCM through their element stream
    // while a newly constructed MediaStream([track]) remains silent.
    const stream = known.sourceStream instanceof MediaStream
      && known.sourceStream.getAudioTracks().some((candidate) => candidate.id === track.id)
      ? known.sourceStream
      : new MediaStream([track]);
    if (
      platform === "google_meet"
      && known.forcedChannel !== MIC_CHANNEL
      && typeof MediaStreamTrackProcessor === "function"
      && typeof AudioData === "function"
    ) {
      const trackProcessor = new MediaStreamTrackProcessor({ track });
      const entry = {
        channel,
        track,
        stream,
        source: null,
        processor: null,
        processorTrack: null,
        trackProcessor,
        reader: trackProcessor.readable.getReader(),
        playbackElement: null,
        playbackState: null,
        captureMethod: "track-processor",
        virtualMeetLane,
        receiver: known.receiver,
        directIdentity: known.directIdentity,
        pending: [],
        sourceFrames: 0,
        pcmFrames: 0,
        blockedFrames: 0,
        peak: 0,
        pcmBuffers: new Map(),
        released: false,
      };
      remoteTracks.set(track.id, entry);
      if (!virtualMeetLane && Number.isInteger(channel)) {
        meetingEvent("track-connected", capturePolicy.connectedTrackDetail(channel, platform));
        if (known.directIdentity) bind(channel, known.directIdentity);
      }
      consumeTrackProcessor(entry).catch((error) => {
        if (!entry.released) {
          meetingEvent("warning", { code: "track-processor-failed", message: String(error?.message || error) });
        }
      });
      return;
    }
    await ensureAudioGraph();
    if (!running || remoteTracks.has(track.id) || track.readyState === "ended") return;
    const source = context.createMediaStreamSource(stream);
    const processor = new AudioWorkletNode(context, "kuali-pcm", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [1],
      channelCount: 1,
      channelCountMode: "explicit",
      channelInterpretation: "speakers",
    });
    source.connect(processor);
    // The worklet only reads its input and leaves its output silent. Connecting
    // it directly keeps Chrome pulling remote Meet streams; a zero-gain node may
    // be optimized away, leaving receiver lanes connected but with no PCM.
    processor.connect(context.destination);
    const entry = {
      channel,
      track,
      stream,
      source,
      processor,
      trackProcessor: null,
      processorTrack: null,
      reader: null,
      playbackElement: null,
      playbackState: null,
      captureMethod: "webaudio",
      virtualMeetLane: false,
      receiver: known.receiver,
      directIdentity: known.directIdentity,
      pending: [],
      sourceFrames: 0,
      pcmFrames: 0,
      blockedFrames: 0,
      peak: 0,
      released: false,
    };
    remoteTracks.set(track.id, entry);
    // Report transport lanes separately from participants. Meet exposes a
    // reusable remote lane pool whose size is not the meeting headcount.
    if (platform === "google_meet") {
      meetingEvent("track-connected", capturePolicy.connectedTrackDetail(channel, platform));
    }
    if (known.directIdentity) bind(channel, known.directIdentity);
    processor.port.onmessage = (event) => {
      if (running) onTrackPcm(entry, new Float32Array(event.data));
    };
  }

  function onTrackPcm(entry, samples, route = null) {
    const channel = route?.channel ?? entry.channel;
    const { track } = entry;
    // MIC_CHANNEL reads Meet's source track before its RTP mute gate. Without
    // this check Kuali could hear the microphone while the call could not.
    if (channel === MIC_CHANNEL && !shouldSendLocalMicrophone(track)) {
      entry.blockedFrames += 1;
      return;
    }
    entry.pcmFrames += 1;
    for (const sample of samples) entry.peak = Math.max(entry.peak, Math.abs(sample));
    if (route) {
      announceMeetRoute(route);
      const frame = { channel, ts: Date.now(), pcm: Array.from(samples) };
      if (!route.deviceId) {
        route.pendingAudio.push(frame);
        // About ten seconds at the current 2048-sample framing. This bounds
        // memory if Meet changes its identity UI while still preserving the
        // complete beginning of ordinary speech once correlation succeeds.
        if (route.pendingAudio.length > 80) route.pendingAudio.shift();
        return;
      }
      post("audio", frame);
      return;
    }
    // Structural identity beats temporal guessing. Most importantly, channel
    // 1000 is the local getUserMedia stream and must never be renamed to the
    // remote tile that happens to glow while its PCM callback is running.
    if (!capturePolicy.shouldCorrelateIdentity(channel, entry.directIdentity)) {
      if (entry.directIdentity) bind(channel, entry.directIdentity);
      post("audio", { channel, ts: Date.now(), pcm: Array.from(samples) });
      return;
    }
    const remoteOnly = [...remoteTracks.values()].filter((value) => value.channel !== MIC_CHANNEL);
    const rosterSize = identitySnapshot().roster.filter(({ identity }) => !identity.isSelf).length;
    // Meet must keep its receiver lanes separate even when it currently exposes
    // only one of them. Labelling that lane as "Sala" would hide the exact
    // participant mapping and destroy overlapping-speaker information. Zoom
    // and Teams retain their explicit mixed fallback for now.
    const definitelyMixed = platform !== "google_meet"
      && channel !== MIC_CHANNEL
      && remoteOnly.length === 1
      && rosterSize > 1;
    if (definitelyMixed) {
      bindMixed(channel);
    } else if (!voteForIdentity(channel)) {
      entry.pending.push({ channel, ts: Date.now(), pcm: Array.from(samples) });
      // After roughly two seconds, use the track as a provisional identity. If
      // the real tile appears later, rebind the channel without dropping audio.
      if (entry.pending.length >= 16) {
        bind(channel, {
          id: `${platform}:track:${track.id}`,
          name: "Participante sin identificar",
          avatarUrl: null,
          isSelf: false,
        });
      }
      return;
    }
    post("audio", { channel, ts: Date.now(), pcm: Array.from(samples) });
  }

  function releaseTrack(trackId, forget = false) {
    const entry = remoteTracks.get(trackId);
    if (!entry) {
      if (forget) knownTracks.delete(trackId);
      return;
    }
    remoteTracks.delete(trackId);
    entry.released = true;
    entry.reader?.cancel?.().catch(() => {});
    try { entry.processorTrack?.stop(); } catch (_) {}
    try { entry.playbackElement?.pause(); } catch (_) {}
    try { entry.playbackElement?.remove(); } catch (_) {}
    try { entry.source?.disconnect(); } catch (_) {}
    try { entry.processor?.disconnect(); } catch (_) {}
    if (entry.processor?.port) entry.processor.port.onmessage = null;
    const binding = bindings.get(entry.channel);
    meetingEvent("participant-left", { channel: entry.channel, participantId: binding?.id || null }, binding?.name || null);
    bindings.delete(entry.channel);
    identityVotes.delete(entry.channel);
    sentIdentity.delete(entry.channel);
    if (forget) knownTracks.delete(trackId);
  }

  function scanMediaElements() {
    const snapshot = identitySnapshot(true);
    sendRosterState(snapshot);
    for (const element of document.querySelectorAll("audio,video")) {
      if (element.paused) continue;
      const stream = element.srcObject;
      if (!(stream instanceof MediaStream)) continue;
      const identity = capturePolicy.mediaElementIdentity(
        platform,
        identityFrom(participantTile(element)),
      );
      for (const track of stream.getAudioTracks()) captureTrack(track, identity, null, stream);
    }
    sendMeetProbe(snapshot);
  }

  async function openLocalMicrophone(meetSenderTrack) {
    // A sender track is useful for discovering which physical microphone Meet
    // selected, but Chrome does not guarantee that feeding the same RTP source
    // into a second AudioContext will produce PCM. Open a fresh, readable
    // stream from that device and apply Meet's data-is-muted state before any
    // samples leave the page.
    const deviceId = clean(meetSenderTrack?.getSettings?.().deviceId);
    if (deviceId) {
      try {
        return await navigator.mediaDevices.getUserMedia({
          audio: { deviceId: { exact: deviceId } },
          video: false,
        });
      } catch (_) {
        // Meet can replace a device while joining. Falling back to the current
        // default is preferable to leaving the local participant silent.
      }
    }
    return navigator.mediaDevices.getUserMedia({ audio: true, video: false });
  }

  async function startMic() {
    try {
      let meetSenderTrack = null;
      if (platform === "google_meet") {
        const candidates = [];
        for (const peer of observedPeers) {
          for (const sender of peer.getSenders?.() || []) {
            const track = sender.track;
            if (track?.kind !== "audio" || track.readyState !== "live") continue;
            const parameters = sender.getParameters?.() || {};
            const active = !(parameters.encodings || []).some((encoding) => encoding.active === false);
            candidates.push({ track, active, label: clean(track.label).toLocaleLowerCase() });
          }
        }
        meetSenderTrack = candidates.find((candidate) => candidate.active && /mic|micro|input/.test(candidate.label))?.track
          || candidates.find((candidate) => candidate.active)?.track
          || candidates[0]?.track
          || null;
      }
      micStream = await openLocalMicrophone(meetSenderTrack);
      micStreamOwned = true;
      const currentMeetUser = platform === "google_meet"
        ? [...meetUsers.values()].find((user) => user.isCurrentUser)
        : null;
      const self = currentMeetUser
        ? meetIdentityForDevice(currentMeetUser.deviceId, "self")
        : (identitySnapshot(true).roster.find(({ identity }) => identity.isSelf)?.identity || {
          id: `${platform}:self`, name: "Tú", avatarUrl: null, isSelf: true,
        });
      localIdentity = self;
      sendRosterState(identitySnapshot(true));
      for (const track of micStream.getAudioTracks()) captureTrack(track, self, MIC_CHANNEL, micStream);
    } catch (error) {
      meetingEvent("warning", { code: "microphone-unavailable", message: String(error?.message || error) });
    }
  }

  async function start(url) {
    if (running) return;
    workletUrl = url || workletUrl;
    if (!workletUrl) {
      meetingEvent("warning", { code: "worklet-unavailable", message: "No llegó el módulo de captura de audio." });
      return;
    }
    running = true;
    meetAnnouncedChannels.clear();
    sentIdentity.clear();
    meetLastFrameTimestamp.clear();
    // The page can discover Meet's roster before the local WebSocket is open.
    // Force a fresh snapshot now so the background/UI receives the full count
    // for this capture instead of treating the pre-connection value as sent.
    lastRosterFingerprint = "";
    lastActiveFingerprint = "";
    await ensureAudioGraph();
    // Enrich receiver tracks with Meet's real element MediaStreams before any
    // fallback MediaStream([track]) sources are constructed.
    scanMediaElements();
    for (const known of knownTracks.values()) {
      captureTrack(known.track, known.directIdentity, known.forcedChannel, known.sourceStream, known.receiver);
    }
    for (const route of meetRoutesBySource.values()) announceMeetRoute(route);
    scanTimer = setInterval(scanMediaElements, 1000);
    activityTimer = setInterval(() => {
      const snapshot = identitySnapshot(true);
      sendActiveSpeakerState(snapshot);
    }, 250);
    if (window === window.top) await startMic();
    post("capture-state", { state: "capturing", tracks: remoteTracks.size });
  }

  async function stop() {
    if (!running) return;
    running = false;
    clearInterval(scanTimer);
    scanTimer = null;
    clearInterval(activityTimer);
    activityTimer = null;
    if (platform === "google_meet" && window === window.top && lastActiveFingerprint) {
      meetingEvent("active-speakers", { platform, participants: [] });
    }
    if (micStreamOwned) {
      for (const track of micStream?.getTracks?.() || []) track.stop();
    }
    micStream = null;
    micStreamOwned = false;
    localIdentity = null;
    lastRosterFingerprint = "";
    lastMeetProbeAt = 0;
    lastActiveFingerprint = "";
    observedActive.clear();
    for (const route of meetRoutesBySource.values()) route.pendingAudio.length = 0;
    for (const [trackId, entry] of [...remoteTracks.entries()]) {
      // Meet's virtual receiver processors must stay attached from the instant
      // their ontrack event fires. Between recordings they only drain and close
      // frames; keeping them warm prevents a later start from receiving a
      // permanently silent decoded track.
      if (entry.virtualMeetLane && entry.track.readyState === "live") {
        entry.pcmBuffers.clear();
        entry.pcmFrames = 0;
        entry.peak = 0;
        continue;
      }
      releaseTrack(trackId, false);
    }
    meetAnnouncedChannels.clear();
    meetLastFrameTimestamp.clear();
    sentIdentity.clear();
    for (const channel of [...bindings.keys()]) {
      if (channel !== MIC_CHANNEL) bindings.delete(channel);
    }
    if (context) await context.close().catch(() => {});
    context = null;
    workletReady = null;
    post("capture-state", { state: "idle", tracks: 0 });
  }

  if (
    platform === "google_meet"
    && typeof RTCRtpReceiver === "function"
    && typeof RTCRtpReceiver.prototype.createEncodedStreams === "function"
    && !RTCRtpReceiver.prototype.createEncodedStreams.__kualiObserved
  ) {
    const originalCreateEncodedStreams = RTCRtpReceiver.prototype.createEncodedStreams;
    function kualiCreateEncodedStreams(...args) {
      const receiver = this;
      const streams = Reflect.apply(originalCreateEncodedStreams, receiver, args);
      meetEncodedStreamHookCalls += 1;
      if (!streams?.readable?.pipeThrough || typeof TransformStream !== "function") return streams;
      const observer = new TransformStream({
        transform(frame, controller) {
          try {
            const metadata = frame?.getMetadata?.() || {};
            if (receiver.track?.kind === "audio" || clean(metadata.mimeType).startsWith("audio/")) {
              observeMeetEncodedFrame(receiver, frame);
            }
          } catch (_) {
            // Observation is best-effort; the original Meet frame must always
            // continue through its unmodified receiver pipeline.
          }
          controller.enqueue(frame);
        },
      });
      return { ...streams, readable: streams.readable.pipeThrough(observer) };
    }
    Object.defineProperty(kualiCreateEncodedStreams, "__kualiObserved", { value: true });
    RTCRtpReceiver.prototype.createEncodedStreams = kualiCreateEncodedStreams;
  }

  if (platform === "google_meet" && meetProtocol && typeof window.fetch === "function") {
    const originalFetch = window.fetch;
    window.fetch = async function kualiObservedFetch(...args) {
      const response = await Reflect.apply(originalFetch, this, args);
      if (response.url === "https://meet.google.com/$rpc/google.rtc.meetings.v1.MeetingSpaceService/SyncMeetingSpaceCollections") {
        response.clone().text().then((value) => {
          applyMeetUsers(meetProtocol.decodeSyncUsersBase64(value));
        }).catch((error) => {
          if (!meetProtocolWarningSent) {
            meetProtocolWarningSent = true;
            meetingEvent("warning", {
              code: "meet-roster-decode-failed",
              message: String(error?.message || error),
            });
          }
        });
      }
      return response;
    };
  }

  const OriginalPeerConnection = window.RTCPeerConnection;
  if (typeof OriginalPeerConnection === "function") {
    const observed = new WeakSet();
    function observe(peer) {
      if (observed.has(peer)) return;
      observed.add(peer);
      observedPeers.add(peer);
      peer.addEventListener("datachannel", (event) => observeMeetDataChannel(event.channel));
      const originalCreateDataChannel = peer.createDataChannel.bind(peer);
      peer.createDataChannel = function kualiCreateDataChannel(label, options) {
        const channel = originalCreateDataChannel(label, options);
        observeMeetDataChannel(channel);
        return channel;
      };
      peer.addEventListener("track", (event) => {
        if (event.track?.kind === "audio") {
          const stream = event.streams?.find?.((candidate) => candidate instanceof MediaStream) || null;
          captureTrack(event.track, null, null, stream, event.receiver || null);
        }
      });
    }
    function KualiPeerConnection(...args) {
      const peer = new OriginalPeerConnection(...args);
      peerConstructorOptions.set(peer, args[0] || {});
      observe(peer);
      return peer;
    }
    KualiPeerConnection.prototype = OriginalPeerConnection.prototype;
    Object.setPrototypeOf(KualiPeerConnection, OriginalPeerConnection);
    window.RTCPeerConnection = KualiPeerConnection;
  }

  window.addEventListener("message", (event) => {
    if (event.source !== window || event.data?.protocol !== TO_PAGE) return;
    if (event.data.command === "start") {
      start(event.data.workletUrl).catch((error) => {
        running = false;
        meetingEvent("warning", { code: "capture-start-failed", message: String(error?.message || error) });
      });
    }
    if (event.data.command === "stop") stop();
  });

  post("capture-state", { state: "ready", platform });
})();
