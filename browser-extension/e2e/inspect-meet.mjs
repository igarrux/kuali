#!/usr/bin/env node

const port = Number(process.argv[2]);
if (!Number.isInteger(port)) {
  console.error("Usage: node e2e/inspect-meet.mjs <remote-debugging-port>");
  process.exit(2);
}

const targets = await fetch(`http://127.0.0.1:${port}/json/list`).then((response) => response.json());
const target = targets.find((item) => item.type === "page" && item.url.startsWith("https://meet.google.com/"));
if (!target?.webSocketDebuggerUrl) {
  console.error("No active Google Meet page was found");
  process.exit(3);
}

const expression = `(() => {
  const clean = (value) => String(value || "").replace(/\\s+/g, " ").trim();
  const participants = [];
  const seenIds = new Set();
  for (const node of document.querySelectorAll("[data-participant-id]")) {
    const id = clean(node.getAttribute("data-participant-id"));
    if (!id || seenIds.has(id)) continue;
    seenIds.add(id);
    const selfMarker = node.matches("[data-self-name]")
      ? node
      : node.querySelector("[data-self-name]");
    participants.push({
      id,
      name: clean(node.querySelector("span.notranslate")?.textContent
        || selfMarker?.getAttribute("data-self-name")),
      self: !!selfMarker,
      requestedId: clean(node.getAttribute("data-requested-participant-id")) || null,
      tag: node.tagName,
      classes: [...node.classList].slice(0, 12),
    });
  }

  const requestedIds = [...new Set([...document.querySelectorAll("[data-requested-participant-id]")]
    .map((node) => clean(node.getAttribute("data-requested-participant-id")))
    .filter(Boolean))];

  const media = [...document.querySelectorAll("audio,video")].map((element, index) => {
    const stream = element.srcObject instanceof MediaStream ? element.srcObject : null;
    const tile = element.closest("[data-participant-id]");
    return {
      index,
      tag: element.tagName,
      paused: element.paused,
      muted: element.muted,
      volume: element.volume,
      streamId: stream?.id || null,
      tracks: (stream?.getAudioTracks() || []).map((track) => ({
        id: track.id,
        label: track.label,
        enabled: track.enabled,
        muted: track.muted,
        readyState: track.readyState,
      })),
      elementParticipantId: clean(element.getAttribute("data-participant-id")) || null,
      closestParticipantId: clean(tile?.getAttribute("data-participant-id")) || null,
      parentAttributes: element.parentElement
        ? Object.fromEntries([...element.parentElement.attributes]
          .filter((attribute) => /participant|device|audio|stream|request/i.test(attribute.name))
          .map((attribute) => [attribute.name, attribute.value]))
        : {},
    };
  }).filter((item) => item.tracks.length);

  const selfMarkers = [...document.querySelectorAll("[data-self-name]")].map((node) => ({
    name: clean(node.getAttribute("data-self-name")),
    participantId: clean(node.closest("[data-participant-id]")?.getAttribute("data-participant-id")) || null,
    tag: node.tagName,
  }));

  const participantControls = [...document.querySelectorAll("button,[role='button']")]
    .map((node) => ({ aria: clean(node.getAttribute("aria-label")), text: clean(node.textContent) }))
    .filter(({ aria, text }) => /participant|participante|persona|people|everyone|todos/i.test(aria + " " + text))
    .slice(0, 20);

  return {
    href: location.href,
    title: document.title,
    participants,
    requestedIds,
    selfMarkers,
    participantControls,
    media,
  };
})()`;

const socket = new WebSocket(target.webSocketDebuggerUrl);
const timeout = setTimeout(() => {
  console.error("Timed out while reading Meet through CDP");
  process.exit(4);
}, 10_000);

socket.onopen = () => {
  socket.send(JSON.stringify({
    id: 1,
    method: "Runtime.evaluate",
    params: { expression, returnByValue: true },
  }));
};
socket.onmessage = (event) => {
  const message = JSON.parse(event.data);
  if (message.id !== 1) return;
  clearTimeout(timeout);
  if (message.result?.exceptionDetails) {
    console.error(JSON.stringify(message.result.exceptionDetails, null, 2));
    process.exitCode = 5;
  } else {
    console.log(JSON.stringify(message.result?.result?.value || null, null, 2));
  }
  socket.close();
};
socket.onerror = () => {
  clearTimeout(timeout);
  console.error("Could not connect to the Meet CDP target");
  process.exit(6);
};
