import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { ENGLISH_TRANSLATIONS } from "./i18n.js";

const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
const importSource = app.slice(app.indexOf("const AUDIO_IMPORT_STAGES ="), app.indexOf("async function openMeeting("));
const updateSource = app.slice(app.indexOf("function canRestartForUpdate("), app.indexOf("function renderUpdateState("));
const openMeetingSource = app.slice(app.indexOf("async function openMeeting("), app.indexOf("// --- meeting ---"));
const speakerHelpers = app.slice(app.indexOf("function isMeetingParticipant("), app.indexOf("function isWebMeeting("));
const renderSpeakersSource = app.slice(app.indexOf("function renderSpeakers("), app.indexOf("/** Groups adjacent turns"));
const eventSource = app.slice(app.indexOf("function handleEvent("), app.indexOf("function upsertUtterance("));

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function harness(invoke) {
  const document = { activeElement: null };
  const elements = new Map();
  const element = (id) => {
    if (!elements.has(id)) elements.set(id, {
      textContent: "",
      value: "",
      disabled: false,
      hidden: false,
      open: false,
      focus() { document.activeElement = this; },
      showModal() { this.open = true; },
      close() { this.open = false; },
      removeAttribute(name) { delete this[name]; },
      reportValidity() { return true; },
      set innerHTML(_) { throw new Error("Imported file names must never become HTML"); },
    });
    return elements.get(id);
  };
  const state = {
    audioImport: {
      path: null, picking: false, pending: false, cancelling: false,
      progress: null, error: "", notice: "", returnFocus: null,
    },
    liveMeetings: new Map(),
    status: "offline",
    tasksLoaded: true,
  };
  const events = [];
  element("audio-import-speaker-mode").value = "auto";
  const context = vm.createContext({
    state, document, $: element, invoke,
    t: (source, variables = {}) => source.replace(/\{(\w+)\}/g, (_, key) => variables[key]),
    renderUpdateState() {},
    refreshMeetings: async () => events.push("refresh"),
    refreshFolders: async () => events.push("folders"),
    refreshTagCatalog: async () => events.push("tags"),
    openMeeting: async (id) => events.push(["open", id]),
    toast: (...args) => events.push(["toast", ...args]),
  });
  vm.runInContext(`${importSource}\n${updateSource}`, context);
  return { api: context, state, element, document, events };
}

const submitEvent = { preventDefault() {} };

test("picker cancellation preserves selection and untrusted filenames stay text", async () => {
  let path = '/tmp/<img src=x onerror=alert(1)>.wav';
  const ui = harness(async (command) => {
    assert.equal(command, "pick_audio_file");
    return path;
  });
  ui.api.openAudioImport(ui.element("opener"));
  await ui.api.pickAudioImportFile();
  assert.equal(ui.element("audio-import-file-name").textContent, '<img src=x onerror=alert(1)>.wav');
  assert.equal(ui.element("btn-start-audio-import").disabled, false);
  path = null;
  await ui.api.pickAudioImportFile();
  assert.match(ui.state.audioImport.path, /\.wav$/);
  ui.api.closeAudioImport();
  assert.equal(ui.element("audio-import-dialog").open, false);
  assert.equal(ui.document.activeElement, ui.element("opener"));
});

test("import uses automatic count, blocks duplicate submission and opens the saved meeting", async () => {
  const work = deferred();
  const calls = [];
  const ui = harness((command, args) => {
    calls.push([command, args]);
    return work.promise;
  });
  ui.api.openAudioImport(ui.element("opener"));
  ui.state.audioImport.path = "/tmp/conversation.wav";
  const first = ui.api.startAudioImport(submitEvent);
  await ui.api.startAudioImport(submitEvent);
  assert.equal(calls.length, 1);
  assert.equal(calls[0][1].numSpeakers, null);
  assert.equal(ui.api.canRestartForUpdate(), false);
  ui.api.closeAudioImport();
  assert.equal(ui.element("audio-import-dialog").open, true);
  work.resolve("saved-123");
  await first;
  assert.equal(ui.state.tasksLoaded, false);
  assert.deepEqual(ui.events.slice(0, 4), ["refresh", "folders", "tags", ["open", "saved-123"]]);
  assert.equal(ui.element("audio-import-dialog").open, false);
  assert.equal(ui.document.activeElement, ui.element("opener"));
  assert.equal(ui.api.canRestartForUpdate(), true);
});

test("unknown totals stay indeterminate and explicit speaker counts reach the backend", async () => {
  const work = deferred();
  const ui = harness((command, args) => {
    assert.equal(args.numSpeakers, 3);
    return work.promise;
  });
  ui.state.audioImport.path = "/tmp/conversation.wav";
  ui.element("audio-import-speaker-mode").value = "exact";
  ui.element("audio-import-speaker-count").valueAsNumber = 3;
  const pending = ui.api.startAudioImport(submitEvent);
  ui.api.handleAudioImportProgress({ stage: "diarizing", done: 0, total: 0 });
  assert.equal(Object.hasOwn(ui.element("audio-import-progress-bar"), "value"), false);
  assert.equal(ui.element("audio-import-progress-detail").textContent, "");
  ui.api.handleAudioImportProgress({ stage: "transcribing", done: 5, total: 10 });
  assert.equal(ui.element("audio-import-progress-bar").value, 50);
  assert.equal(ui.element("audio-import-progress-detail").textContent, "50%");
  work.resolve("saved-123");
  await pending;
});

test("cancellation waits for the import worker, retains the file and allows retry", async () => {
  const work = deferred();
  const ui = harness((command) => command === "cancel_audio_import" ? Promise.resolve() : work.promise);
  ui.api.openAudioImport();
  ui.state.audioImport.path = "/tmp/conversation.wav";
  const pending = ui.api.startAudioImport(submitEvent);
  await ui.api.cancelAudioImport();
  assert.equal(ui.state.audioImport.pending, true);
  assert.equal(ui.state.audioImport.cancelling, true);
  assert.equal(ui.element("btn-cancel-audio-import").disabled, true);
  assert.match(ui.element("audio-import-stage").textContent, /paso actual/);
  ui.api.closeAudioImport();
  assert.equal(ui.element("audio-import-dialog").open, true);
  work.reject("audio_import_cancelled");
  await pending;
  assert.equal(ui.state.audioImport.pending, false);
  assert.equal(ui.state.audioImport.error, "");
  assert.match(ui.state.audioImport.notice, /cancelada/);
  assert.equal(ui.state.audioImport.path, "/tmp/conversation.wav");
  assert.equal(ui.element("btn-start-audio-import").disabled, false);
  assert.equal(ui.document.activeElement, ui.element("btn-start-audio-import"));
  assert.equal(ui.events.length, 0);
});

test("a failed cancel request leaves the active import visible and retryable", async () => {
  const work = deferred();
  const ui = harness((command) => command === "cancel_audio_import" ? Promise.reject("connection lost") : work.promise);
  ui.api.openAudioImport();
  ui.state.audioImport.path = "/tmp/conversation.wav";
  const pending = ui.api.startAudioImport(submitEvent);
  await ui.api.cancelAudioImport();
  assert.equal(ui.state.audioImport.pending, true);
  assert.equal(ui.state.audioImport.cancelling, false);
  assert.equal(ui.element("btn-cancel-audio-import").disabled, false);
  assert.match(ui.state.audioImport.error, /connection lost/);
  assert.equal(ui.document.activeElement, ui.element("audio-import-error"));
  work.reject("model not downloaded");
  await pending;
  assert.match(ui.state.audioImport.error, /model not downloaded/);
  assert.equal(ui.element("btn-start-audio-import").disabled, false);
});

for (const stage of ["saving", "analyzing"]) test(`${stage} is past the commit point and cannot be cancelled`, async () => {
  const work = deferred();
  const commands = [];
  const ui = harness((command) => {
    commands.push(command);
    return work.promise;
  });
  ui.api.openAudioImport();
  ui.state.audioImport.path = "/tmp/conversation.wav";
  const pending = ui.api.startAudioImport(submitEvent);
  ui.api.handleAudioImportProgress({ stage, done: 0, total: 0 });
  assert.equal(ui.element("btn-cancel-audio-import").disabled, true);
  if (stage === "analyzing") {
    assert.equal(ui.element("audio-import-stage").textContent, "Identificando participantes y creando resumen…");
    assert.equal(Object.hasOwn(ui.element("audio-import-progress-bar"), "value"), false);
  }
  await ui.api.cancelAudioImport();
  assert.deepEqual(commands, ["import_audio_file"]);
  assert.equal(ui.state.audioImport.cancelling, false);
  work.resolve("saved-123");
  await pending;
  assert.equal(ui.element("audio-import-dialog").open, false);
});

test("completed import reloads inferred names and keeps unknown speakers in the meeting view", async () => {
  const saved = {
    meta: { id: "saved-123" },
    speakers: [
      { userId: 1, displayName: "Carlos", audioKind: "mixed" },
      { userId: 2, displayName: "Participante 2", audioKind: "mixed" },
      { userId: 3, displayName: "Voces superpuestas", audioKind: "overlap" },
    ],
    utterances: [{ speakerId: 1, text: "Hola, soy Carlos" }],
    summary: { title: "Reunión de equipo" },
  };
  const commands = [];
  const ui = harness(async (command, args) => {
    commands.push(command);
    if (command === "import_audio_file") return saved.meta.id;
    assert.equal(command, "load_meeting");
    assert.equal(args.id, saved.meta.id);
    return saved;
  });
  const chips = [];
  ui.element("speakers").replaceChildren = (...children) => chips.splice(0, chips.length, ...children);
  Object.assign(ui.document, {
    createElement: () => ({ style: {}, classList: { add() {} }, children: [], appendChild(child) { this.children.push(child); } }),
    createTextNode: (text) => ({ text }),
  });
  Object.assign(ui.api, {
    talkingFor: () => new Set(),
    prepareMeetingIndex() {}, isLiveMeeting: () => false,
    showPane() {}, renderMeetingList() {},
    history: { replaceState() {} },
    renderMeeting: () => ui.api.renderSpeakers(),
    refreshMeetingIndex: async () => {}, renderStatus() {}, scrollTranscriptToEnd() {},
  });
  vm.runInContext(`${speakerHelpers}\n${renderSpeakersSource}\n${openMeetingSource}`, ui.api);
  ui.state.viewing = { meta: { id: "previous" }, speakers: [] };
  ui.api.openAudioImport();
  ui.state.audioImport.path = "/tmp/conversation.wav";
  await ui.api.startAudioImport(submitEvent);
  assert.deepEqual(commands, ["import_audio_file", "load_meeting"]);
  assert.equal(ui.state.viewing, saved);
  assert.deepEqual(chips.map((chip) => chip.children.at(-1).text), ["Carlos", "Participante 2"]);
  assert.equal(ui.state.audioImport.error, "");
  assert.equal(ui.element("audio-import-dialog").open, false);
});

test("AI failure clears summary progress while the saved audio import succeeds", async () => {
  const work = deferred();
  const ui = harness(() => work.promise);
  const rendered = [];
  ui.state.processingMeetings = new Map([["saved-123", "summarizing"], ["other", "summarizing"]]);
  ui.state.viewing = { meta: { id: "saved-123" }, summary: null };
  Object.assign(ui.api, {
    renderMeetingList: () => rendered.push("list"),
    renderSummary: () => rendered.push("summary"),
    renderMeetingProgress: () => rendered.push("progress"),
    refreshMeetingAfterSummary: async (id) => rendered.push(["reload", id]),
  });
  vm.runInContext(eventSource, ui.api);
  ui.api.openAudioImport();
  ui.state.audioImport.path = "/tmp/conversation.wav";
  const pending = ui.api.startAudioImport(submitEvent);
  ui.api.handleAudioImportProgress({ stage: "analyzing", done: 0, total: 0 });
  ui.api.handleEvent({ type: "summaryFailed", meetingId: "saved-123", message: "Provider unavailable" });
  assert.equal(ui.state.processingMeetings.has("saved-123"), false);
  assert.equal(ui.state.processingMeetings.has("other"), true);
  assert.equal(ui.state.tasksLoaded, false);
  assert.deepEqual(rendered, ["list", "summary", "progress", ["reload", "saved-123"]]);
  assert.deepEqual(ui.events[0], ["toast", "No se pudo completar el análisis con IA: Provider unavailable. La transcripción sigue disponible.", "Kuali", true]);
  assert.equal(ui.state.audioImport.error, "");
  assert.equal(ui.state.audioImport.pending, true);
  work.resolve("saved-123");
  await pending;
  assert.equal(ui.state.audioImport.error, "");
  assert.equal(ui.element("audio-import-dialog").open, false);
  assert.ok(ui.events.some((event) => event[0] === "open" && event[1] === "saved-123"));
});

test("overlapping speech has a translated group label without adding a participant chip", () => {
  const people = [
    { userId: 1, displayName: "Participante 1", audioKind: "mixed" },
    { userId: 2, displayName: "Participante 2", audioKind: "mixed" },
    { userId: 3, displayName: "Voces superpuestas", audioKind: "overlap" },
    { userId: 4, displayName: "Kuali", isBot: true },
  ];
  const chips = [];
  const context = vm.createContext({
    state: { viewing: { meta: { id: "imported" }, speakers: people } },
    talkingFor: () => new Set(),
    t: (key) => ENGLISH_TRANSLATIONS[key] ?? key,
    $: () => ({ replaceChildren: (...children) => chips.splice(0, chips.length, ...children) }),
    document: {
      createElement: () => ({ style: {}, classList: { add() {} }, children: [], appendChild(child) { this.children.push(child); } }),
      createTextNode: (text) => ({ text }),
    },
  });
  vm.runInContext(`${speakerHelpers}\n${renderSpeakersSource}`, context);
  context.renderSpeakers();
  assert.equal(chips.length, 2);
  assert.deepEqual(chips.map((chip) => chip.children.at(-1).text), ["Participante 1", "Participante 2"]);
  assert.equal(people.filter(context.isMeetingParticipant).length, 2);
  assert.equal(context.speakerDisplayName(people[2]), "Overlapping voices");
  assert.equal(context.isMeetingParticipant({ displayName: "Voces superpuestas" }), true);
});

test("audio import exposes translated actions, uncertainty and first-use download", () => {
  for (const key of [
    "Importar audio", "Diarizar y transcribir", "Cancelar importación",
    "Cancelando… Kuali terminará el paso actual.", "Archivos de audio",
    "Identificando participantes y creando resumen…",
    "No se pudo completar el análisis con IA: {error}. La transcripción sigue disponible.",
    "Si conoces la cantidad de personas, indicarla puede mejorar la separación. Las voces superpuestas y el ruido pueden reducir la precisión.",
    "El audio se procesa localmente. Se descargarán unos 33 MB para distinguir voces; descarga también un modelo de transcripción en Ajustes para transcribir sin conexión.",
    "Con el resumen automático activado, la IA configurada analizará la transcripción para deducir nombres y resumir; puede ser un servicio remoto. Solo asigna nombres con evidencia, sin confirmar identidades. También puedes importar sin IA.",
  ]) assert.ok(ENGLISH_TRANSLATIONS[key], key);
});
