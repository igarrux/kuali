/*
 * Copyright 2026 Kuali contributors
 * SPDX-License-Identifier: Apache-2.0
 * Kuali interoperability layer; the capture.v1 contract was adapted from Vexa.
 */
const FROM_PAGE = "kuali.capture.v1";
const TO_PAGE = "kuali.control.v1";
const SUGGESTION_LIFETIME_MS = 10_000;
const PRIVACY_URL = chrome.runtime.getURL("privacy.html");

function translated(key, fallback) {
  try {
    return chrome.i18n?.getMessage(key) || fallback;
  } catch (_) {
    return fallback;
  }
}

const ui = {
  recordingIndicator: translated("recordingIndicator", "Kuali is recording and transcribing"),
  moveRecordingIndicator: translated("moveRecordingIndicator", "Move recording indicator"),
  minimizeRecordingIndicator: translated("minimizeRecordingIndicator", "Minimize recording indicator"),
  expandRecordingIndicator: translated("expandRecordingIndicator", "Expand recording indicator"),
  stop: translated("stop", "Stop"),
  stopRecordingAria: translated("stopRecordingAria", "Stop Kuali recording and transcription"),
  recordSuggestionAria: translated("recordSuggestionAria", "Kuali suggestion to record the meeting"),
  recordSuggestionTitle: translated("recordSuggestionTitle", "Record this meeting with Kuali?"),
  recordSuggestionDescription: translated("recordSuggestionDescription", "Live local transcription and a summary when it ends"),
  review: translated("review", "Review"),
  closeSuggestion: translated("closeSuggestion", "Close suggestion"),
  consentTitle: translated("consentTitle", "Before recording this meeting"),
  consentDetailed: translated("consentDetailed", "Kuali will capture participant audio and identity —name, photo, and platform ID— and send them to the Kuali app on this computer to transcribe the meeting."),
  destinationDetailed: translated("destinationDetailed", "The app may share the transcript with the AI provider or webhook you configured. The extension does not sell data or retain audio as a file."),
  consentConfirmation: translated("consentDetailedConfirmation", "I confirm that I informed the participants and have permission to record and transcribe this meeting."),
  privacyPolicy: translated("privacyPolicy", "Read the privacy policy"),
  cancel: translated("cancel", "Cancel"),
  recordAndTranscribe: translated("recordAndTranscribe", "Record and transcribe"),
};

let suggestionHost = null;
let suggestionFrame = null;
let suggestionRetry = null;
let recordingHost = null;
let runtimeAvailable = true;
const suggestedMeetings = new Set();

/**
 * Reloading an extension does not remove content scripts already present in a
 * tab. Chrome invalidates their `runtime`, and `sendMessage` throws before it
 * returns a Promise, so attaching `.catch()` to the call is not sufficient.
 */
function sendRuntimeMessage(message) {
  if (!runtimeAvailable) return Promise.resolve(null);
  try {
    if (!chrome.runtime?.id) {
      runtimeAvailable = false;
      return Promise.resolve(null);
    }
    return Promise.resolve(chrome.runtime.sendMessage(message)).catch(() => null);
  } catch {
    runtimeAvailable = false;
    return Promise.resolve(null);
  }
}

function meetingInfo() {
  const host = location.hostname;
  if (host === "meet.google.com") {
    return { platform: "google_meet", meetingId: location.pathname.split("/").filter(Boolean)[0] || document.title };
  }
  if (host === "zoom.us" || host.endsWith(".zoom.us")) {
    const match = location.pathname.match(/\/(?:wc|j|w)\/([^/?#]+)/);
    return { platform: "zoom", meetingId: match?.[1] || document.title };
  }
  return {
    platform: "microsoft_teams",
    meetingId: new URL(location.href).searchParams.get("meetingId") || location.pathname.split("/").filter(Boolean).at(-1) || document.title,
  };
}

window.addEventListener("message", (event) => {
  if (event.source !== window || event.data?.protocol !== FROM_PAGE) return;
  if (window === window.top && event.data.type === "meeting-event" && event.data.kind === "roster-state") {
    maybeSuggestRecording(event.data).catch(() => {});
  }
  sendRuntimeMessage({ type: "capture-event", event: event.data });
});

chrome.runtime.onMessage.addListener((message) => {
  if (message?.type === "capture-control") {
    window.postMessage({
      protocol: TO_PAGE,
      command: message.command,
      workletUrl: chrome.runtime.getURL("src/pcm-worklet.js"),
    }, "*");
  }
  if (message?.type === "capture-status" && message.status !== "idle") {
    dismissSuggestion(false);
  }
  if (window === window.top && message?.type === "capture-status") {
    if (message.status === "capturing") showRecordingIndicator();
    if (message.status === "idle") dismissRecordingIndicator();
  }
});

async function maybeSuggestRecording(event) {
  if (location.hostname !== "meet.google.com" || suggestionHost) return;
  const participants = event.detail?.participants;
  if (!Array.isArray(participants) || participants.length === 0) {
    clearTimeout(suggestionRetry);
    suggestionRetry = null;
    return;
  }
  // Capture can identify the current user only after joining. This avoids a
  // recording suggestion on the camera/microphone preview screen.
  if (!participants.some((participant) => participant?.isSelf)) {
    clearTimeout(suggestionRetry);
    suggestionRetry = null;
    return;
  }

  const { meetingId } = meetingInfo();
  const key = meetingId || location.pathname;
  if (suggestedMeetings.has(key)) return;

  const state = await sendRuntimeMessage({ type: "capture-state" });
  if (!state || state.status !== "idle") return;
  if (!state.kualiAvailable) {
    clearTimeout(suggestionRetry);
    suggestionRetry = setTimeout(() => {
      suggestionRetry = null;
      maybeSuggestRecording(event).catch(() => {});
    }, 5_000);
    return;
  }
  clearTimeout(suggestionRetry);
  suggestionRetry = null;
  suggestedMeetings.add(key);
  showRecordingSuggestion();
}

function dismissSuggestion(remember = true) {
  clearTimeout(suggestionRetry);
  suggestionRetry = null;
  if (!suggestionHost) return;
  if (remember) suggestionHost.dataset.dismissed = "true";
  cancelAnimationFrame(suggestionFrame);
  suggestionFrame = null;
  suggestionHost.remove();
  suggestionHost = null;
}

function dismissRecordingIndicator() {
  recordingHost?.remove();
  recordingHost = null;
}

function showRecordingIndicator() {
  if (recordingHost || !document.documentElement) return;
  const host = document.createElement("div");
  host.id = "kuali-recording-indicator";
  const shadow = host.attachShadow({ mode: "closed" });
  shadow.innerHTML = `
    <style>
      :host { all: initial; }
      .indicator {
        position: fixed;
        z-index: 2147483647;
        left: 16px;
        bottom: 84px;
        box-sizing: border-box;
        display: flex;
        align-items: center;
        gap: 6px;
        max-width: calc(100vw - 36px);
        padding: 5px;
        border: 1px solid rgba(248, 113, 113, .34);
        border-radius: 999px;
        background: rgba(18, 24, 33, .96);
        color: #f2f5f7;
        box-shadow: 0 12px 34px rgba(0, 0, 0, .34);
        font: 600 12px/1.2 Google Sans, Roboto, ui-sans-serif, system-ui, sans-serif;
        backdrop-filter: blur(14px);
        animation: indicator-enter .2s ease-out both;
      }
      .indicator.dragging { cursor: grabbing; animation: none; user-select: none; }
      .drag-area {
        min-width: 0;
        min-height: 30px;
        display: flex;
        align-items: center;
        gap: 9px;
        padding: 0 6px;
        border: 0;
        border-radius: 999px;
        color: inherit;
        background: transparent;
        cursor: grab;
        touch-action: none;
      }
      .drag-area:active { cursor: grabbing; }
      .grip { color: #8b97a8; font: 700 13px/1 sans-serif; }
      .dot {
        width: 8px;
        height: 8px;
        flex: 0 0 auto;
        border-radius: 50%;
        background: #fb7185;
        box-shadow: 0 0 0 4px rgba(251, 113, 133, .13);
      }
      .label { min-width: 0; overflow-wrap: anywhere; }
      .compact-label { display: none; }
      .action {
        min-height: 30px;
        padding: 0 10px;
        border: 1px solid rgba(255, 255, 255, .12);
        border-radius: 999px;
        color: #f2f5f7;
        background: rgba(255, 255, 255, .07);
        font: 700 11px/1 Google Sans, Roboto, ui-sans-serif, system-ui, sans-serif;
        cursor: pointer;
        touch-action: manipulation;
        transition: background-color .14s ease, border-color .14s ease;
      }
      .action:hover { border-color: rgba(255, 255, 255, .22); background: rgba(255, 255, 255, .12); }
      button:focus-visible { outline: 2px solid #fda4af; outline-offset: 2px; }
      .toggle {
        width: 30px;
        padding: 0;
        font-size: 16px;
      }
      .indicator.is-compact .drag-area { gap: 7px; padding-left: 7px; }
      .indicator.is-compact .grip,
      .indicator.is-compact .label,
      .indicator.is-compact .stop { display: none; }
      .indicator.is-compact .compact-label { display: inline; }
      @keyframes indicator-enter {
        from { opacity: 0; transform: translateY(-8px); }
        to { opacity: 1; transform: translateY(0); }
      }
      @media (prefers-reduced-motion: reduce) {
        .indicator { animation: none; }
        .action { transition: none; }
      }
      @media (prefers-color-scheme: light) {
        .indicator { background: rgba(251, 252, 251, .97); color: #17201d; box-shadow: 0 12px 34px rgba(34, 61, 51, .18); }
        .action { color: #552126; background: rgba(190, 24, 93, .08); border-color: rgba(190, 24, 93, .18); }
        .action:hover { background: rgba(190, 24, 93, .13); border-color: rgba(190, 24, 93, .28); }
      }
    </style>
    <aside class="indicator is-compact" role="status" aria-live="polite" aria-label="${ui.recordingIndicator}">
      <button class="drag-area" type="button" aria-label="${ui.moveRecordingIndicator}" title="${ui.moveRecordingIndicator}">
        <span class="grip" aria-hidden="true">⠿</span>
        <span class="dot" aria-hidden="true"></span>
        <span class="compact-label" aria-hidden="true">Kuali</span>
        <span class="label" aria-hidden="true">${ui.recordingIndicator}</span>
      </button>
      <button class="action toggle" type="button" aria-label="${ui.expandRecordingIndicator}" title="${ui.expandRecordingIndicator}">+</button>
      <button class="action stop" type="button" aria-label="${ui.stopRecordingAria}">${ui.stop}</button>
    </aside>
  `;
  const indicator = shadow.querySelector(".indicator");
  const dragArea = shadow.querySelector(".drag-area");
  const toggle = shadow.querySelector(".toggle");
  let compact = true;
  let drag = null;

  const placeIndicator = (left, top) => {
    const rect = indicator.getBoundingClientRect();
    const margin = 8;
    const maxLeft = Math.max(margin, window.innerWidth - rect.width - margin);
    const maxTop = Math.max(margin, window.innerHeight - rect.height - margin);
    indicator.style.left = `${Math.min(Math.max(margin, left), maxLeft)}px`;
    indicator.style.top = `${Math.min(Math.max(margin, top), maxTop)}px`;
    indicator.style.right = "auto";
    indicator.style.bottom = "auto";
  };

  const syncCompactState = () => {
    indicator.classList.toggle("is-compact", compact);
    const label = compact ? ui.expandRecordingIndicator : ui.minimizeRecordingIndicator;
    toggle.textContent = compact ? "+" : "−";
    toggle.setAttribute("aria-label", label);
    toggle.title = label;
  };

  const finishDrag = (event) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    dragArea.releasePointerCapture?.(event.pointerId);
    drag = null;
    indicator.classList.remove("dragging");
  };

  dragArea.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    const rect = indicator.getBoundingClientRect();
    drag = {
      pointerId: event.pointerId,
      originX: event.clientX,
      originY: event.clientY,
      left: rect.left,
      top: rect.top,
    };
    dragArea.setPointerCapture?.(event.pointerId);
    indicator.classList.add("dragging");
    event.preventDefault();
  });
  dragArea.addEventListener("pointermove", (event) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    placeIndicator(
      drag.left + event.clientX - drag.originX,
      drag.top + event.clientY - drag.originY,
    );
  });
  dragArea.addEventListener("pointerup", finishDrag);
  dragArea.addEventListener("pointercancel", finishDrag);
  toggle.addEventListener("click", () => {
    compact = !compact;
    syncCompactState();
  });
  shadow.querySelector(".stop").addEventListener("click", () => {
    sendRuntimeMessage({ type: "capture-stop" });
  });
  document.documentElement.append(host);
  recordingHost = host;
}

function showRecordingSuggestion() {
  if (suggestionHost || !document.documentElement) return;
  const host = document.createElement("div");
  host.id = "kuali-recording-suggestion";
  const shadow = host.attachShadow({ mode: "closed" });
  shadow.innerHTML = `
    <style>
      :host { all: initial; }
      :host {
        --k-bg: #0b0e13;
        --k-surface: #121821;
        --k-surface-2: #171f2b;
        --k-line: rgba(207, 224, 238, .12);
        --k-text: #f2f5f7;
        --k-muted: #8b97a8;
        --k-accent: #7ddab9;
        --k-accent-strong: #49c99d;
        --k-accent-soft: rgba(125, 218, 185, .12);
      }
      .toast {
        --remaining: 1;
        position: fixed;
        z-index: 2147483647;
        top: 18px;
        left: 50%;
        transform: translateX(-50%);
        box-sizing: border-box;
        width: min(440px, calc(100vw - 32px));
        overflow: hidden;
        display: grid;
        grid-template-columns: 42px minmax(0, 1fr) auto 28px;
        gap: 12px;
        align-items: center;
        padding: 12px 12px 14px;
        border: 1px solid var(--k-line);
        border-radius: 18px;
        background: color-mix(in srgb, var(--k-surface) 96%, transparent);
        color: var(--k-text);
        box-shadow: 0 18px 54px rgba(0, 0, 0, .38), 0 2px 10px rgba(0, 0, 0, .22);
        font-family: Google Sans, Roboto, ui-sans-serif, system-ui, sans-serif;
        animation: enter .28s cubic-bezier(.2, .8, .2, 1) both;
        backdrop-filter: blur(16px);
      }
      .toast[hidden], .consent[hidden] { display: none; }
      .logo-wrap {
        width: 42px;
        height: 42px;
        display: grid;
        place-items: center;
        border-radius: 13px;
        background: var(--k-accent-soft);
        border: 1px solid color-mix(in srgb, var(--k-accent) 36%, transparent);
        box-shadow: inset 0 1px rgba(255, 255, 255, .08);
      }
      .logo { width: 27px; height: 27px; }
      .copy { min-width: 0; }
      .title { margin: 0 0 3px; font-size: 14px; font-weight: 700; line-height: 1.25; }
      .description { margin: 0; color: var(--k-muted); font-size: 12px; line-height: 1.35; }
      .record {
        min-height: 34px;
        padding: 0 14px;
        border: 0;
        border-radius: 11px;
        background: var(--k-accent);
        color: #08130f;
        font: 700 12px/1 Google Sans, Roboto, ui-sans-serif, system-ui, sans-serif;
        cursor: pointer;
        transition: transform .14s ease, background .14s ease;
      }
      .record:hover { background: var(--k-accent-strong); transform: translateY(-1px); }
      .record:active { transform: translateY(0); }
      .record:focus-visible, .close:focus-visible, .cancel:focus-visible, .privacy:focus-visible, .consent-check input:focus-visible {
        outline: 2px solid var(--k-accent);
        outline-offset: 3px;
      }
      .close {
        width: 28px;
        height: 28px;
        padding: 0;
        border: 0;
        border-radius: 9px;
        background: transparent;
        color: var(--k-muted);
        font: 18px/1 sans-serif;
        cursor: pointer;
      }
      .close:hover { color: white; background: rgba(255, 255, 255, .08); }
      .progress {
        position: absolute;
        left: 12px;
        right: 12px;
        bottom: 5px;
        height: 3px;
        overflow: hidden;
        border-radius: 999px;
        background: var(--k-surface-2);
      }
      .progress::after {
        content: "";
        display: block;
        width: calc(var(--remaining) * 100%);
        height: 100%;
        border-radius: inherit;
        background: linear-gradient(90deg, var(--k-accent-strong), var(--k-accent));
      }
      .consent {
        position: fixed;
        z-index: 2147483647;
        top: 18px;
        left: 50%;
        transform: translateX(-50%);
        box-sizing: border-box;
        width: min(500px, calc(100vw - 32px));
        max-height: calc(100vh - 36px);
        overflow: auto;
        overscroll-behavior: contain;
        padding: 18px;
        border: 1px solid var(--k-line);
        border-radius: 18px;
        background: color-mix(in srgb, var(--k-surface) 97%, transparent);
        color: var(--k-text);
        box-shadow: 0 18px 54px rgba(0, 0, 0, .42), 0 2px 10px rgba(0, 0, 0, .24);
        font-family: Google Sans, Roboto, ui-sans-serif, system-ui, sans-serif;
        backdrop-filter: blur(16px);
      }
      .consent-head { display: flex; gap: 12px; align-items: center; margin-bottom: 13px; }
      .consent h2 { margin: 0; font-size: 16px; line-height: 1.25; text-wrap: balance; }
      .consent p { margin: 0 0 9px; color: var(--k-muted); font-size: 12px; line-height: 1.5; overflow-wrap: anywhere; }
      .consent strong { color: var(--k-text); }
      .consent-check {
        display: grid;
        grid-template-columns: 18px minmax(0, 1fr);
        gap: 10px;
        align-items: start;
        margin: 14px 0;
        padding: 12px;
        border: 1px solid var(--k-line);
        border-radius: 12px;
        background: var(--k-surface-2);
        color: var(--k-text);
        font-size: 12px;
        line-height: 1.4;
        cursor: pointer;
      }
      .consent-check input { width: 16px; height: 16px; margin: 1px 0 0; accent-color: var(--k-accent); }
      .privacy { color: var(--k-accent); font-size: 12px; text-underline-offset: 3px; }
      .privacy:hover { color: var(--k-accent-strong); }
      .consent-actions { display: flex; justify-content: flex-end; gap: 9px; margin-top: 16px; }
      .cancel, .confirm {
        min-height: 36px;
        padding: 0 14px;
        border-radius: 11px;
        font: 700 12px/1 Google Sans, Roboto, ui-sans-serif, system-ui, sans-serif;
        cursor: pointer;
        transition: background-color .14s ease, border-color .14s ease, opacity .14s ease;
      }
      .cancel { border: 1px solid var(--k-line); color: var(--k-text); background: transparent; }
      .cancel:hover { background: rgba(255, 255, 255, .07); }
      .confirm { border: 0; color: #08130f; background: var(--k-accent); }
      .confirm:hover:not(:disabled) { background: var(--k-accent-strong); }
      .confirm:disabled { cursor: not-allowed; opacity: .45; }
      @keyframes enter {
        from { opacity: 0; transform: translate(-50%, -12px) scale(.97); }
        to { opacity: 1; transform: translate(-50%, 0) scale(1); }
      }
      @media (max-width: 520px) {
        .toast { grid-template-columns: 38px minmax(0, 1fr) 28px; }
        .logo-wrap { width: 38px; height: 38px; }
        .record { grid-column: 2; justify-self: start; }
        .close { grid-column: 3; grid-row: 1; }
      }
      @media (prefers-reduced-motion: reduce) {
        .toast { animation: none; }
        .record, .cancel, .confirm { transition: none; }
      }
      @media (prefers-color-scheme: light) {
        :host {
          --k-bg: #f2f5f3;
          --k-surface: #fbfcfb;
          --k-surface-2: #f0f4f2;
          --k-line: rgba(27, 48, 40, .12);
          --k-text: #17201d;
          --k-muted: #6c7974;
          --k-accent: #188f69;
          --k-accent-strong: #117655;
          --k-accent-soft: rgba(24, 143, 105, .1);
        }
        .toast { box-shadow: 0 18px 54px rgba(34, 61, 51, .16), 0 2px 10px rgba(34, 61, 51, .08); }
      }
    </style>
    <section class="toast" role="dialog" aria-label="${ui.recordSuggestionAria}">
      <div class="logo-wrap"><img class="logo" width="27" height="27" alt="" src="${chrome.runtime.getURL("kuali-logo.svg")}"></div>
      <div class="copy">
        <p class="title">${ui.recordSuggestionTitle}</p>
        <p class="description">${ui.recordSuggestionDescription}</p>
      </div>
      <button class="record" type="button">${ui.review}</button>
      <button class="close" type="button" aria-label="${ui.closeSuggestion}">×</button>
      <div class="progress" aria-hidden="true"></div>
    </section>
    <section class="consent" role="dialog" aria-labelledby="kuali-consent-title" hidden>
      <div class="consent-head">
        <div class="logo-wrap"><img class="logo" width="27" height="27" alt="" src="${chrome.runtime.getURL("kuali-logo.svg")}"></div>
        <h2 id="kuali-consent-title">${ui.consentTitle}</h2>
      </div>
      <p>${ui.consentDetailed}</p>
      <p>${ui.destinationDetailed}</p>
      <label class="consent-check">
        <input type="checkbox" name="participant-consent" />
        <span>${ui.consentConfirmation}</span>
      </label>
      <a class="privacy" href="${PRIVACY_URL}" target="_blank" rel="noreferrer">${ui.privacyPolicy}</a>
      <div class="consent-actions">
        <button class="cancel" type="button">${ui.cancel}</button>
        <button class="confirm" type="button" disabled>${ui.recordAndTranscribe}</button>
      </div>
    </section>
  `;
  document.documentElement.append(host);
  suggestionHost = host;

  const toast = shadow.querySelector(".toast");
  const consent = shadow.querySelector(".consent");
  const consentCheck = shadow.querySelector(".consent-check input");
  const confirm = shadow.querySelector(".confirm");
  shadow.querySelector(".record").addEventListener("click", () => {
    cancelAnimationFrame(suggestionFrame);
    suggestionFrame = null;
    toast.hidden = true;
    consent.hidden = false;
    consentCheck.focus();
  });
  shadow.querySelector(".close").addEventListener("click", () => dismissSuggestion());
  shadow.querySelector(".cancel").addEventListener("click", () => dismissSuggestion());
  host.addEventListener("keydown", (event) => {
    if (event.key === "Escape") dismissSuggestion();
  });
  consentCheck.addEventListener("change", () => {
    confirm.disabled = !consentCheck.checked;
  });
  confirm.addEventListener("click", () => {
    if (!consentCheck.checked) {
      consentCheck.focus();
      return;
    }
    sendRuntimeMessage({ type: "capture-start" });
    dismissSuggestion();
  });

  const startedAt = performance.now();
  const update = (now) => {
    if (suggestionHost !== host) return;
    const remainingMs = Math.max(0, SUGGESTION_LIFETIME_MS - (now - startedAt));
    toast.style.setProperty("--remaining", String(remainingMs / SUGGESTION_LIFETIME_MS));
    if (remainingMs === 0) {
      dismissSuggestion();
      return;
    }
    suggestionFrame = requestAnimationFrame(update);
  };
  suggestionFrame = requestAnimationFrame(update);
}

sendRuntimeMessage({
  type: "frame-ready",
  ...(window === window.top ? { ...meetingInfo(), title: document.title } : {}),
});

if (window === window.top) {
  window.addEventListener("pagehide", () => {
    sendRuntimeMessage({ type: "capture-stop" });
  }, { once: true });
}
