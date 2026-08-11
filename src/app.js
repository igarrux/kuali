/* Kuali — desktop interface.
 *
 * The engine is the source of truth. This layer only consumes events and
 * renders state; it contains no business logic. */

import {
  currentLocale,
  localizeStaticDocument,
  setLanguagePreference,
  t,
} from "./i18n.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const state = {
  status: "offline",
  modelState: { state: "absent" },
  webMeetings: { enabled: true, port: 9099, listening: false },
  meetings: [],
  /** Complete meeting currently displayed. */
  viewing: null,
  /** ID of the meeting currently being recorded, if any. */
  liveId: null,
  liveMeta: null,
  /** Complete live meetings indexed by public ID. */
  liveMeetings: new Map(),
  config: null,
  models: [],
  modelsDirectory: "",
  customVocabulary: [],
  settingsTab: "discord",
  /** Provider catalog with availability state. */
  providers: [],
  /** Unsaved provider settings while the modal is open. */
  providerSettings: {},
  /** Models published by each provider during this session, keyed by ID. */
  providerModels: {},
  /** Subscriptions edited in Settings but not yet saved. */
  webhooks: [],
  webhookChannels: [],
  /** Selected provider ID; empty means automatic selection. */
  selectedProvider: "",
  libraryQuery: "",
  libraryRequest: 0,
  collapsedChannels: new Set(),
  librarySelectionMode: false,
  selectedMeetingIds: new Set(),
  selectedMeetingNames: new Map(),
  libraryScrollTop: 0,
  libraryContextTarget: null,
  libraryContextReturnFocus: null,
  confirmationResolve: null,
  confirmationReturnFocus: null,
  factoryResetReturnFocus: null,
  factoryResetPending: false,
  guideImageReturnFocus: null,
  taskAssigneeFilter: "all",
  currentPane: "idle",
  meetingInsightTab: "summary",
  tasks: [],
  tasksLoaded: false,
  taskPeople: [],
  taskFilters: {
    query: "",
    people: new Set(),
    meeting: "all",
    dateFrom: "",
    dateTo: "",
    status: "pending",
  },
  taskRenderLimit: 250,
  extensionPath: "",
  discordGuideStep: 0,
  meetGuideStep: 0,
  autostartEnabled: false,
  updateInfo: null,
  updateCurrentVersion: null,
  updateStatus: "idle",
  updateProgress: null,
  updateTimer: null,
  updateBootTimer: null,
  updateAutomaticAttempted: null,
  /** IDs with an open speech turn, used by the speaking indicator. */
  talking: new Map(),
  /** Ephemeral drafts keyed by turn ID; never included in summaries. */
  liveDrafts: new Map(),
  elapsedTimer: null,
};

const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
const UPDATE_BOOT_DELAY_MS = 15 * 1000;

function isLiveMeeting(id) {
  return Boolean(id) && state.liveMeetings.has(id);
}

function summariesEnabled(config = state.config) {
  return config?.llm?.["summarize-on-leave"] !== false;
}

function talkingFor(meetingId) {
  if (!state.talking.has(meetingId)) state.talking.set(meetingId, new Set());
  return state.talking.get(meetingId);
}

function isWebMeeting(meta) {
  return ["Google Meet", "Microsoft Teams", "Zoom", "Reunión web"].includes(meta?.guildName);
}

function meetingForEvent(meetingId) {
  return state.liveMeetings.get(meetingId)
    ?? (state.viewing?.meta.id === meetingId ? state.viewing : null);
}

function applyLiveSnapshot(snapshot, selectCurrent = false) {
  const meetings = snapshot.currentMeetings
    ?? (snapshot.currentMeeting ? [snapshot.currentMeeting] : []);
  state.liveMeetings.clear();
  for (const meeting of meetings) state.liveMeetings.set(meeting.meta.id, meeting);
  state.liveId = snapshot.currentMeeting?.meta.id ?? meetings.at(-1)?.meta.id ?? null;
  state.liveMeta = snapshot.currentMeeting?.meta ?? meetings.at(-1)?.meta ?? null;
  if (selectCurrent && state.liveId) {
    state.viewing = state.liveMeetings.get(state.liveId);
  } else if (state.viewing && state.liveMeetings.has(state.viewing.meta.id)) {
    state.viewing = state.liveMeetings.get(state.viewing.meta.id);
  }
}

// --- utilities ------------------------------------------------------------

function timestamp(ms) {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n) => String(n).padStart(2, "0");
  return h > 0 ? `${pad(h)}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}

function humanBytes(bytes) {
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  return `${Math.round(bytes / 1e6)} MB`;
}

function canRestartForUpdate() {
  return state.liveMeetings.size === 0 && ["offline", "watching"].includes(state.status);
}

function renderUpdateState() {
  const info = state.updateInfo;
  const busy = ["checking", "installing"].includes(state.updateStatus);
  const safeToRestart = canRestartForUpdate();
  const version = info?.version || "";
  let statusText = t("Kuali comprobará periódicamente si hay una versión nueva.");

  if (state.updateStatus === "checking") {
    statusText = t("Buscando actualizaciones…");
  } else if (state.updateStatus === "installing") {
    const progress = state.updateProgress?.totalBytes
      ? `${Math.min(100, Math.round((state.updateProgress.downloadedBytes / state.updateProgress.totalBytes) * 100))}%`
      : state.updateProgress?.downloadedBytes
        ? humanBytes(state.updateProgress.downloadedBytes)
        : "";
    statusText = progress
      ? t("Descargando Kuali {version}: {progress}", { version, progress })
      : t("Descargando Kuali {version}…", { version });
  } else if (info) {
    statusText = safeToRestart
      ? t("Kuali {version} está disponible.", { version })
      : t("La actualización se instalará cuando termine la actividad actual.");
  } else if (state.updateStatus === "current") {
    statusText = t("Tienes la versión más reciente.");
  } else if (state.updateStatus === "error") {
    statusText = t("No se pudo buscar actualizaciones.");
  }

  const settingsStatus = $("update-settings-status");
  if (settingsStatus) settingsStatus.textContent = statusText;
  const checkButton = $("btn-check-update");
  if (checkButton) {
    checkButton.disabled = busy;
    checkButton.textContent = state.updateStatus === "checking"
      ? t("Buscando actualizaciones…")
      : t("Buscar ahora");
  }

  for (const id of ["btn-install-update", "btn-settings-install-update"]) {
    const button = $(id);
    if (!button) continue;
    button.hidden = !info;
    button.disabled = busy || !safeToRestart;
    button.textContent = state.updateStatus === "installing"
      ? t("Descargando Kuali {version}…", { version })
      : t("Actualizar y reiniciar");
    button.title = info && !safeToRestart
      ? t("Termina la reunión o el resumen antes de reiniciar Kuali.")
      : "";
  }

  const banner = $("update-banner");
  if (banner) {
    banner.hidden = !info;
    $("update-banner-title").textContent = t("Hay una actualización de Kuali");
    $("update-banner-detail").textContent = state.updateStatus === "installing"
      ? statusText
      : safeToRestart
        ? t("Versión {version} lista para instalar.", { version })
        : t("La actualización se instalará cuando termine la actividad actual.");
  }
}

async function installAvailableUpdate({ automatic = false } = {}) {
  if (!state.updateInfo || state.updateStatus === "installing") return;
  if (!canRestartForUpdate()) {
    if (!automatic) {
      toast(
        t("Termina la reunión o el resumen antes de reiniciar Kuali."),
        t("Actualizaciones"),
        true,
      );
    }
    renderUpdateState();
    return;
  }
  const version = state.updateInfo.version;
  state.updateStatus = "installing";
  state.updateProgress = null;
  renderUpdateState();
  try {
    const installed = await invoke("install_update");
    if (!installed) {
      state.updateInfo = null;
      state.updateStatus = "current";
      renderUpdateState();
      if (!automatic) toast(t("No hay una actualización pendiente."), t("Actualizaciones"));
      return;
    }
    toast(t("Actualización instalada; reiniciando…"), t("Actualizaciones"));
  } catch (error) {
    state.updateStatus = "available";
    renderUpdateState();
    toast(
      `${t("No se pudo instalar la actualización.")} ${String(error)}`,
      t("Actualizaciones"),
      true,
    );
    state.updateAutomaticAttempted = version;
  }
}

function maybeInstallUpdateAutomatically() {
  const version = state.updateInfo?.version;
  if (!version || state.config?.application?.["automatic-updates"] === false) return;
  if (!canRestartForUpdate() || state.updateAutomaticAttempted === version) return;
  state.updateAutomaticAttempted = version;
  installAvailableUpdate({ automatic: true });
}

async function checkForUpdates({ manual = false } = {}) {
  if (["checking", "installing"].includes(state.updateStatus)) return;
  state.updateStatus = "checking";
  renderUpdateState();
  try {
    const info = await invoke("check_for_update");
    state.updateInfo = info;
    state.updateCurrentVersion = info?.currentVersion || state.updateCurrentVersion;
    state.updateStatus = info ? "available" : "current";
    state.updateProgress = null;
    renderUpdateState();
    if (info && !manual) maybeInstallUpdateAutomatically();
  } catch (error) {
    state.updateStatus = state.updateInfo ? "available" : "error";
    renderUpdateState();
    if (manual) {
      toast(
        `${t("No se pudo buscar actualizaciones.")} ${String(error)}`,
        t("Actualizaciones"),
        true,
      );
    }
  }
}

function scheduleAutomaticUpdateChecks() {
  clearTimeout(state.updateBootTimer);
  clearInterval(state.updateTimer);
  state.updateBootTimer = null;
  state.updateTimer = null;
  if (state.config?.application?.["automatic-updates"] === false) return;
  state.updateBootTimer = setTimeout(() => checkForUpdates(), UPDATE_BOOT_DELAY_MS);
  state.updateTimer = setInterval(() => checkForUpdates(), UPDATE_CHECK_INTERVAL_MS);
}

function shortDate(iso) {
  const d = new Date(iso);
  return d.toLocaleString(currentLocale(), {
    day: "2-digit",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function meetingTitle(meta) {
  if (meta.displayTitle?.trim()) return meta.displayTitle.trim();
  if (["Google Meet", "Microsoft Teams", "Zoom"].includes(meta.guildName)) {
    return t("Reunión de {platform}", { platform: meta.guildName });
  }
  return `${meta.guildName} · ${meta.channelName}`;
}

function libraryMeetingName(meta) {
  return `${meetingTitle(meta)} · ${shortDate(meta.startedAt)}`;
}

function liveMeetingTitle(meeting) {
  if (meeting.meta.displayTitle?.trim()) return meeting.meta.displayTitle.trim();
  const people = meeting.speakers.filter((speaker) => !speaker.isBot).map((speaker) => speaker.displayName);
  if (people.length === 1) return t("Sesión de {person}", { person: people[0] });
  if (people.length === 2) return t("{first} y {second}", { first: people[0], second: people[1] });
  if (people.length > 2) {
    return t("{first}, {second} y {count} más", {
      first: people[0],
      second: people[1],
      count: people.length - 2,
    });
  }
  return meetingTitle(meeting.meta);
}

function settleConfirmation(accepted) {
  const resolve = state.confirmationResolve;
  if (!resolve) return;
  state.confirmationResolve = null;
  $("confirm-modal").hidden = true;
  resolve(accepted);
  state.confirmationReturnFocus?.focus?.();
  state.confirmationReturnFocus = null;
}

function askForConfirmation({ kind, title, target, description, action = t("Eliminar") }) {
  if (state.confirmationResolve) settleConfirmation(false);
  const returnFocus = state.libraryContextReturnFocus ?? document.activeElement;
  closeLibraryContextMenu();
  state.confirmationReturnFocus = returnFocus;
  $("confirm-kind").textContent = t(kind);
  $("confirm-title").textContent = t(title);
  $("confirm-target").textContent = target;
  $("confirm-description").textContent = t(description);
  $("btn-confirm-accept").textContent = t(action);
  $("confirm-modal").hidden = false;
  requestAnimationFrame(() => $("btn-confirm-cancel").focus());
  return new Promise((resolve) => {
    state.confirmationResolve = resolve;
  });
}

const FACTORY_RESET_CONFIRMATION = "Dejar Kuali como recién instalado";

function factoryResetPhrase() {
  return t(FACTORY_RESET_CONFIRMATION);
}

function refreshFactoryResetConfirmation({ clear = false } = {}) {
  const phrase = factoryResetPhrase();
  const input = $("factory-reset-input");
  $("factory-reset-phrase").textContent = phrase;
  if (clear) input.value = "";
  $("btn-confirm-factory-reset").disabled =
    state.factoryResetPending || input.value !== phrase;
}

function openFactoryResetDialog() {
  state.factoryResetReturnFocus = document.activeElement;
  state.factoryResetPending = false;
  $("factory-reset-status").textContent = "";
  $("factory-reset-modal").hidden = false;
  refreshFactoryResetConfirmation({ clear: true });
  requestAnimationFrame(() => $("factory-reset-input").focus());
}

function closeFactoryResetDialog() {
  if (state.factoryResetPending || $("factory-reset-modal").hidden) return;
  $("factory-reset-modal").hidden = true;
  $("factory-reset-input").value = "";
  state.factoryResetReturnFocus?.focus?.();
  state.factoryResetReturnFocus = null;
}

async function performFactoryReset() {
  const input = $("factory-reset-input");
  if (input.value !== factoryResetPhrase() || state.factoryResetPending) return;

  state.factoryResetPending = true;
  input.disabled = true;
  $("btn-cancel-factory-reset").disabled = true;
  const button = $("btn-confirm-factory-reset");
  button.disabled = true;
  button.textContent = t("Borrando y reiniciando…");
  $("factory-reset-status").textContent = t("Kuali se reiniciará como una instalación nueva.");

  try {
    await invoke("factory_reset", { confirmation: input.value });
    localStorage.clear();
  } catch (error) {
    state.factoryResetPending = false;
    input.disabled = false;
    $("btn-cancel-factory-reset").disabled = false;
    button.textContent = t("Borrar todo y reiniciar");
    $("factory-reset-status").textContent = t("No se pudo restablecer Kuali: {error}", {
      error: String(error),
    });
    refreshFactoryResetConfirmation();
    input.focus();
  }
}

function toast(message, source = "", isError = false) {
  const el = document.createElement("div");
  el.className = `toast${isError ? " error" : ""}`;
  if (source) {
    const tag = document.createElement("span");
    tag.className = "toast-source";
    tag.textContent = source;
    el.appendChild(tag);
  }
  el.appendChild(document.createTextNode(message));
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), isError ? 9000 : 4500);
}

function hasAutomaticFollowTarget(config = state.config) {
  return Boolean(
    config?.discord?.["follow-user-id"] || config?.discord?.["follow-username"]?.trim(),
  );
}

function normalizedDiscordUsername(value) {
  return value.trim().replace(/^@+/, "").trim();
}

function isAutomaticFollowEnabled(config = state.config) {
  return (
    hasAutomaticFollowTarget(config) && config?.discord?.["follow-automatically"] !== false
  );
}

// --- status bar -----------------------------------------------------------

const STATUS_TEXT = {
  offline: ["Desconectado", ""],
  watching: ["Esperando llamada", "watching"],
  joining: ["Entrando…", "working"],
  recording: ["Transcribiendo", "recording"],
  finalizing: ["Terminando transcripción…", "working"],
  summarizing: ["Sacando el resumen…", "working"],
};

function renderWebListenerStatus() {
  const badge = $("web-listener-status");
  if (!badge) return;
  const web = state.webMeetings;
  badge.className = "listener-state";
  if (!web.enabled) {
    badge.textContent = t("Desactivado");
    badge.classList.add("off");
  } else if (web.listening) {
    badge.textContent = t("Escuchando · {port}", { port: web.port });
    badge.classList.add("ready");
  } else {
    badge.textContent = t("Puerto {port} no disponible", { port: web.port });
  }
}

function renderStatus() {
  const modelPreparation =
    state.status === "joining"
      ? {
          verifying: t("Verificando modelo…"),
          loading: t("Cargando Whisper…"),
        }[state.modelState.state]
      : null;
  let [text, dotClass] = modelPreparation
    ? [modelPreparation, "working"]
    : STATUS_TEXT[state.status] ?? ["—", ""];
  text = t(text);
  if (state.status === "recording" && state.liveMeetings.size > 1) {
    text = t("Transcribiendo {count} reuniones", { count: state.liveMeetings.size });
  }
  $("status-text").textContent = text;
  $("status-dot").className = `status-dot ${dotClass}`;

  const connected = state.status !== "offline";
  const hasFollowTarget = hasAutomaticFollowTarget();
  const automaticFollow = isAutomaticFollowEnabled();
  $("ready-discord").textContent = connected ? t("Conectado") : t("Sin conexión");
  $("ready-discord-dot").classList.toggle("off", !connected);
  $("ready-follow").textContent =
    modelPreparation
      ? t("En llamada")
      : state.status === "joining"
        ? t("Entrando…")
      : connected
        ? automaticFollow
          ? t("Automático")
          : hasFollowTarget
            ? t("Manual")
            : t("/grabar listo")
        : t("En pausa");
  $("ready-follow-dot").classList.toggle("off", !connected);
  $("ready-follow-dot").classList.toggle("busy", state.status === "joining");

  const web = state.webMeetings;
  $("ready-web").textContent = !web.enabled
    ? t("Desactivada")
    : web.listening
      ? t("Lista · puerto {port}", { port: web.port })
      : t("No disponible · {port}", { port: web.port });
  $("ready-web-dot").classList.toggle("off", !web.enabled || !web.listening);
  renderWebListenerStatus();

  const followButton = $("btn-toggle-follow");
  followButton.hidden = !connected || !hasFollowTarget;
  followButton.textContent = automaticFollow
    ? t("Pausar seguimiento de Discord")
    : t("Activar seguimiento de Discord");
  followButton.setAttribute("aria-pressed", String(automaticFollow));
  // This setting controls future joins and applies immediately. It must not
  // interrupt a call that is already being recorded.
  followButton.disabled = false;

  const live = state.status === "recording" && isLiveMeeting(state.viewing?.meta.id);
  $("elapsed").hidden = !live;

  if (live && !state.elapsedTimer) {
    state.elapsedTimer = setInterval(updateElapsed, 1000);
    updateElapsed();
  } else if (!live && state.elapsedTimer) {
    clearInterval(state.elapsedTimer);
    state.elapsedTimer = null;
  }
}

function updateElapsed() {
  if (!state.viewing) return;
  const started = new Date(state.viewing.meta.startedAt).getTime();
  $("elapsed").textContent = timestamp(Date.now() - started);
}

// --- panes ----------------------------------------------------------------

function showPane(name) {
  for (const pane of ["setup", "idle", "tasks", "guide", "meeting"]) {
    $(`pane-${pane}`).hidden = pane !== name;
  }
  state.currentPane = name;
  const showingLibrary = ["setup", "idle", "meeting"].includes(name);
  $("sidebar-library-content").hidden = !showingLibrary;
  $("app-layout").classList.toggle("focus-section", !showingLibrary);
  const active = ["idle", "meeting"].includes(name) ? "home" : name;
  for (const [id, view] of [["nav-home", "home"], ["nav-tasks", "tasks"]]) {
    const button = $(id);
    const selected = view === active;
    button.classList.toggle("active", selected);
    if (selected) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  }
}

async function goHome() {
  state.viewing = null;
  renderStatus();
  await renderRoot();
  renderMeetingList();
  history.replaceState(null, "", "#home");
}

async function showTasks() {
  state.viewing = null;
  showPane("tasks");
  renderStatus();
  history.replaceState(null, "", "#tasks");
  await refreshTasks();
}

async function showGuide() {
  state.viewing = null;
  showPane("guide");
  renderStatus();
  history.replaceState(null, "", "#guide");
  await renderGuide();
}

async function renderRoot() {
  const automaticFollow = isAutomaticFollowEnabled();
  const modelPreparation = ["verifying", "loading"].includes(state.modelState.state);
  // A browser meeting works without Discord. Never hide active incoming audio
  // behind the bot setup flow.
  if (state.viewing) {
    showPane("meeting");
    renderMeeting();
    return;
  }
  const missing = await invoke("missing_requirements");
  if (missing.length > 0 && !state.webMeetings.enabled) {
    $("missing-list").replaceChildren(
      ...missing.map((m) => {
        const li = document.createElement("li");
        li.textContent = t(m);
        return li;
      }),
    );
    showPane("setup");
    return;
  }

  showPane("idle");
    $("idle-eyebrow").textContent = automaticFollow
      ? t("Seguimiento automático")
      : t("Entrada manual");
    const copy = {
      offline: state.webMeetings.listening
        ? [
            "Kuali está listo para una reunión web",
            t("Usa la extensión desde Meet, Teams o Zoom con el puerto {port}. Discord puede configurarse aparte.", {
              port: state.webMeetings.port,
            }),
          ]
        : [
            "Kuali está desconectado",
            "Abre Ajustes para revisar Discord o el receptor de reuniones web.",
          ],
      watching: [
        automaticFollow
          ? "Esperando a que entres a una llamada"
          : "Kuali está listo para una llamada",
        automaticFollow
          ? "Entra a un canal de voz de Discord. Kuali te seguirá y empezará a transcribir."
          : "Entra a un canal de voz y usa /grabar para invitar a Kuali.",
      ],
      joining: [
        modelPreparation ? "Kuali ya está en la llamada" : "Entrando a tu llamada…",
        modelPreparation
          ? state.modelState.state === "verifying"
            ? "Comprobando que los pesos estén íntegros antes de abrirlos. El audio queda en espera."
            : "El modelo se está cargando desde su almacenamiento. El audio queda en espera y se transcribirá en cuanto esté listo."
          : "Kuali está conectando el audio y preparando el modelo de transcripción.",
      ],
      recording: [
        "La reunión está en marcha",
        "Elige la reunión en el historial para seguir la transcripción en vivo.",
      ],
      finalizing: [
        "Terminando la transcripción…",
        "Kuali está procesando los últimos fragmentos que ya había capturado.",
      ],
      summarizing: [
        "Ordenando lo que se habló…",
        "Kuali está preparando el resumen, las decisiones y las tareas.",
      ],
    }[state.status] ?? ["Preparando Kuali…", "Esto solo tomará un momento."];
  $("idle-title").textContent = t(copy[0]);
  $("idle-sub").textContent = t(copy[1]);
}

// --- history --------------------------------------------------------------

async function refreshMeetings() {
  const request = ++state.libraryRequest;
  const query = state.libraryQuery.trim();
  const list = $("meeting-list");
  list.setAttribute("aria-busy", "true");
  if (query) $("library-search-status").textContent = t("Buscando…");

  try {
    const meetings = await invoke(
      query ? "search_meetings" : "list_meetings",
      query ? { query } : {},
    );
    if (request !== state.libraryRequest) return;
    state.meetings = meetings;
  } catch (e) {
    if (request !== state.libraryRequest) return;
    toast(String(e), t("historial"), true);
    $("library-search-status").textContent = t("No se pudo buscar.");
    return;
  } finally {
    if (request === state.libraryRequest) list.removeAttribute("aria-busy");
  }
  renderMeetingList();
}

function renderMeetingList() {
  if ($("sidebar-library-content").hidden) return;
  const list = $("meeting-list");
  const preservedScrollTop = list.scrollTop || state.libraryScrollTop;
  const query = state.libraryQuery.trim();
  const storedMeetings = state.meetings.filter((meeting) => !isLiveMeeting(meeting.id));
  $("meeting-count").textContent = storedMeetings.length || "";
  $("history-empty").hidden = storedMeetings.length > 0;
  $("history-empty").textContent = query
    ? t("No hay reuniones que coincidan con «{query}».", { query })
    : t("Tus reuniones aparecerán aquí cuando Kuali termine de escucharlas.");
  $("library-search-status").textContent = query
    ? t(storedMeetings.length === 1 ? "{count} resultado" : "{count} resultados", {
        count: storedMeetings.length,
      })
    : "";

  const channels = new Map();
  for (const meta of storedMeetings) {
    const platform = meetingPlatform(meta);
    const web = platform !== "discord";
    const key = web ? `platform:${platform}` : `${meta.guildId}:${meta.channelId}`;
    if (!channels.has(key)) {
      channels.set(key, {
        key,
        guildId: meta.guildId,
        channelId: meta.channelId,
        guildName: meta.guildName,
        channelName: web ? meta.guildName : meta.channelName,
        platform,
        web,
        meetings: [],
      });
    }
    channels.get(key).meetings.push(meta);
  }

  list.replaceChildren(
    ...[...channels.values()].map((channel) => channelGroupNode(channel, Boolean(query))),
  );
  // Rebuilding groups to mark the active selection must preserve scroll
  // position in long libraries.
  list.scrollTop = preservedScrollTop;
  state.libraryScrollTop = preservedScrollTop;
  renderLiveMeetingList();
  updateLibrarySelectionControls();
}

function meetingPlatform(meta) {
  if (meta?.guildName === "Google Meet") return "google-meet";
  if (meta?.guildName === "Microsoft Teams") return "teams";
  if (meta?.guildName === "Zoom") return "zoom";
  return "discord";
}

function platformMark(platform) {
  const mark = document.createElement("span");
  mark.className = `platform-mark ${platform}`;
  mark.setAttribute("aria-hidden", "true");
  mark.textContent = { "google-meet": "M", teams: "T", zoom: "Z", discord: "D" }[platform] ?? "K";
  return mark;
}

function renderLiveMeetingList() {
  const meetings = [...state.liveMeetings.values()]
    .sort((a, b) => new Date(b.meta.startedAt) - new Date(a.meta.startedAt));
  $("live-library").hidden = meetings.length === 0;
  $("live-meeting-count").textContent = meetings.length;
  $("live-meeting-list").replaceChildren(...meetings.map((meeting) => {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "live-meeting-item";
    if (state.viewing?.meta.id === meeting.meta.id) button.classList.add("active");
    const copy = document.createElement("span");
    copy.className = "live-meeting-copy";
    const title = document.createElement("strong");
    title.textContent = liveMeetingTitle(meeting);
    const meta = document.createElement("small");
    const participants = meeting.speakers.filter((speaker) => !speaker.isBot).length;
    meta.textContent = t("{count} {people} · Transcribiendo", {
      count: participants || "—",
      people: t(participants === 1 ? "participante" : "participantes"),
    });
    copy.append(title, meta);
    const pulse = document.createElement("span");
    pulse.className = "live-pulse";
    pulse.setAttribute("aria-hidden", "true");
    button.append(platformMark(meetingPlatform(meeting.meta)), copy, pulse);
    button.addEventListener("click", () => openMeeting(meeting.meta.id));
    item.appendChild(button);
    return item;
  }));
}

function selectableMeetingIds() {
  return state.meetings
    .map((meeting) => meeting.id)
    .filter((id) => !isLiveMeeting(id));
}

function setLibrarySelectionMode(enabled) {
  state.librarySelectionMode = enabled;
  if (!enabled) {
    state.selectedMeetingIds.clear();
    state.selectedMeetingNames.clear();
  }
  renderMeetingList();
}

function setMeetingSelection(meta, selected) {
  if (isLiveMeeting(meta.id)) return;
  if (selected) {
    state.selectedMeetingIds.add(meta.id);
    state.selectedMeetingNames.set(meta.id, libraryMeetingName(meta));
  } else {
    state.selectedMeetingIds.delete(meta.id);
    state.selectedMeetingNames.delete(meta.id);
  }
}

function toggleMeetingSelection(meta, selected = !state.selectedMeetingIds.has(meta.id)) {
  setMeetingSelection(meta, selected);
  renderMeetingList();
}

function updateLibrarySelectionControls() {
  const selected = state.selectedMeetingIds.size;
  const visible = selectableMeetingIds();
  const allVisible = visible.length > 0 && visible.every((id) => state.selectedMeetingIds.has(id));
  $("library-bulk-actions").hidden = !state.librarySelectionMode;
  $("library-selection-count").textContent = t(
    selected === 1 ? "{count} seleccionada" : "{count} seleccionadas",
    { count: selected },
  );
  $("btn-delete-selected").disabled = selected === 0;
  $("btn-select-visible").disabled = visible.length === 0;
  $("btn-select-visible").textContent = allVisible ? t("Ninguna") : t("Todas");
}

function channelGroupNode(channel, searching) {
  const group = document.createElement("li");
  group.className = "channel-group";

  const head = document.createElement("div");
  head.className = "channel-head";

  const expanded = searching || !state.collapsedChannels.has(channel.key);
  const meetingsId = `channel-${channel.key.replace(/[^a-zA-Z0-9_-]/g, "-")}`;
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "channel-toggle";
  toggle.title = "Clic derecho o mantén presionado para más acciones";
  toggle.setAttribute("aria-expanded", String(expanded));
  toggle.setAttribute("aria-controls", meetingsId);
  if (searching) {
    toggle.classList.add("searching");
    toggle.setAttribute("aria-disabled", "true");
  }

  const chevron = document.createElement("span");
  chevron.className = "channel-chevron";
  chevron.setAttribute("aria-hidden", "true");
  chevron.textContent = "›";

  const names = document.createElement("span");
  names.className = "channel-label";
  const name = document.createElement("strong");
  name.className = "channel-name";
  name.textContent = channel.web ? channel.channelName : `# ${channel.channelName}`;
  const guild = document.createElement("small");
  guild.textContent = channel.web ? t("Reuniones del navegador") : channel.guildName;
  names.append(name, guild);

  const count = document.createElement("span");
  count.className = "channel-count";
  count.textContent = channel.meetings.length;
  toggle.append(chevron, platformMark(channel.platform), names, count);
  toggle.addEventListener("click", () => {
    if (searching) return;
    if (expanded) state.collapsedChannels.add(channel.key);
    else state.collapsedChannels.delete(channel.key);
    renderMeetingList();
  });

  if (state.librarySelectionMode) {
    head.classList.add("selecting");
    const selector = document.createElement("input");
    selector.type = "checkbox";
    selector.className = "channel-selector";
    const selectable = channel.meetings.filter((meeting) => !isLiveMeeting(meeting.id));
    const selected = selectable.filter((meeting) => state.selectedMeetingIds.has(meeting.id)).length;
    selector.checked = selectable.length > 0 && selected === selectable.length;
    selector.indeterminate = selected > 0 && selected < selectable.length;
    selector.disabled = selectable.length === 0;
    selector.setAttribute(
      "aria-label",
      t("Seleccionar la carpeta de # {channel}", { channel: channel.channelName }),
    );
    selector.addEventListener("change", () => {
      for (const meeting of selectable) setMeetingSelection(meeting, selector.checked);
      renderMeetingList();
    });
    head.append(selector, toggle);
  } else {
    head.append(toggle);
  }
  bindLibraryContextMenu(head, { kind: "channel", channel, searching });

  const meetings = document.createElement("ul");
  meetings.id = meetingsId;
  meetings.className = "channel-meetings";
  meetings.hidden = !expanded;
  meetings.append(...channel.meetings.map(meetingListNode));
  group.append(head, meetings);
  return group;
}

function meetingListNode(meta) {
  const entry = document.createElement("li");
  entry.className = "meeting-entry";
  if (state.librarySelectionMode) entry.classList.add("selecting");

  const button = document.createElement("button");
  button.type = "button";
  button.className = "meeting-item";
  button.title = "Clic derecho o mantén presionado para más acciones";
  button.dataset.id = meta.id;
  if (meta.id === state.viewing?.meta.id) {
    button.classList.add("active");
    button.setAttribute("aria-current", "true");
  }
  if (isLiveMeeting(meta.id)) button.classList.add("live");

  const title = document.createElement("span");
  title.className = "m-title";
  title.textContent = meetingTitle(meta);

  const kind = document.createElement("span");
  kind.className = "m-date";
  kind.textContent =
    meta.searchMatch?.source
      ? `${shortDate(meta.startedAt)} · ${meta.searchMatch.source}`
      : `${shortDate(meta.startedAt)} · Reunión guardada`;

  button.append(title, kind);
  if (meta.searchMatch) {
    const snippet = document.createElement("span");
    snippet.className = "m-snippet";
    const match = document.createElement("mark");
    match.textContent = meta.searchMatch.matched;
    snippet.append(
      document.createTextNode(meta.searchMatch.before),
      match,
      document.createTextNode(meta.searchMatch.after),
    );
    button.appendChild(snippet);
  }
  if (state.librarySelectionMode) {
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.className = "meeting-selector";
    checkbox.checked = state.selectedMeetingIds.has(meta.id);
    checkbox.disabled = isLiveMeeting(meta.id);
    checkbox.setAttribute(
      "aria-label",
      t("Seleccionar reunión del {title}", { title: title.textContent }),
    );
    if (checkbox.checked) button.classList.add("selected");
    if (checkbox.disabled) {
      checkbox.title = "La reunión en curso no se puede eliminar";
      button.addEventListener("click", () => openMeeting(meta.id));
    } else {
      checkbox.addEventListener("change", () => toggleMeetingSelection(meta, checkbox.checked));
      button.setAttribute("aria-pressed", String(checkbox.checked));
      button.addEventListener("click", () => toggleMeetingSelection(meta));
    }
    entry.append(checkbox, button);
  } else {
    button.addEventListener("click", () => openMeeting(meta.id));
    entry.appendChild(button);
  }
  bindLibraryContextMenu(button, { kind: "meeting", meeting: meta });
  return entry;
}

function contextTargetMeetings(target) {
  return target.kind === "meeting" ? [target.meeting] : target.channel.meetings;
}

function meetingBelongsToChannel(meta, channel) {
  if (channel.web) return meetingPlatform(meta) === channel.platform;
  return meta.guildId === channel.guildId && meta.channelId === channel.channelId;
}

function closeLibraryContextMenu(restoreFocus = false) {
  $("library-context-menu").hidden = true;
  state.libraryContextTarget = null;
  if (restoreFocus) state.libraryContextReturnFocus?.focus?.();
  state.libraryContextReturnFocus = null;
}

function openLibraryContextMenu(x, y, target, returnFocus = document.activeElement) {
  state.libraryContextTarget = target;
  state.libraryContextReturnFocus = returnFocus;
  const selectable = contextTargetMeetings(target).filter((meeting) => !isLiveMeeting(meeting.id));
  const allSelected =
    selectable.length > 0 &&
    selectable.every((meeting) => state.selectedMeetingIds.has(meeting.id));
  const containsLive = target.kind === "meeting"
    ? isLiveMeeting(target.meeting.id)
    : [...state.liveMeetings.values()].some((meeting) =>
      meetingBelongsToChannel(meeting.meta, target.channel));
  const select = $("context-select");
  const remove = $("context-delete");
  select.disabled = selectable.length === 0;
  select.textContent = allSelected
    ? "Quitar de la selección"
    : target.kind === "channel"
      ? target.searching
        ? "Seleccionar resultados"
        : "Seleccionar carpeta"
      : "Seleccionar reunión";
  remove.disabled = containsLive;
  remove.title = containsLive ? "Primero saca a Kuali de la llamada" : "";
  remove.textContent = target.kind === "channel" ? t("Eliminar carpeta…") : t("Eliminar reunión…");

  const menu = $("library-context-menu");
  menu.hidden = false;
  const bounds = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(8, Math.min(x, window.innerWidth - bounds.width - 8))}px`;
  menu.style.top = `${Math.max(8, Math.min(y, window.innerHeight - bounds.height - 8))}px`;
  requestAnimationFrame(() => [select, remove].find((item) => !item.disabled)?.focus());
}

function bindLibraryContextMenu(element, target) {
  let timer = null;
  let origin = null;
  let suppressClick = false;
  const returnFocus = element.matches("button") ? element : element.querySelector("button");
  const cancelLongPress = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  };

  element.addEventListener("contextmenu", (event) => {
    event.preventDefault();
    event.stopPropagation();
    openLibraryContextMenu(event.clientX, event.clientY, target, returnFocus);
  });
  element.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    origin = { x: event.clientX, y: event.clientY };
    cancelLongPress();
    timer = setTimeout(() => {
      suppressClick = true;
      openLibraryContextMenu(origin.x, origin.y, target, returnFocus);
    }, 520);
  });
  element.addEventListener("pointermove", (event) => {
    if (!origin || Math.hypot(event.clientX - origin.x, event.clientY - origin.y) <= 8) return;
    cancelLongPress();
  });
  for (const eventName of ["pointerup", "pointercancel", "pointerleave"]) {
    element.addEventListener(eventName, cancelLongPress);
  }
  element.addEventListener(
    "click",
    (event) => {
      if (!suppressClick) return;
      suppressClick = false;
      event.preventDefault();
      event.stopImmediatePropagation();
    },
    true,
  );
  element.addEventListener("keydown", (event) => {
    if (event.key !== "ContextMenu" && !(event.shiftKey && event.key === "F10")) return;
    event.preventDefault();
    const bounds = element.getBoundingClientRect();
    openLibraryContextMenu(bounds.left + 14, bounds.top + 14, target, returnFocus);
  });
}

function toggleContextSelection() {
  const target = state.libraryContextTarget;
  if (!target) return;
  const meetings = contextTargetMeetings(target).filter((meeting) => !isLiveMeeting(meeting.id));
  const allSelected =
    meetings.length > 0 && meetings.every((meeting) => state.selectedMeetingIds.has(meeting.id));
  state.librarySelectionMode = true;
  for (const meeting of meetings) setMeetingSelection(meeting, !allSelected);
  closeLibraryContextMenu();
  renderMeetingList();
}

async function deleteContextTarget() {
  const target = state.libraryContextTarget;
  if (!target) return;
  if (target.kind === "meeting") await deleteMeeting(target.meeting);
  else await deleteChannelGroup(target.channel);
}

async function deleteChannelGroup(channel) {
  const count = channel.meetings.length;
  const label = channel.web ? channel.channelName : `# ${channel.channelName}`;
  const accepted = await askForConfirmation({
    kind: t("Eliminar carpeta"),
    title: t("¿Eliminar esta carpeta completa?"),
    target: `${label}\n${channel.guildName}`,
    description: t(
      "Se borrarán todas las reuniones, transcripciones y resúmenes guardados de esta carpeta{visible}. Esta acción no se puede deshacer.",
      { visible: count > 0 ? t(" ({count} visibles ahora)", { count }) : "" },
    ),
    action: t("Eliminar carpeta"),
  });
  if (!accepted) return;

  try {
    const removed = await invoke("delete_channel_meetings", {
      meetingId: channel.meetings[0].id,
    });
    state.selectedMeetingIds.clear();
    state.selectedMeetingNames.clear();
    state.librarySelectionMode = false;
    if (state.viewing && meetingBelongsToChannel(state.viewing.meta, channel)) state.viewing = null;
    await refreshMeetings();
    await renderRoot();
    renderStatus();
    toast(t(removed === 1 ? "{count} reunión eliminada" : "{count} reuniones eliminadas", {
      count: removed,
    }), label);
  } catch (error) {
    toast(String(error), `eliminar ${label}`, true);
  }
}

async function deleteSelectedMeetings() {
  const ids = [...state.selectedMeetingIds];
  if (ids.length === 0) return;
  const names = ids.map((id) => state.selectedMeetingNames.get(id) ?? id);
  const shown = names.slice(0, 5);
  if (names.length > shown.length) {
    shown.push(t("…y {count} más", { count: names.length - shown.length }));
  }
  const accepted = await askForConfirmation({
    kind: t("Eliminar selección"),
    title: t(ids.length === 1 ? "¿Eliminar {count} reunión?" : "¿Eliminar {count} reuniones?", {
      count: ids.length,
    }),
    target: shown.join("\n"),
    description: t("Se borrarán sus transcripciones y resúmenes. Esta acción no se puede deshacer."),
    action: ids.length === 1
      ? t("Eliminar reunión")
      : t("Eliminar {count} reuniones", { count: ids.length }),
  });
  if (!accepted) return;

  const button = $("btn-delete-selected");
  button.disabled = true;
  button.textContent = t("Eliminando…");
  try {
    const removed = await invoke("delete_meetings", { ids });
    if (state.viewing && state.selectedMeetingIds.has(state.viewing.meta.id)) state.viewing = null;
    state.selectedMeetingIds.clear();
    state.selectedMeetingNames.clear();
    state.librarySelectionMode = false;
    await refreshMeetings();
    await renderRoot();
    renderStatus();
    toast(t(removed === 1 ? "{count} reunión eliminada" : "{count} reuniones eliminadas", {
      count: removed,
    }), t("biblioteca"));
  } catch (error) {
    toast(String(error), "eliminar reuniones", true);
  } finally {
    button.disabled = false;
    button.textContent = t("Eliminar");
    updateLibrarySelectionControls();
  }
}

async function deleteMeeting(meta, button = null) {
  if (isLiveMeeting(meta.id)) return;
  const accepted = await askForConfirmation({
    kind: t("Eliminar reunión"),
    title: t("¿Eliminar esta reunión?"),
    target: `${libraryMeetingName(meta)}\n${meta.guildName}`,
    description: t(
      "Se borrarán la transcripción, el resumen y las tareas de esta reunión. Esta acción no se puede deshacer.",
    ),
    action: t("Eliminar reunión"),
  });
  if (!accepted) return;

  if (button) button.disabled = true;
  try {
    await invoke("delete_meeting", { id: meta.id });
    state.selectedMeetingIds.delete(meta.id);
    state.selectedMeetingNames.delete(meta.id);
    if (state.viewing?.meta.id === meta.id) state.viewing = null;
    await refreshMeetings();
    await renderRoot();
    renderStatus();
    toast(t("Reunión eliminada"), libraryMeetingName(meta));
  } catch (error) {
    toast(String(error), "eliminar reunión", true);
  } finally {
    if (button) button.disabled = false;
  }
}

async function openMeeting(id) {
  try {
    state.viewing = state.liveMeetings.get(id) ?? await invoke("load_meeting", { id });
    state.taskAssigneeFilter = "all";
    state.meetingInsightTab = "summary";
  } catch (e) {
    toast(String(e), "reunión", true);
    return;
  }
  showPane("meeting");
  renderMeetingList();
  history.replaceState(null, "", `#meeting=${encodeURIComponent(id)}`);
  renderMeeting();
  renderStatus();
  scrollTranscriptToEnd();
}

// --- meeting --------------------------------------------------------------

function renderMeeting() {
  const meeting = state.viewing;
  if (!meeting) return;

  $("meeting-title").textContent = isLiveMeeting(meeting.meta.id)
    ? liveMeetingTitle(meeting)
    : meetingTitle(meeting.meta);
  const live = isLiveMeeting(meeting.meta.id);
  $("transcript-live").hidden = !live;
  $("btn-leave-call").hidden = !live || isWebMeeting(meeting.meta);
  // A live meeting remains in memory and would be saved again on completion,
  // so deletion stays hidden until the meeting has actually ended.
  $("btn-delete").hidden = live;
  $("btn-resummarize").hidden = live || !summariesEnabled();

  const last = meeting.utterances.at(-1);
  const parts = [shortDate(meeting.meta.startedAt)];
  if (last) parts.push(timestamp(last.endMs));
  const participantCount = meeting.speakers.filter((speaker) => !speaker.isBot).length;
  parts.push(t(participantCount === 1 ? "{count} participante" : "{count} participantes", {
    count: participantCount,
  }));
  parts.push(t(meeting.utterances.length === 1 ? "{count} intervención" : "{count} intervenciones", {
    count: meeting.utterances.length,
  }));
  $("meeting-meta").textContent = parts.join(" · ");

  renderSpeakers();
  renderTranscript();
  renderSummary();
  renderMeetingTasks();
  selectMeetingInsightTab(state.meetingInsightTab);
}

function renderSpeakers() {
  const speakers = state.viewing.speakers.filter((s) => !s.isBot);
  const talking = talkingFor(state.viewing.meta.id);
  $("speakers").replaceChildren(
    ...speakers.map((speaker) => {
      const chip = document.createElement("span");
      chip.className = "speaker-chip";
      if (talking.has(speaker.userId)) chip.classList.add("talking");

      if (speaker.avatarUrl) {
        const img = document.createElement("img");
        img.className = "avatar";
        img.src = speaker.avatarUrl;
        img.alt = "";
        img.width = 20;
        img.height = 20;
        img.loading = "lazy";
        chip.appendChild(img);
      } else {
        const initial = document.createElement("span");
        initial.className = "initial";
        initial.style.background = speaker.color;
        initial.textContent = (speaker.displayName[0] ?? "?").toUpperCase();
        chip.appendChild(initial);
      }

      chip.appendChild(document.createTextNode(speaker.displayName));
      return chip;
    }),
  );
}

/** Groups adjacent turns from one speaker to avoid repeating their name on
 *  every sentence. */
function groupTurns(utterances) {
  const turns = [];
  for (const u of utterances) {
    const last = turns.at(-1);
    if (
      last &&
      last.speakerId === u.speakerId &&
      Boolean(last.provisional) === Boolean(u.provisional)
    ) {
      last.text += ` ${u.text}`;
      last.endMs = u.endMs;
      last.confidence = Math.min(last.confidence ?? 1, u.confidence ?? 1);
    } else {
      turns.push({ ...u });
    }
  }
  return turns;
}

function renderTranscript() {
  const container = $("transcript");
  const meeting = state.viewing;
  const live = isLiveMeeting(meeting.meta.id);
  const talking = talkingFor(meeting.meta.id);
  const talkingNames = meeting.speakers
    .filter((speaker) => talking.has(speaker.userId))
    .map((speaker) => speaker.displayName);

  const drafts = live
    ? [...state.liveDrafts.values()].filter((draft) => draft.meetingId === meeting.meta.id)
    : [];
  const visibleUtterances = [...meeting.utterances, ...drafts]
    .sort((a, b) => a.startMs - b.startMs || a.id.localeCompare(b.id));

  if (visibleUtterances.length === 0) {
    const empty = document.createElement("p");
    empty.className = "transcript-empty";
    empty.textContent =
      live
        ? talkingNames.length > 0
          ? t("Escuchando a {people}…", { people: talkingNames.join(", ") })
          : t("Escuchando. La primera frase aparecerá en cuanto haya una pausa.")
        : t("No se transcribió nada en esta reunión.");
    container.replaceChildren(empty);
    return;
  }

  const byId = new Map(meeting.speakers.map((s) => [s.userId, s]));

  const nodes = groupTurns(visibleUtterances).map((turn) => {
      const speaker = byId.get(turn.speakerId);
      const row = document.createElement("div");
      row.className = "turn";

      const time = document.createElement("div");
      time.className = "time";
      time.textContent = timestamp(turn.startMs);

      const body = document.createElement("div");
      const who = document.createElement("div");
      who.className = "who";
      who.style.color = speaker?.color ?? "";
      who.textContent = speaker?.displayName ?? t("Desconocido ({id})", { id: turn.speakerId });

      const said = document.createElement("p");
      said.className = "said";
      if (turn.provisional) {
        row.classList.add("provisional");
        said.title = t("Borrador en vivo; puede corregirse al terminar el turno");
      }
      // Low-confidence Whisper output remains visible but subdued so readers
      // can inspect it with appropriate caution.
      if (turn.confidence !== null && turn.confidence < 0.5) {
        said.classList.add("low-confidence");
        said.title = t("Whisper no estaba muy seguro de este fragmento");
      }
      said.textContent = turn.text;

      body.append(who, said);
      row.append(time, body);
      return row;
    });

  if (live && talkingNames.length > 0) {
    const listening = document.createElement("div");
    listening.className = "live-listening";
    const dot = document.createElement("span");
    dot.setAttribute("aria-hidden", "true");
    const text = document.createElement("span");
    text.textContent = t("Escuchando a {people}…", { people: talkingNames.join(", ") });
    listening.append(dot, text);
    nodes.push(listening);
  }

  container.replaceChildren(...nodes);
}

function scrollTranscriptToEnd() {
  const el = $("transcript");
  el.scrollTop = el.scrollHeight;
}

function renderSummary() {
  const container = $("summary");
  const meeting = state.viewing;
  const summary = meeting.summary;

  if (!summary) {
    const pending = document.createElement("p");
    pending.className = "summary-pending";
    pending.textContent = !summariesEnabled()
      ? t("Los resúmenes y tareas están desactivados.")
      : state.status === "summarizing"
        ? t("Pidiéndole el resumen al modelo…")
        : isLiveMeeting(meeting.meta.id)
          ? t("El resumen sale al terminar la llamada.")
          : t("Esta reunión no tiene resumen. Puedes generarlo con «Rehacer resumen».");
    container.replaceChildren(pending);
    return;
  }

  const nodes = [];

  if (summary.overview) {
    nodes.push(heading(t("Resumen general")));
    const p = document.createElement("p");
    p.className = "overview";
    p.textContent = summary.overview;
    nodes.push(p);
  }

  for (const [title, items] of [
    [t("Decisiones"), summary.decisions],
    [t("Puntos clave"), summary.keyPoints],
    [t("Preguntas abiertas"), summary.openQuestions],
  ]) {
    if (!items || items.length === 0) continue;
    nodes.push(heading(title));
    const ul = document.createElement("ul");
    ul.append(
      ...items.map((item) => {
        const li = document.createElement("li");
        li.textContent = item;
        return li;
      }),
    );
    nodes.push(ul);
  }

  if (summary.generatedBy) {
    const by = document.createElement("p");
    by.className = "generated-by";
    by.textContent = t("Resumen generado por {provider}", { provider: summary.generatedBy });
    nodes.push(by);
  }

  container.replaceChildren(...nodes);
}

function selectMeetingInsightTab(name, focus = false) {
  state.meetingInsightTab = name === "tasks" ? "tasks" : "summary";
  for (const tabName of ["summary", "tasks"]) {
    const tab = $(`meeting-tab-${tabName}`);
    const panel = $(`meeting-panel-${tabName}`);
    const active = tabName === state.meetingInsightTab;
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
    panel.hidden = !active;
  }
  if (focus) $(`meeting-tab-${state.meetingInsightTab}`).focus();
}

function renderMeetingTasks() {
  const meeting = state.viewing;
  const summary = meeting?.summary;
  const tasks = summary?.actionItems ?? [];
  const hasTasks = tasks.length > 0;
  $("task-assignee-filter-section").hidden = !hasTasks;
  if (hasTasks) renderAssigneeFilter(meeting, summary);
  else state.taskAssigneeFilter = "all";
  $("meeting-task-count").textContent = tasks.length ? String(tasks.length) : "";
  const container = $("meeting-tasks-list");
  if (!summary) {
    container.replaceChildren(emptySummaryMessage(
      !summariesEnabled()
        ? t("Los resúmenes y tareas están desactivados.")
        : isLiveMeeting(meeting.meta.id)
        ? t("Las tareas aparecerán al terminar la llamada.")
        : t("Genera el resumen para extraer las tareas de esta reunión."),
    ));
    return;
  }
  container.replaceChildren(...renderTaskGroups(meeting, tasks));
}

function personKey(name) {
  return (name ?? "")
    .normalize("NFD")
    .replace(/\p{Diacritic}/gu, "")
    .trim()
    .toLocaleLowerCase();
}

function taskParticipants(meeting, summary) {
  const participants = new Map();
  for (const task of summary?.actionItems ?? []) {
    if (!task.assignee) continue;
    const owner = canonicalAssignee(meeting, task.assignee);
    const key = personKey(owner);
    if (!key || participants.has(key)) continue;
    const speaker = meeting.speakers.find((candidate) =>
      !candidate.isBot && [candidate.displayName, candidate.username]
        .some((name) => personKey(name) === key));
    participants.set(key, speaker ?? { displayName: owner, avatarUrl: null, color: "" });
  }
  return participants;
}

function renderAssigneeFilter(meeting, summary) {
  const filter = $("task-assignee-filter");
  const participants = taskParticipants(meeting, summary);
  const choices = [
    { value: "all", speaker: { displayName: t("Todos"), avatarUrl: null, color: "var(--accent)" } },
    ...[...participants].map(([key, speaker]) => ({ value: `person:${key}`, speaker })),
  ];
  if (summary.actionItems.some((task) => !task.assignee)) {
    choices.push({
      value: "unassigned",
      speaker: { displayName: t("Sin asignar"), avatarUrl: null, color: "var(--muted)" },
    });
  }
  if (!choices.some((choice) => choice.value === state.taskAssigneeFilter)) {
    state.taskAssigneeFilter = "all";
  }
  filter.replaceChildren(...choices.map(({ value, speaker }) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "assignee-choice";
    button.classList.toggle("active", value === state.taskAssigneeFilter);
    button.setAttribute("aria-pressed", String(value === state.taskAssigneeFilter));
    button.append(participantAvatar(speaker, 26), document.createTextNode(speaker.displayName));
    button.addEventListener("click", () => {
      state.taskAssigneeFilter = value;
      renderMeetingTasks();
    });
    return button;
  }));
}

function participantAvatar(speaker, size = 24) {
  if (speaker?.avatarUrl) {
    const image = document.createElement("img");
    image.className = "avatar";
    image.src = speaker.avatarUrl;
    image.alt = "";
    image.width = size;
    image.height = size;
    image.loading = "lazy";
    return image;
  }
  const initial = document.createElement("span");
  initial.className = "initial avatar-initial";
  initial.style.width = `${size}px`;
  initial.style.height = `${size}px`;
  initial.style.background = speaker?.color || "var(--surface-3)";
  initial.textContent = (speaker?.displayName?.[0] ?? "?").toUpperCase();
  initial.setAttribute("aria-hidden", "true");
  return initial;
}

function canonicalAssignee(meeting, assignee) {
  if (!assignee) return t("Sin asignar");
  const key = personKey(assignee);
  return (
    meeting.speakers.find(
      (speaker) =>
        !speaker.isBot &&
        [speaker.displayName, speaker.username].some((name) => personKey(name) === key),
    )?.displayName ?? assignee
  );
}

function renderTaskGroups(meeting, tasks) {
  if (tasks.length === 0) {
    return [emptySummaryMessage(t("No salió ninguna tarea de esta reunión."))];
  }

  const selected = state.taskAssigneeFilter;
  if (selected !== "all") {
    const filtered = tasks.filter((task) => {
      if (selected === "unassigned") return !task.assignee;
      return selected === `person:${personKey(canonicalAssignee(meeting, task.assignee))}`;
    });
    const owner = selected === "unassigned"
      ? t("Sin asignar")
      : [...taskParticipants(meeting, { actionItems: tasks }).entries()]
        .find(([key]) => selected === `person:${key}`)?.[1]?.displayName ?? t("Este participante");
    if (filtered.length === 0) {
      return [emptySummaryMessage(t("{owner} no tiene tareas asignadas en esta reunión.", { owner }))];
    }
    return [taskGroupNode(owner, filtered, meeting.meta.id, meeting)];
  }

  const groups = new Map();
  for (const task of tasks) {
    const owner = canonicalAssignee(meeting, task.assignee);
    const key = personKey(owner) || "unassigned";
    if (!groups.has(key)) groups.set(key, { owner, tasks: [] });
    groups.get(key).tasks.push(task);
  }
  return [...groups.values()].map((group) =>
    taskGroupNode(group.owner, group.tasks, meeting.meta.id, meeting),
  );
}

function taskGroupNode(owner, tasks, meetingId, meeting) {
  const group = document.createElement("section");
  group.className = "task-group";

  const head = document.createElement("div");
  head.className = "task-group-head";
  const title = document.createElement("h5");
  const speaker = meeting?.speakers.find((candidate) =>
    !candidate.isBot && personKey(candidate.displayName) === personKey(owner));
  title.append(participantAvatar(speaker ?? { displayName: owner }, 24), document.createTextNode(owner));
  const count = document.createElement("span");
  count.className = "task-count";
  const pending = tasks.filter((task) => !task.done).length;
  count.textContent = t("{pending}/{total} pendientes", { pending, total: tasks.length });
  head.append(title, count);

  group.append(head, ...tasks.map((task) => taskNode(task, meetingId)));
  return group;
}

function emptySummaryMessage(text) {
  const none = document.createElement("p");
  none.className = "summary-pending";
  none.textContent = text;
  return none;
}

function heading(text) {
  const h = document.createElement("h4");
  h.textContent = text;
  return h;
}

function taskNode(task, meetingId) {
  const row = document.createElement("label");
  row.className = `task${task.done ? " done" : ""}`;

  const box = document.createElement("input");
  box.type = "checkbox";
  box.checked = task.done;
  box.addEventListener("change", async () => {
    try {
      await invoke("set_task_done", {
        meetingId,
        taskId: task.id,
        done: box.checked,
      });
      task.done = box.checked;
      renderMeetingTasks();
      state.tasksLoaded = false;
    } catch (e) {
      box.checked = !box.checked;
      toast(String(e), "tareas", true);
    }
  });

  const text = document.createElement("span");
  text.className = "task-text";
  text.appendChild(document.createTextNode(task.text));

  if (task.due || task.sourceMs != null) {
    const meta = document.createElement("small");
    meta.className = "task-meta";
    const rest = [task.due, task.sourceMs != null ? timestamp(task.sourceMs) : null].filter(Boolean);
    if (rest.length > 0) {
      meta.appendChild(document.createTextNode(rest.join(" · ")));
    }
    text.appendChild(meta);
  }

  row.append(box, text);
  return row;
}

// --- global tasks ---------------------------------------------------------

async function refreshTasks(force = false) {
  if (!state.tasksLoaded || force) {
    $("global-task-list").setAttribute("aria-busy", "true");
    try {
      state.tasks = await invoke("list_tasks");
      indexTaskPeople();
      state.tasksLoaded = true;
    } catch (error) {
      toast(String(error), "tareas", true);
    } finally {
      $("global-task-list").removeAttribute("aria-busy");
    }
  }
  renderGlobalTaskFilters();
  renderGlobalTasks();
}

function taskOwnerKey(item) {
  if (!item.task.assignee) return "__unassigned__";
  return item.assigneeId ? `id:${item.assigneeId}` : `name:${personKey(item.task.assignee)}`;
}

function indexTaskPeople() {
  const people = new Map();
  for (const item of state.tasks) {
    const key = taskOwnerKey(item);
    if (people.has(key)) continue;
    people.set(key, {
      key,
      displayName: item.task.assignee || "Sin asignar",
      searchKey: personKey(item.task.assignee || "Sin asignar"),
      avatarUrl: item.assigneeAvatarUrl,
      color: item.assigneeColor || (key === "__unassigned__" ? "var(--muted)" : ""),
    });
  }
  state.taskPeople = [...people.values()].sort((a, b) => {
    if (a.key === "__unassigned__") return 1;
    if (b.key === "__unassigned__") return -1;
    return a.displayName.localeCompare(b.displayName, undefined, { sensitivity: "base" });
  });
  const available = new Set(state.taskPeople.map((person) => person.key));
  for (const selected of state.taskFilters.people) {
    if (!available.has(selected)) state.taskFilters.people.delete(selected);
  }
}

function renderGlobalTaskFilters() {
  const meetings = new Map(state.tasks.map((item) => [item.meetingId, item.meetingTitle]));
  const meeting = $("tasks-meeting-filter");
  meeting.replaceChildren(
    Object.assign(document.createElement("option"), { value: "all", textContent: t("Todas") }),
    ...[...meetings].sort((a, b) => a[1].localeCompare(b[1], undefined, { sensitivity: "base" })).map(([id, title]) =>
      Object.assign(document.createElement("option"), { value: id, textContent: title })),
  );
  if (![...meeting.options].some((option) => option.value === state.taskFilters.meeting)) {
    state.taskFilters.meeting = "all";
  }
  meeting.value = state.taskFilters.meeting;
  $("tasks-date-from").value = state.taskFilters.dateFrom;
  $("tasks-date-to").value = state.taskFilters.dateTo;
  $("tasks-status-filter").value = state.taskFilters.status;
  renderTaskPersonOptions();
  renderTaskDateLabel();
}

function renderTaskPersonOptions() {
  const selected = state.taskFilters.people;
  const selectedPeople = state.taskPeople.filter((person) => selected.has(person.key));
  $("tasks-person-label").textContent = selected.size === 0
    ? t("Todas las personas")
    : selected.size === 1
      ? selectedPeople[0]?.displayName ?? t("1 persona")
      : t("{count} personas", { count: selected.size });

  const query = personKey($("tasks-person-search").value);
  const matching = state.taskPeople.filter((person) => !query || person.searchKey.includes(query));
  const visible = matching.slice(0, 75);
  const options = visible.map((person) => {
    const label = document.createElement("label");
    label.className = "people-option";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = selected.has(person.key);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) selected.add(person.key);
      else selected.delete(person.key);
      state.taskRenderLimit = 250;
      renderTaskPersonOptions();
      renderGlobalTasks();
    });
    const copy = document.createElement("span");
    copy.textContent = person.displayName;
    label.append(checkbox, participantAvatar(person, 28), copy);
    return label;
  });
  if (options.length === 0) {
    const empty = document.createElement("p");
    empty.className = "filter-empty";
    empty.textContent = t("No hay personas que coincidan.");
    options.push(empty);
  }
  $("tasks-person-options").replaceChildren(...options);
  $("tasks-person-result-note").textContent = matching.length > visible.length
    ? t("{count} coincidencias · escribe más para acotar; se muestran 75", {
        count: matching.length.toLocaleString(currentLocale()),
      })
    : t(matching.length === 1 ? "{count} persona" : "{count} personas", {
        count: matching.length.toLocaleString(currentLocale()),
      });
}

function formatFilterDate(value) {
  if (!value) return "";
  return new Intl.DateTimeFormat(currentLocale(), { day: "numeric", month: "short", year: "numeric" })
    .format(new Date(`${value}T12:00:00`));
}

function renderTaskDateLabel() {
  const { dateFrom, dateTo } = state.taskFilters;
  $("tasks-date-label").textContent = dateFrom && dateTo
    ? `${formatFilterDate(dateFrom)} – ${formatFilterDate(dateTo)}`
    : dateFrom
      ? t("Desde {date}", { date: formatFilterDate(dateFrom) })
      : dateTo
        ? t("Hasta {date}", { date: formatFilterDate(dateTo) })
        : t("Cualquier fecha");
}

function setTaskFilterPopover(name, open) {
  for (const candidate of ["person", "date"]) {
    const expanded = candidate === name && open;
    $(`tasks-${candidate}-popover`).hidden = !expanded;
    $(`tasks-${candidate}-trigger`).setAttribute("aria-expanded", String(expanded));
  }
  if (open && name === "person") {
    $("tasks-person-search").focus();
    renderTaskPersonOptions();
  }
}

function closeTaskFilterPopovers() {
  setTaskFilterPopover("", false);
}

function applyTaskDateRange() {
  const from = $("tasks-date-from").value;
  const to = $("tasks-date-to").value;
  if (from && to && from > to) {
    $("tasks-date-error").textContent = t("La fecha inicial debe ser anterior a la fecha final.");
    $("tasks-date-from").focus();
    return false;
  }
  $("tasks-date-error").textContent = "";
  state.taskFilters.dateFrom = from;
  state.taskFilters.dateTo = to;
  state.taskRenderLimit = 250;
  renderTaskDateLabel();
  renderGlobalTasks();
  closeTaskFilterPopovers();
  return true;
}

function filteredGlobalTasks() {
  const filters = state.taskFilters;
  const query = personKey(filters.query);
  const from = filters.dateFrom ? new Date(`${filters.dateFrom}T00:00:00`).getTime() : null;
  const to = filters.dateTo ? new Date(`${filters.dateTo}T23:59:59.999`).getTime() : null;
  return state.tasks.filter((item) => {
    if (filters.status === "pending" && item.task.done) return false;
    if (filters.status === "done" && !item.task.done) return false;
    if (filters.people.size > 0 && !filters.people.has(taskOwnerKey(item))) return false;
    if (filters.meeting !== "all" && item.meetingId !== filters.meeting) return false;
    const startedAt = new Date(item.startedAt).getTime();
    if (from && startedAt < from) return false;
    if (to && startedAt > to) return false;
    if (query) {
      const haystack = personKey([
        item.task.text,
        item.task.assignee,
        item.task.due,
      ].filter(Boolean).join(" "));
      if (!haystack.includes(query)) return false;
    }
    return true;
  }).sort((a, b) => Number(a.task.done) - Number(b.task.done)
    || new Date(b.startedAt) - new Date(a.startedAt));
}

function renderGlobalTasks() {
  const visible = filteredGlobalTasks();
  const pendingTotal = state.tasks.filter((item) => !item.task.done).length;
  $("pending-task-count").hidden = pendingTotal === 0;
  $("pending-task-count").textContent = pendingTotal;
  $("tasks-page-count").textContent = t(
    pendingTotal === 1 ? "{count} pendiente" : "{count} pendientes",
    { count: pendingTotal },
  );
  if (visible.length === 0) {
    const empty = document.createElement("div");
    empty.className = "tasks-empty";
    const title = document.createElement("strong");
    title.textContent = state.tasks.length === 0
      ? t("Todavía no hay tareas")
      : t("No hay tareas con estos filtros");
    const copy = document.createElement("p");
    copy.textContent = state.tasks.length === 0
      ? t("Cuando Kuali resuma una reunión, los compromisos aparecerán aquí.")
      : t("Prueba otra persona, fecha, reunión o estado.");
    empty.append(title, copy);
    $("global-task-list").replaceChildren(empty);
    return;
  }
  const rendered = visible.slice(0, state.taskRenderLimit).map(globalTaskCard);
  if (visible.length > state.taskRenderLimit) {
    const more = document.createElement("button");
    more.type = "button";
    more.className = "ghost tasks-load-more";
    more.textContent = t("Mostrar {count} más de {total}", {
      count: Math.min(250, visible.length - state.taskRenderLimit),
      total: visible.length.toLocaleString(currentLocale()),
    });
    more.addEventListener("click", () => {
      state.taskRenderLimit += 250;
      renderGlobalTasks();
    });
    rendered.push(more);
  }
  $("global-task-list").replaceChildren(...rendered);
}

function globalTaskCard(item) {
  const card = document.createElement("article");
  card.className = `global-task-card${item.task.done ? " done" : ""}`;
  const checkbox = document.createElement("input");
  checkbox.type = "checkbox";
  checkbox.checked = item.task.done;
  checkbox.setAttribute(
    "aria-label",
    t("Marcar «{task}» como {status}", {
      task: item.task.text,
      status: t(item.task.done ? "pendiente" : "completada"),
    }),
  );
  checkbox.addEventListener("change", async () => {
    try {
      await invoke("set_task_done", {
        meetingId: item.meetingId,
        taskId: item.task.id,
        done: checkbox.checked,
      });
      item.task.done = checkbox.checked;
      const openTask = state.viewing?.summary?.actionItems?.find((task) => task.id === item.task.id);
      if (openTask) openTask.done = checkbox.checked;
      renderGlobalTasks();
      if (state.viewing?.meta.id === item.meetingId) renderMeetingTasks();
    } catch (error) {
      checkbox.checked = !checkbox.checked;
      toast(String(error), "tareas", true);
    }
  });

  const avatar = participantAvatar({
    displayName: item.task.assignee || "Sin asignar",
    avatarUrl: item.assigneeAvatarUrl,
    color: item.assigneeColor,
  }, 32);
  const content = document.createElement("div");
  content.className = "global-task-content";
  const taskText = document.createElement("strong");
  taskText.textContent = item.task.text;
  const details = document.createElement("div");
  details.className = "global-task-details";
  const owner = document.createElement("span");
  owner.textContent = item.task.assignee || t("Sin asignar");
  details.append(owner);
  if (item.task.due) details.append(document.createTextNode(` · ${item.task.due}`));
  content.append(taskText, details);

  // Origin metadata supports filtering and navigation but is not part of the
  // task itself, so it lives in a separate block.
  const origin = document.createElement("aside");
  origin.className = "task-origin";
  origin.append(platformMark(meetingPlatform({ guildName: item.guildName })));
  const originCopy = document.createElement("span");
  originCopy.className = "task-origin-copy";
  const originLabel = document.createElement("small");
  originLabel.textContent = t("Reunión");
  const meeting = document.createElement("button");
  meeting.type = "button";
  meeting.className = "task-meeting-link";
  meeting.textContent = item.meetingTitle;
  meeting.addEventListener("click", () => openMeeting(item.meetingId));
  const originMeta = document.createElement("span");
  const platform = meetingPlatform({ guildName: item.guildName });
  const sourceName = platform === "discord" ? `# ${item.channelName}` : item.guildName;
  originMeta.textContent = `${sourceName} · ${shortDate(item.startedAt)}`;
  originCopy.append(originLabel, meeting, originMeta);
  origin.append(originCopy);
  card.append(checkbox, avatar, content, origin);
  return card;
}

// --- setup guide ----------------------------------------------------------

const DISCORD_GUIDE_TITLES = [
  "Crea la aplicación",
  "Pega el token",
  "Autoriza el servidor",
];

const KUALI_EXTENSION_STORE_URL =
  "https://chromewebstore.google.com/detail/kuali/cgojkmdggflcggedmapamcmkelgaahhp";

const MEET_GUIDE_TITLES = [
  "Instala la extensión",
  "Fija Kuali",
  "Haz una prueba",
];

function initialSetupCompleted() {
  return localStorage.getItem("kuali.onboarding.completed") === "true";
}

function selectedRequiredModel() {
  return state.models.find((model) => model.id === $("required-model-select").value);
}

function hasDownloadedWhisperWeight() {
  return state.models.some((model) => model.downloaded);
}

function audioIsWaitingForWhisper() {
  return state.liveMeetings.size > 0
    || ["joining", "recording", "finalizing"].includes(state.status);
}

function renderRequiredModelProgress() {
  const downloading = state.modelState.state === "downloading";
  const row = $("required-model-download-row");
  row.hidden = !downloading;
  if (!downloading) return;

  const total = state.modelState.totalBytes;
  const downloaded = state.modelState.downloadedBytes;
  const percentage = total ? Math.round((downloaded / total) * 100) : 0;
  $("required-model-progress-bar").style.width = `${percentage}%`;
  $("required-model-progress-text").textContent = `${percentage}% · ${humanBytes(downloaded)}`;
}

function renderRequiredModelActivity() {
  const selected = selectedRequiredModel();
  const configured = state.config?.whisper?.model;
  const downloading = state.modelState.state === "downloading";
  const missingWeights = !hasDownloadedWhisperWeight();
  $("model-required").hidden = !missingWeights && !downloading;
  $("required-model-select").disabled = state.models.length === 0 || downloading;

  const stateBadge = $("model-required-state");
  stateBadge.className = "guide-state";
  if (downloading) {
    stateBadge.textContent = t("Descargando…");
    stateBadge.classList.add("downloading");
  } else if (state.modelState.state === "failed") {
    stateBadge.textContent = t("Descarga fallida");
    stateBadge.classList.add("failed");
  } else if (selected?.downloaded) {
    stateBadge.textContent = t("Modelo listo");
    stateBadge.classList.add("ready");
  } else {
    stateBadge.textContent = t("Obligatorio");
  }

  const button = $("btn-required-model");
  const selectedIsConfigured = selected?.id === configured;
  button.disabled = !selected || downloading || (selected.downloaded && selectedIsConfigured);
  button.textContent = downloading
    ? t("Descargando…")
    : selected?.downloaded
      ? selectedIsConfigured
        ? t("Modelo listo")
        : t("Usar este modelo")
      : t("Descargar · {size}", { size: humanBytes(selected?.approxBytes ?? 0) });
  renderRequiredModelProgress();
}

function renderRequiredModelNotice(requestedModel = "") {
  const select = $("required-model-select");
  const configured = state.config?.whisper?.model;
  const requested = state.models.find((model) => model.id === requestedModel);
  const configuredModel = state.models.find((model) => model.id === configured);
  const installedFallback = state.models.find((model) => model.downloaded);
  const recommended = state.models.find((model) => model.id === "large-v3-turbo-q5");
  const chosen = requested
    ?? (configuredModel?.downloaded ? configuredModel : null)
    ?? installedFallback
    ?? configuredModel
    ?? recommended
    ?? state.models[0];

  select.replaceChildren(
    ...state.models.map((model) => {
      const option = document.createElement("option");
      option.value = model.id;
      const recommendation = model.id === "large-v3-turbo-q5"
        ? ` · ${t("Recomendado")}`
        : "";
      option.textContent = `${t(model.label)} — ${humanBytes(model.approxBytes)}${recommendation}${model.downloaded ? " ✓" : ""}`;
      return option;
    }),
  );
  if (chosen) select.value = chosen.id;

  const selected = selectedRequiredModel();
  $("model-required-message").textContent = audioIsWaitingForWhisper()
    ? t("Kuali sigue capturando el audio de la llamada. Cuando termine la descarga, transcribirá todo lo pendiente; no se perderá nada.")
    : t("No hay pesos de Whisper descargados. Elige uno para habilitar la transcripción local.");

  $("required-model-hint").textContent = selected?.downloaded
    ? t("Ya está descargado en {directory}.", { directory: state.modelsDirectory })
    : t("Se descargarán {size} en {directory}.", {
        size: humanBytes(selected?.approxBytes ?? 0),
        directory: state.modelsDirectory,
      });
  $("required-model-note").textContent = selected?.downloaded
    ? t("Este modelo está listo. Ya puedes completar la configuración inicial.")
    : state.modelState.state === "downloading"
      ? t("La descarga continúa aunque cambies de sección dentro de Kuali.")
      : state.modelState.state === "failed"
        ? t("No se pudo completar la descarga. Puedes intentarlo nuevamente.")
      : t("Necesitas al menos un modelo descargado para transcribir.");

  const finish = $("btn-finish-guide");
  finish.textContent = initialSetupCompleted()
    ? t("Volver al inicio")
    : t("Completar configuración");
  finish.title = !initialSetupCompleted() && !hasDownloadedWhisperWeight()
    ? t("Descarga un modelo para poder terminar.")
    : "";
  renderRequiredModelActivity();
}

async function refreshRequiredModelNotice(requestedModel = $("required-model-select").value) {
  [state.models, state.modelsDirectory] = await Promise.all([
    invoke("whisper_models"),
    invoke("resolved_models_directory"),
  ]);
  renderRequiredModelNotice(requestedModel);
}

async function downloadRequiredModel() {
  const model = selectedRequiredModel();
  if (!model || state.modelState.state === "downloading") return;

  const button = $("btn-required-model");
  button.disabled = true;
  try {
    const config = structuredClone(state.config);
    config.whisper.model = model.id;
    await invoke("set_config", { config });
    state.config = config;
    // Calling this even for an existing weight also ensures that the small
    // Silero VAD model is present before setup is considered complete.
    await invoke("download_model", { model: model.id });
    await refreshRequiredModelNotice(model.id);
    const snapshot = await invoke("get_snapshot");
    state.modelState = snapshot.modelState;
    renderRequiredModelNotice(model.id);
    renderStatus();
    toast(t("Modelo descargado"), "Whisper");
  } catch (error) {
    toast(String(error), "Whisper", true);
    renderRequiredModelNotice(model.id);
  }
}

async function finishInitialSetup() {
  if (initialSetupCompleted()) {
    await goHome();
    return true;
  }

  await refreshRequiredModelNotice();
  const model = selectedRequiredModel();
  if (!model?.downloaded) {
    toast(
      t("Descarga el modelo elegido antes de completar la configuración."),
      t("Configuración inicial"),
      true,
    );
    $("model-required").scrollIntoView({ behavior: "smooth", block: "start" });
    $("required-model-select").focus();
    return false;
  }

  if (state.config.whisper.model !== model.id) {
    const config = structuredClone(state.config);
    config.whisper.model = model.id;
    await invoke("set_config", { config });
    state.config = config;
  }
  localStorage.setItem("kuali.onboarding.completed", "true");
  await goHome();
  return true;
}

function renderDiscordGuideStep({ focus = false } = {}) {
  const lastStep = DISCORD_GUIDE_TITLES.length - 1;
  state.discordGuideStep = Math.max(0, Math.min(lastStep, state.discordGuideStep));
  const step = state.discordGuideStep;
  const pages = [...document.querySelectorAll("[data-discord-guide-step]")];
  for (const page of pages) page.hidden = Number(page.dataset.discordGuideStep) !== step;

  $("discord-guide-progress-label").textContent = t("Paso {step} de {total}", {
    step: step + 1,
    total: DISCORD_GUIDE_TITLES.length,
  });
  $("discord-guide-progress-title").textContent = t(DISCORD_GUIDE_TITLES[step]);
  $("discord-guide-progress").setAttribute("aria-valuenow", String(step + 1));
  $("discord-guide-progress-bar").style.width = `${((step + 1) / DISCORD_GUIDE_TITLES.length) * 100}%`;

  const back = $("btn-discord-guide-back");
  const next = $("btn-discord-guide-next");
  back.disabled = step === 0;
  const discordReady = Boolean(state.config?.discord?.["bot-token"]);
  if (step === lastStep && !discordReady) {
    next.disabled = true;
    next.textContent = t("Conecta el bot primero");
  } else {
    next.disabled = false;
    next.textContent = step === lastStep ? t("Terminar guía") : t("Siguiente →");
  }

  if (focus) {
    const heading = pages[step]?.querySelector("h4");
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    }
  }
}

function setDiscordGuideStep(step, { focus = true } = {}) {
  state.discordGuideStep = step;
  renderDiscordGuideStep({ focus });
}

function renderMeetGuideStep({ focus = false } = {}) {
  const lastStep = MEET_GUIDE_TITLES.length - 1;
  state.meetGuideStep = Math.max(0, Math.min(lastStep, state.meetGuideStep));
  const step = state.meetGuideStep;
  const pages = [...document.querySelectorAll("[data-meet-guide-step]")];
  for (const page of pages) page.hidden = Number(page.dataset.meetGuideStep) !== step;
  $("meet-guide-progress-label").textContent = t("Paso {step} de {total}", {
    step: step + 1,
    total: MEET_GUIDE_TITLES.length,
  });
  $("meet-guide-progress-title").textContent = t(MEET_GUIDE_TITLES[step]);
  $("meet-guide-progress").setAttribute("aria-valuenow", String(step + 1));
  $("meet-guide-progress-bar").style.width = `${((step + 1) / MEET_GUIDE_TITLES.length) * 100}%`;
  $("btn-meet-guide-back").disabled = step === 0;
  $("btn-meet-guide-next").textContent = step === lastStep ? t("Terminar guía") : t("Siguiente →");
  if (focus) {
    const heading = pages[step]?.querySelector("h4");
    if (heading) {
      heading.tabIndex = -1;
      heading.focus();
    }
  }
}

function setMeetGuideStep(step, { focus = true } = {}) {
  state.meetGuideStep = step;
  renderMeetGuideStep({ focus });
}

async function advanceMeetGuide() {
  if (state.meetGuideStep === MEET_GUIDE_TITLES.length - 1) {
    await finishInitialSetup();
    return;
  }
  setMeetGuideStep(state.meetGuideStep + 1);
}

async function advanceDiscordGuide() {
  if (state.discordGuideStep === 1) {
    const token = $("guide-token").value.trim();
    if (!token) {
      $("guide-token-error").textContent = t("Pega el token que copiaste en Discord para continuar.");
      $("guide-token").focus();
      return;
    }
    const username = normalizedDiscordUsername($("guide-discord-username").value);
    if (!username) {
      $("guide-username-error").textContent = t(
        "Escribe tu @usuario de Discord para activar el seguimiento automático.",
      );
      $("guide-discord-username").focus();
      return;
    }
    const next = $("btn-discord-guide-next");
    next.disabled = true;
    next.textContent = t("Comprobando token…");
    if (!await openDiscordInstallFromGuide()) {
      renderDiscordGuideStep();
      return;
    }
  }
  if (state.discordGuideStep === DISCORD_GUIDE_TITLES.length - 1) {
    await finishInitialSetup();
    return;
  }
  setDiscordGuideStep(state.discordGuideStep + 1);
}

async function openDiscordInstallFromGuide() {
  const token = $("guide-token").value.trim();
  if (!token) {
    $("guide-token-error").textContent = t("Pega el token que copiaste en Discord para continuar.");
    $("guide-token").focus();
    return false;
  }
  try {
    await invoke("open_discord_install", { botToken: token });
    $("guide-token-error").textContent = "";
    return true;
  } catch (error) {
    $("guide-token-error").textContent = String(error);
    $("guide-token").focus();
    return false;
  }
}

async function renderGuide() {
  [state.models, state.modelsDirectory] = await Promise.all([
    invoke("whisper_models"),
    invoke("resolved_models_directory"),
  ]);
  renderRequiredModelNotice($("required-model-select").value);
  const discordReady = Boolean(state.config?.discord?.["bot-token"]);
  const following = isAutomaticFollowEnabled();
  $("guide-discord-state").textContent = following
    ? t("Seguimiento configurado")
    : discordReady
      ? t("Bot configurado")
      : t("Sin configurar");
  $("guide-discord-state").classList.toggle("ready", discordReady);
  $("guide-token").value = state.config?.discord?.["bot-token"] ?? "";
  $("guide-discord-username").value = state.config?.discord?.["follow-username"] ?? "";
  renderDiscordGuideStep();
  renderMeetGuideStep();
  $("guide-meet-state").textContent = state.webMeetings.listening
    ? t("Kuali disponible")
    : t("Chrome Web Store");
  $("guide-meet-state").classList.toggle("ready", state.webMeetings.listening);
  if (!state.extensionPath) {
    try {
      state.extensionPath = await invoke("browser_extension_path");
    } catch (error) {
      state.extensionPath = "";
      toast(String(error), "extensión", true);
    }
  }
  $("extension-path").textContent = state.extensionPath || t("No se encontró la carpeta");
  $("btn-copy-extension-path").disabled = !state.extensionPath;
  $("btn-reveal-extension").disabled = !state.extensionPath;
}

async function copyText(value, label) {
  try {
    await navigator.clipboard.writeText(value);
    toast(t("{label} copiada", { label }), t("guía"));
  } catch {
    toast(
      t("No pude copiarla automáticamente. Selecciona el texto y usa ⌘ C."),
      t("guía"),
      true,
    );
  }
}

async function saveDiscordFromGuide() {
  const token = $("guide-token").value.trim();
  const username = normalizedDiscordUsername($("guide-discord-username").value);
  if (!token) {
    $("guide-discord-note").textContent = t("Pega el token del bot para continuar.");
    $("guide-token").focus();
    return false;
  }
  if (!username) {
    $("guide-username-error").textContent = t(
      "Escribe tu @usuario de Discord para activar el seguimiento automático.",
    );
    $("guide-discord-username").focus();
    return false;
  }
  const button = $("btn-save-discord-guide");
  const previousConfig = structuredClone(state.config);
  button.disabled = true;
  button.textContent = t("Guardando y conectando…");
  try {
    const config = structuredClone(state.config);
    config.discord["bot-token"] = token;
    const previousUsername = normalizedDiscordUsername(config.discord["follow-username"] ?? "");
    config.discord["follow-username"] = username;
    config.discord["follow-automatically"] = true;
    if (previousUsername.toLowerCase() !== username.toLowerCase()) {
      config.discord["follow-user-id"] = null;
    }
    await invoke("set_config", { config });
    state.config = config;
    await invoke("connect");
    $("guide-discord-note").textContent = t(
      "Bot conectado. Kuali buscará a @{username} en los servidores que comparten.",
      { username },
    );
    await renderGuide();
    renderStatus();
    return true;
  } catch (error) {
    state.config = previousConfig;
    try {
      await invoke("set_config", { config: previousConfig });
    } catch {
      // Preserve the original connection error because it explains what the
      // user must correct at this step.
    }
    renderDiscordGuideStep();
    $("guide-discord-note").textContent = t("No se pudo conectar: {error}", {
      error: String(error),
    });
    return false;
  } finally {
    button.disabled = false;
    button.textContent = t("Ya autoricé · conectar");
  }
}

// --- engine events --------------------------------------------------------

function handleEvent(event) {
  switch (event.type) {
    case "statusChanged":
      state.status = event.status;
      renderRequiredModelNotice($("required-model-select").value);
      renderStatus();
      renderUpdateState();
      maybeInstallUpdateAutomatically();
      if (["idle", "setup"].includes(state.currentPane)) renderRoot();
      break;

    case "modelStateChanged":
      state.modelState = event.state;
      renderModelProgress();
      renderRequiredModelActivity();
      if (!["downloading", "verifying"].includes(event.state.state)) {
        refreshRequiredModelNotice().catch((error) =>
          toast(String(error), "Whisper", true));
      }
      renderStatus();
      if (["idle", "setup"].includes(state.currentPane)) renderRoot();
      break;

    case "webMeetingsStatusChanged":
      state.webMeetings = {
        enabled: event.enabled,
        port: event.port,
        listening: event.listening,
      };
      renderStatus();
      if (["idle", "setup"].includes(state.currentPane)) renderRoot();
      break;

    case "discordFollowChanged":
      if (state.config) {
        state.config.discord["follow-user-id"] = event.userId;
        state.config.discord["follow-automatically"] = event.enabled;
      }
      renderStatus();
      if (state.currentPane === "guide") renderGuide();
      else if (["idle", "setup"].includes(state.currentPane)) renderRoot();
      toast(t("Usuario de Discord reconocido; seguimiento automático activado"), "Discord");
      break;

    case "meetingStarted":
      state.liveId = event.meeting.id;
      state.liveMeta = event.meeting;
      // Build the live meeting immediately. Loading it through two disk-backed
      // promises used to drop roster events arriving in the meantime.
      const liveMeeting = {
        meta: event.meeting,
        speakers: [],
        utterances: [],
        summary: null,
      };
      state.liveMeetings.set(event.meeting.id, liveMeeting);
      renderRequiredModelNotice($("required-model-select").value);
      talkingFor(event.meeting.id).clear();
      state.taskAssigneeFilter = "all";
      if (["idle", "setup"].includes(state.currentPane) && !state.viewing) {
        state.viewing = liveMeeting;
        showPane("meeting");
        renderMeeting();
      }
      renderStatus();
      refreshMeetings();
      renderLiveMeetingList();
      renderUpdateState();
      break;

    case "meetingEnded": {
      const endedWasOpen = state.viewing?.meta.id === event.meetingId;
        state.liveMeetings.delete(event.meetingId);
        renderRequiredModelNotice($("required-model-select").value);
        state.talking.delete(event.meetingId);
        for (const [id, draft] of state.liveDrafts) {
          if (draft.meetingId === event.meetingId) state.liveDrafts.delete(id);
        }
        if (state.liveId === event.meetingId) {
          const next = [...state.liveMeetings.values()].at(-1) ?? null;
          state.liveId = next?.meta.id ?? null;
          state.liveMeta = next?.meta ?? null;
        }
        refreshMeetings();
        renderStatus();
        if (endedWasOpen) {
          // The live object lacks the provisional title and final timestamp the
          // engine just persisted. Replacing it prevents the header from
          // reverting to the technical channel name while summarizing.
          invoke("load_meeting", { id: event.meetingId })
            .then((saved) => {
              if (state.viewing?.meta.id !== event.meetingId) return;
              state.viewing = saved;
              renderMeeting();
            })
            .catch(() => renderMeeting());
        }
        state.tasksLoaded = false;
        renderUpdateState();
        break;
      }

    case "speakerJoined":
      {
        const meeting = meetingForEvent(event.meetingId);
        if (!meeting) break;
        upsert(meeting.speakers, event.speaker, (s) => s.userId);
        if (state.viewing?.meta.id === event.meetingId) renderMeeting();
        renderLiveMeetingList();
      }
      break;

    case "speakerLeft":
      talkingFor(event.meetingId).delete(event.userId);
      if (state.viewing?.meta.id === event.meetingId) {
        renderSpeakers();
        renderTranscript();
      }
      break;

    case "speakingChanged":
      if (event.speaking) talkingFor(event.meetingId).add(event.userId);
      else talkingFor(event.meetingId).delete(event.userId);
      if (state.viewing?.meta.id === event.meetingId) {
        renderSpeakers();
        renderTranscript();
      }
      break;

    case "utteranceAdded": {
      state.liveDrafts.delete(event.utterance.id);
      const meeting = meetingForEvent(event.meetingId);
      if (!meeting) break;
      upsertUtterance(meeting.utterances, event.utterance);
      if (state.viewing?.meta.id !== event.meetingId) break;

      const following = shouldFollow();
      renderMeeting();
      if (following) scrollTranscriptToEnd();
      break;
    }

    case "utterancePreview": {
      const draft = { ...event.utterance, meetingId: event.meetingId, provisional: true };
      state.liveDrafts.set(draft.id, draft);
      if (state.viewing?.meta.id !== event.meetingId) break;
      const following = shouldFollow();
      renderMeeting();
      if (following) scrollTranscriptToEnd();
      break;
    }

    case "utterancePreviewCleared":
      state.liveDrafts.delete(event.utteranceId);
      if (state.viewing?.meta.id === event.meetingId) renderMeeting();
      break;

    case "summaryStarted":
      if (state.viewing?.meta.id === event.meetingId) renderSummary();
      break;

    case "summaryReady":
      {
        const meeting = meetingForEvent(event.meetingId);
        if (meeting) {
          meeting.summary = event.summary;
          if (event.summary.title) meeting.meta.displayTitle = event.summary.title;
        }
        if (state.viewing?.meta.id === event.meetingId) renderMeeting();
      }
      state.tasksLoaded = false;
      refreshMeetings();
      toast(t("Resumen listo"), "Kuali");
      break;

    case "error":
      toast(event.message, event.source, true);
      break;
  }
}

function upsertUtterance(list, utterance) {
  const existing = list.findIndex((item) => item.id === utterance.id);
  if (existing >= 0) list.splice(existing, 1);
  const at = list.findLastIndex((item) => item.startMs <= utterance.startMs);
  list.splice(at + 1, 0, utterance);
}

function upsert(list, item, key) {
  const at = list.findIndex((existing) => key(existing) === key(item));
  if (at >= 0) list[at] = item;
  else list.push(item);
}

/** Auto-scrolls only while the user remains near the bottom. */
function shouldFollow() {
  if (!$("follow-live").checked) return false;
  const el = $("transcript");
  return el.scrollHeight - el.scrollTop - el.clientHeight < 120;
}

// --- settings -------------------------------------------------------------

function selectSettingsTab(name, focus = false) {
  const tabs = [...document.querySelectorAll("[data-settings-tab]")];
  const selected = tabs.find((tab) => tab.dataset.settingsTab === name) ?? tabs[0];
  if (!selected) return;
  state.settingsTab = selected.dataset.settingsTab;

  for (const tab of tabs) {
    const active = tab === selected;
    tab.classList.toggle("active", active);
    tab.setAttribute("aria-selected", String(active));
    tab.tabIndex = active ? 0 : -1;
  }
  for (const panel of document.querySelectorAll("[data-settings-panel]")) {
    panel.hidden = panel.dataset.settingsPanel !== state.settingsTab;
  }
  if (focus) selected.focus();
}

async function openSettings() {
  [state.config, state.models, state.modelsDirectory] = await Promise.all([
    invoke("get_config"),
    invoke("whisper_models"),
    invoke("resolved_models_directory"),
  ]);
  renderRequiredModelNotice(state.config.whisper.model);
  try {
    state.providers = await invoke("provider_catalog");
  } catch {
    state.providers = [];
  }
  try {
    state.webhookChannels = await invoke("webhook_channels");
  } catch {
    state.webhookChannels = [];
  }
  try {
    state.autostartEnabled = await invoke("autostart_enabled");
  } catch {
    state.autostartEnabled = false;
  }

  const c = state.config;
  $("cfg-token").value = c.discord["bot-token"] ?? "";
  $("cfg-follow").value = c.discord["follow-username"] ?? "";
  $("cfg-follow-automatically").checked = c.discord["follow-automatically"] !== false;
  $("cfg-leave-empty").checked = c.discord["leave-when-empty"];
  $("cfg-post-summary").checked = c.discord["post-summary-to-channel"];
  $("cfg-web-enabled").checked = c.meet?.enabled !== false;
  $("cfg-web-port").value = c.meet?.port ?? 9099;
  $("cfg-web-port").disabled = !$("cfg-web-enabled").checked;
  renderWebListenerStatus();

  renderWhisperModelOptions(c.whisper.model);
  $("cfg-models-directory").value = c.whisper["models-directory"] ?? "";
  $("cfg-language").value = c.whisper.language;
  state.customVocabulary = [...(c.whisper["custom-vocabulary"] ?? [])];
  $("cfg-vocabulary-input").value = "";
  renderCustomVocabulary();
  updateModelsDirectoryHint();
  updateModelHint();
  renderInstalledModels();

  // Provider settings remain in memory until Save so switching providers does
  // not discard a freshly entered key.
  state.providerSettings = structuredClone(c.llm.providers ?? {});
  state.selectedProvider = c.llm["preferred-provider"] ?? "";
  renderProviders();

  $("cfg-output-language").value = c.llm["output-language"] ?? "español";
  $("cfg-summarize").checked = c.llm["summarize-on-leave"];
  updateSummarySettingsVisibility();
  $("cfg-ui-language").value = c.application?.language ?? "auto";
  $("cfg-autostart").checked = state.autostartEnabled;
  $("cfg-automatic-updates").checked = c.application?.["automatic-updates"] !== false;
  renderUpdateState();
  state.webhooks = structuredClone(c.integrations?.webhooks ?? []);
  renderWebhooks();

  $("save-note").textContent = "";
  $("settings-modal").hidden = false;
  selectSettingsTab(state.settingsTab);
  renderModelProgress();
}

function renderWhisperModelOptions(selected = $("cfg-model").value) {
  $("cfg-model").replaceChildren(
    ...state.models.map((m) => {
      const opt = document.createElement("option");
      opt.value = m.id;
      opt.textContent = `${t(m.label)} — ${humanBytes(m.approxBytes)}${m.downloaded ? " ✓" : ""}`;
      return opt;
    }),
  );
  $("cfg-model").value = selected;
}

// --- webhooks -------------------------------------------------------------

function randomHex(byteLength) {
  const bytes = new Uint8Array(byteLength);
  crypto.getRandomValues(bytes);
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function randomWebhookSecret() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  const binary = String.fromCharCode(...bytes);
  return `whsec_${btoa(binary)}`;
}

function isStandardWebhookSecret(secret) {
  if (!secret?.startsWith("whsec_")) return false;
  try {
    const encoded = secret.slice("whsec_".length);
    const padded = encoded.padEnd(encoded.length + ((4 - (encoded.length % 4)) % 4), "=");
    const byteLength = atob(padded).length;
    return byteLength >= 24 && byteLength <= 64;
  } catch {
    return false;
  }
}

function newWebhook() {
  return {
    id: crypto.randomUUID?.() ?? `webhook-${Date.now()}-${randomHex(6)}`,
    name: t("Nueva aplicación"),
    url: "",
    secret: randomWebhookSecret(),
    enabled: true,
    scope: { kind: "all" },
  };
}

function webhookChannelValue(channel) {
  return `channel:${channel.guildId}:${channel.channelId}`;
}

function configuredChannel(webhook) {
  if (webhook.scope?.kind !== "channel") return null;
  return state.webhookChannels.find(
    (channel) =>
      channel.guildId === webhook.scope["guild-id"] &&
      channel.channelId === webhook.scope["channel-id"],
  );
}

function webhookCard(webhook) {
  const card = document.createElement("article");
  card.className = "webhook-card";

  const head = document.createElement("div");
  head.className = "webhook-card-head";
  const identity = document.createElement("label");
  identity.className = "webhook-identity";
  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = webhook.enabled !== false;
  enabled.addEventListener("change", () => {
    webhook.enabled = enabled.checked;
    card.classList.toggle("disabled", !enabled.checked);
  });
  const title = document.createElement("strong");
  title.textContent = webhook.name.trim() || t("Aplicación sin nombre");
  identity.append(enabled, title);

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "ghost danger";
  remove.textContent = t("Quitar");
  remove.addEventListener("click", async () => {
    const accepted = await askForConfirmation({
      kind: t("Eliminar suscripción"),
      title: t("¿Quitar esta aplicación?"),
      target: `${webhook.name.trim() || t("Aplicación sin nombre")}\n${webhook.url || t("Sin URL")}`,
      description: t(
        "Kuali dejará de enviarle reuniones. Esto no elimina ningún dato que la aplicación ya haya recibido.",
      ),
      action: t("Quitar suscripción"),
    });
    if (!accepted) return;
    state.webhooks = state.webhooks.filter((candidate) => candidate.id !== webhook.id);
    renderWebhooks();
  });
  head.append(identity, remove);

  const fields = document.createElement("div");
  fields.className = "webhook-fields";

  const nameField = document.createElement("label");
  nameField.className = "field";
  const nameLabel = document.createElement("span");
  nameLabel.textContent = t("Nombre de la aplicación");
  const name = document.createElement("input");
  name.type = "text";
  name.value = webhook.name;
  name.placeholder = t("Ej.: Tareas internas");
  name.autocomplete = "off";
  name.addEventListener("input", () => {
    webhook.name = name.value;
    title.textContent = name.value.trim() || t("Aplicación sin nombre");
  });
  nameField.append(nameLabel, name);

  const urlField = document.createElement("label");
  urlField.className = "field webhook-url-field";
  const urlLabel = document.createElement("span");
  urlLabel.textContent = t("URL receptora");
  const url = document.createElement("input");
  url.type = "url";
  url.value = webhook.url;
  url.placeholder = "http://localhost:3000/webhooks/kuali";
  url.autocomplete = "off";
  url.spellcheck = false;
  url.addEventListener("input", () => (webhook.url = url.value.trim()));
  urlField.append(urlLabel, url);

  const scopeField = document.createElement("label");
  scopeField.className = "field webhook-scope-field";
  const scopeLabel = document.createElement("span");
  scopeLabel.textContent = t("Reuniones que recibirá");
  const scope = document.createElement("select");
  scope.appendChild(new Option(t("Todos los servidores y canales"), "all"));
  for (const channel of state.webhookChannels) {
    scope.appendChild(
      new Option(`${channel.guildName} · # ${channel.channelName}`, webhookChannelValue(channel)),
    );
  }
  scope.appendChild(new Option(t("Otro canal por ID…"), "manual"));
  const knownChannel = configuredChannel(webhook);
  scope.value = webhook.scope?.kind === "all"
    ? "all"
    : knownChannel
      ? webhookChannelValue(knownChannel)
      : "manual";
  scope.addEventListener("change", () => {
    if (scope.value === "all") {
      webhook.scope = { kind: "all" };
    } else if (scope.value === "manual") {
      webhook.scope = webhook.scope?.kind === "channel"
        ? webhook.scope
        : { kind: "channel", "guild-id": "", "channel-id": "" };
    } else {
      const [, guildId, channelId] = scope.value.split(":");
      webhook.scope = { kind: "channel", "guild-id": guildId, "channel-id": channelId };
    }
    renderWebhooks();
  });
  scopeField.append(scopeLabel, scope);

  fields.append(nameField, urlField, scopeField);

  if (webhook.scope?.kind === "channel" && !knownChannel) {
    const manual = document.createElement("div");
    manual.className = "webhook-manual-scope";
    for (const [label, key] of [
      [t("ID del servidor"), "guild-id"],
      [t("ID del canal"), "channel-id"],
    ]) {
      const field = document.createElement("label");
      field.className = "field";
      const text = document.createElement("span");
      text.textContent = label;
      const input = document.createElement("input");
      input.type = "text";
      input.inputMode = "numeric";
      input.value = webhook.scope[key] ?? "";
      input.placeholder = "123456789012345678";
      input.autocomplete = "off";
      input.spellcheck = false;
      input.addEventListener("input", () => (webhook.scope[key] = input.value.trim()));
      field.append(text, input);
      manual.appendChild(field);
    }
    fields.appendChild(manual);
  }

  const secretField = document.createElement("div");
  secretField.className = "field webhook-secret-field";
  const secretLabel = document.createElement("span");
  secretLabel.textContent = t("Secreto para verificar la firma");
  const secretEntry = document.createElement("div");
  secretEntry.className = "secret-entry";
  const secret = document.createElement("input");
  secret.type = "password";
  secret.value = webhook.secret;
  secret.readOnly = true;
  secret.spellcheck = false;
  secret.setAttribute("aria-label", t("Secreto de {name}", { name: webhook.name }));
  const reveal = document.createElement("button");
  reveal.type = "button";
  reveal.className = "ghost";
  reveal.textContent = t("Ver");
  reveal.addEventListener("click", () => {
    const visible = secret.type === "text";
    secret.type = visible ? "password" : "text";
    reveal.textContent = visible ? t("Ver") : t("Ocultar");
  });
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "ghost";
  copy.textContent = t("Copiar");
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(webhook.secret);
      toast(t("Secreto copiado"), webhook.name || "webhook");
    } catch {
      secret.type = "text";
      secret.focus();
      secret.select();
      toast(t("No pude copiarlo automáticamente; quedó seleccionado"), "webhook", true);
    }
  });
  const regenerate = document.createElement("button");
  regenerate.type = "button";
  regenerate.className = "ghost";
  regenerate.textContent = t("Regenerar");
  regenerate.addEventListener("click", async () => {
    const accepted = await askForConfirmation({
      kind: t("Cambiar secreto"),
      title: t("¿Generar un secreto nuevo?"),
      target: webhook.name.trim() || t("Aplicación sin nombre"),
      description: t(
        "La aplicación rechazará los siguientes eventos hasta que actualices allí el secreto compartido.",
      ),
      action: t("Generar secreto"),
    });
    if (!accepted) return;
    webhook.secret = randomWebhookSecret();
    renderWebhooks();
  });
  secretEntry.append(secret, reveal, copy, regenerate);
  secretField.append(secretLabel, secretEntry);
  const standardSecret = isStandardWebhookSecret(webhook.secret);
  if (!standardSecret) {
    const warning = document.createElement("small");
    warning.className = "webhook-secret-warning";
    warning.textContent = t(
      "Este secreto usa el formato anterior. Regénéralo y actualízalo en la aplicación receptora.",
    );
    secretField.append(warning);
  }

  const actions = document.createElement("div");
  actions.className = "webhook-actions";
  const test = document.createElement("button");
  test.type = "button";
  test.className = "ghost";
  test.textContent = t("Enviar prueba");
  test.disabled = !standardSecret;
  const result = document.createElement("span");
  result.className = "webhook-test-result";
  result.setAttribute("role", "status");
  test.addEventListener("click", async () => {
    test.disabled = true;
    result.className = "webhook-test-result";
    result.textContent = t("Enviando…");
    try {
      result.textContent = await invoke("test_webhook", { webhook: structuredClone(webhook) });
      result.classList.add("ok");
    } catch (error) {
      result.textContent = String(error);
      result.classList.add("failed");
    } finally {
      test.disabled = false;
    }
  });
  actions.append(test, result);

  card.classList.toggle("disabled", webhook.enabled === false);
  card.append(head, fields, secretField, actions);
  return card;
}

function renderWebhooks() {
  $("webhook-empty").hidden = state.webhooks.length > 0;
  $("webhook-list").replaceChildren(...state.webhooks.map(webhookCard));
}

// --- summary providers ----------------------------------------------------

/** Returns the selected catalog provider, unless selection is automatic. */
function selectedProvider() {
  return state.providers.find((p) => p.id === state.selectedProvider) ?? null;
}

/** Returns editable settings for a provider, creating them on first access. */
function providerSettings(id) {
  const existing = state.providerSettings[id];
  if (existing) return existing;
  const fresh = { "api-key": "", model: null, "base-url": null };
  state.providerSettings[id] = fresh;
  return fresh;
}

function renderProviders() {
  const cards = state.providers.map((provider) => {
    const card = document.createElement("button");
    card.type = "button";
    card.className = "provider-card";
    card.dataset.provider = provider.id;
    card.setAttribute("role", "radio");
    card.setAttribute("aria-checked", String(state.selectedProvider === provider.id));
    card.classList.toggle("selected", state.selectedProvider === provider.id);
    card.classList.toggle("unavailable", !provider.available);

    const title = document.createElement("span");
    title.className = "provider-card-title";
    title.textContent = provider.label;

    const badge = document.createElement("span");
    badge.className = `provider-badge ${provider.available ? "ready" : "missing"}`;
    badge.textContent = provider.available
      ? provider.kind === "localCli"
        ? t("Sesión local")
        : t("Configurado")
      : t("Falta configurar");

    const detail = document.createElement("small");
    detail.className = "provider-card-detail";
    detail.textContent = provider.available
      ? provider.model
        ? t("Modelo: {model}", { model: provider.model })
        : t(provider.description)
      : t(provider.missing ?? "");

    card.append(title, badge, detail);
    card.addEventListener("click", () => {
      // Keep the previous provider's edits in memory so browsing another
      // provider never discards a pasted key.
      captureProviderSettings();
      // Selecting the current provider again returns to automatic mode without
      // introducing a confusing "none" option.
      state.selectedProvider = state.selectedProvider === provider.id ? "" : provider.id;
      renderProviders();
    });
    return card;
  });

  $("provider-list").replaceChildren(...cards);
  renderProviderHint();
  renderProviderSettings();
}

function renderProviderHint() {
  const available = state.providers.filter((p) => p.available);
  const hint = $("provider-hint");

  if (available.length === 0) {
    hint.textContent =
      t("No hay ninguno listo. Instala Claude Code, o elige una API y pega su clave.");
    return;
  }
  if (!state.selectedProvider) {
    hint.textContent = t("Automático: se usará {provider}. Elige uno para fijarlo.", {
      provider: available[0].label,
    });
    return;
  }
  const chosen = selectedProvider();
  hint.textContent = chosen?.available
    ? t("Kuali usará {provider}. Vuelve a pulsarlo para dejarlo en automático.", {
        provider: chosen.label,
      })
    : t("Este proveedor todavía no está listo: sin arreglarlo, el resumen fallará al colgar.");
}

function renderProviderSettings() {
  const panel = $("provider-settings");
  const provider = selectedProvider();
  if (!provider) {
    panel.hidden = true;
    $("provider-test-result").textContent = "";
    return;
  }

  const settings = providerSettings(provider.id);
  panel.hidden = false;
  $("provider-settings-title").textContent = provider.label;
  $("provider-settings-hint").textContent = t(provider.description);

  const needsKey = provider.kind === "remoteApi";
  $("provider-key-field").hidden = !needsKey;
  if (needsKey) {
    // Mask the key on every render; changing providers must not reveal a secret
    // without an explicit request.
    $("cfg-provider-key").type = "password";
    $("btn-toggle-provider-key").textContent = t("Ver");
    $("btn-toggle-provider-key").setAttribute("aria-pressed", "false");
    $("cfg-provider-key").value = settings["api-key"] ?? "";
    $("provider-key-hint").textContent = provider.apiKeyFromEnvironment
      ? t("Ahora mismo se usa la clave del entorno. Escribe una aquí para que mande esta.")
      : t("Se guarda en el config.toml de Kuali, legible solo por tu usuario.");
  }

  $("provider-base-url-field").hidden = !provider.configurableBaseUrl;
  if (provider.configurableBaseUrl) {
    $("cfg-provider-base-url").value = settings["base-url"] ?? "";
    $("cfg-provider-base-url").placeholder = provider.defaultBaseUrl ?? "";
  }

  $("cfg-provider-model").value = settings.model ?? "";
  $("cfg-provider-model").placeholder = provider.defaultModel || t("El de la herramienta…");
  $("btn-refresh-models").hidden = !provider.listsModels;
  renderModelOptions(provider);

  $("provider-test-result").textContent = "";
  $("provider-test-result").className = "provider-test-result";

  // Query providers as soon as their catalog is reachable so the list stays
  // current. Do not wait for full availability: some providers require a model
  // selection that can only come from this catalog.
  if (canListModels(provider) && !state.providerModels[provider.id]) {
    refreshModels(provider.id, { quiet: true });
  }
}

/** Whether the provider exposes a catalog and has the required credentials. */
function canListModels(provider) {
  if (!provider.listsModels) return false;
  if (!provider.needsApiKey) return true;
  return Boolean(providerSettings(provider.id)["api-key"] || provider.apiKeyFromEnvironment);
}

function renderModelOptions(provider) {
  const live = state.providerModels[provider.id];
  const options = live ?? provider.models;

  $("provider-model-options").replaceChildren(
    ...options.map((option) =>
      Object.assign(document.createElement("option"), {
        value: option.id,
        label: t(option.label),
      }),
    ),
  );

  const hint = $("provider-model-hint");
  const source =
    provider.kind === "localCli"
      ? t("según la propia herramienta")
      : t("publicados ahora mismo por {provider}", { provider: provider.label });

  if (live) {
    hint.textContent = t("{count} modelos {source}. Puedes escribir otro identificador si prefieres.", {
      count: live.length,
      source,
    });
  } else if (provider.listsModels) {
    hint.textContent = t("Consultando los modelos disponibles…");
  } else {
    hint.textContent = t("Escribe el identificador exacto tal y como lo nombra el proveedor.");
  }
}

/** Requests a model catalog using the provider settings currently on screen. */
async function refreshModels(id, { quiet = false } = {}) {
  const button = $("btn-refresh-models");
  const hint = $("provider-model-hint");
  captureProviderSettings();

  if (!quiet) {
    button.disabled = true;
    hint.textContent = t("Consultando los modelos disponibles…");
  }
  try {
    const models = await invoke("provider_models", {
      id,
      settings: providerSettings(id),
    });
    state.providerModels[id] = models;
  } catch (e) {
    // Failure is non-fatal: bundled suggestions and free-form input remain.
    if (selectedProvider()?.id === id) hint.textContent = String(e);
    return;
  } finally {
    button.disabled = false;
  }

  // The visible provider may have changed while the request was pending.
  const provider = selectedProvider();
  if (provider?.id === id) renderModelOptions(provider);
}

/** Captures values from the visible provider fields. */
function captureProviderSettings() {
  const provider = selectedProvider();
  if (!provider) return;
  const settings = providerSettings(provider.id);
  if (provider.kind === "remoteApi") settings["api-key"] = $("cfg-provider-key").value.trim();
  if (provider.configurableBaseUrl) {
    settings["base-url"] = $("cfg-provider-base-url").value.trim() || null;
  }
  settings.model = $("cfg-provider-model").value.trim() || null;
}

/** Returns settings ready to persist, omitting empty providers. */
function providerSettingsForConfig() {
  return Object.fromEntries(
    Object.entries(state.providerSettings).filter(
      ([, settings]) => settings["api-key"] || settings.model || settings["base-url"],
    ),
  );
}

async function testProvider() {
  const provider = selectedProvider();
  if (!provider) return;

  const button = $("btn-test-provider");
  const result = $("provider-test-result");
  captureProviderSettings();

  button.disabled = true;
  result.className = "provider-test-result";
  result.textContent = t("Probando…");
  try {
    // Test the on-screen values rather than persisted ones. A test must not save
    // a key when the user later cancels Settings.
    result.textContent = await invoke("test_provider", {
      id: provider.id,
      settings: providerSettings(provider.id),
    });
    result.classList.add("ok");
  } catch (e) {
    result.textContent = String(e);
    result.classList.add("failed");
  } finally {
    button.disabled = false;
  }
}

function updateModelHint() {
  const chosen = state.models.find((m) => m.id === $("cfg-model").value);
  if (!chosen) return;
  $("model-hint").textContent = chosen.downloaded
    ? t("Ya está descargado en {directory}.", { directory: state.modelsDirectory })
    : t("Sin descargar. Ocupa {size}; se guardará fuera del ejecutable.", {
        size: humanBytes(chosen.approxBytes),
      });
  $("btn-download").hidden = chosen.downloaded;
}

function renderInstalledModels() {
  const installed = state.models.filter((model) => model.downloaded);
  const container = $("installed-models");
  if (installed.length === 0) {
    const empty = document.createElement("p");
    empty.className = "installed-models-empty";
    empty.textContent = t("Todavía no hay pesos descargados en esta carpeta.");
    container.replaceChildren(empty);
    return;
  }

  container.replaceChildren(
    ...installed.map((model) => {
      const row = document.createElement("div");
      row.className = "installed-model";

      const info = document.createElement("span");
      const name = document.createElement("strong");
      name.textContent = t(model.label);
      const size = document.createElement("small");
      size.textContent = t("{size} aprox. · {id}", {
        size: humanBytes(model.approxBytes),
        id: model.id,
      });
      info.append(name, size);

      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "ghost danger";
      remove.textContent = t("Eliminar");
      remove.setAttribute("aria-label", t("Eliminar los pesos de {model}", { model: model.label }));
      remove.addEventListener("click", () => deleteModel(model, remove));
      row.append(info, remove);
      return row;
    }),
  );
}

function updateModelsDirectoryHint() {
  const chosen = $("cfg-models-directory").value.trim();
  $("models-directory-hint").textContent = chosen
    ? t("Al guardar, Kuali moverá aquí los pesos existentes y descargará el modelo elegido si falta.")
    : t("Predeterminada: ~/.kuali ({directory}). Los pesos se trasladan al guardar.", {
        directory: state.modelsDirectory,
      });
}

function renderCustomVocabulary() {
  $("vocabulary-chips").replaceChildren(
    ...state.customVocabulary.map((term, index) => {
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = "vocabulary-chip";
      chip.title = t("Quitar {term}", { term });
      chip.setAttribute("aria-label", t("Quitar {term}", { term }));

      const text = document.createElement("span");
      text.textContent = term;
      const remove = document.createElement("span");
      remove.className = "remove-term";
      remove.setAttribute("aria-hidden", "true");
      remove.textContent = "×";
      chip.append(text, remove);
      chip.addEventListener("click", () => {
        state.customVocabulary.splice(index, 1);
        renderCustomVocabulary();
      });
      return chip;
    }),
  );
}

function addCustomVocabulary() {
  const input = $("cfg-vocabulary-input");
  const candidates = input.value
    .split(/[,;\n]+/)
    .map((term) => term.trim().replace(/\s+/g, " "))
    .filter(Boolean);

  for (const term of candidates) {
    if (state.customVocabulary.length >= 64) {
      toast(t("Whisper admite hasta 64 términos personalizados en Kuali."), t("vocabulario"), true);
      break;
    }
    if (!state.customVocabulary.some((current) => current.toLowerCase() === term.toLowerCase())) {
      state.customVocabulary.push(term.slice(0, 80));
    }
  }
  input.value = "";
  renderCustomVocabulary();
  input.focus();
}

function renderModelProgress() {
  const s = state.modelState;
  const row = $("download-row");
  if (s.state !== "downloading") {
    row.hidden = true;
    return;
  }
  row.hidden = false;
  const pct = s.totalBytes ? Math.round((s.downloadedBytes / s.totalBytes) * 100) : 0;
  $("progress-bar").style.width = `${pct}%`;
  $("progress-text").textContent = `${pct}% · ${humanBytes(s.downloadedBytes)}`;
}

async function saveSettings() {
  const c = structuredClone(state.config);
  const saveButton = $("btn-save-settings");
  if ($("cfg-vocabulary-input").value.trim()) addCustomVocabulary();
  const webPort = Number($("cfg-web-port").value);
  if (!Number.isInteger(webPort) || webPort < 1 || webPort > 65535) {
    toast(
      t("El puerto de la extensión debe estar entre 1 y 65535."),
      t("reuniones web"),
      true,
    );
    $("cfg-web-port").focus();
    return;
  }

  c.discord["bot-token"] = $("cfg-token").value.trim();
  const follow = normalizedDiscordUsername($("cfg-follow").value);
  const previousFollow = normalizedDiscordUsername(c.discord["follow-username"] ?? "");
  c.discord["follow-username"] = follow || null;
  // A new @username must be resolved again. Preserve legacy IDs only when the
  // migrated configuration did not yet contain a username.
  if (follow.toLowerCase() !== previousFollow.toLowerCase()) {
    c.discord["follow-user-id"] = null;
  }
  c.discord["follow-automatically"] = $("cfg-follow-automatically").checked;
  c.discord["leave-when-empty"] = $("cfg-leave-empty").checked;
  c.discord["post-summary-to-channel"] = $("cfg-post-summary").checked;

  c.meet ??= {};
  c.meet.enabled = $("cfg-web-enabled").checked;
  c.meet.port = webPort;

  c.whisper.model = $("cfg-model").value;
  c.whisper["models-directory"] = $("cfg-models-directory").value.trim() || null;
  c.whisper.language = $("cfg-language").value;
  c.whisper["custom-vocabulary"] = [...state.customVocabulary];

  c.integrations ??= {};
  c.integrations.webhooks = structuredClone(state.webhooks);

  captureProviderSettings();
  c.llm["preferred-provider"] = state.selectedProvider || null;
  c.llm.providers = providerSettingsForConfig();
  // Provider-specific model settings superseded the global one. Clear it so it
  // cannot be applied again after a reload.
  c.llm["model-override"] = null;
  c.llm["output-language"] = $("cfg-output-language").value.trim() || "español";
  c.llm["summarize-on-leave"] = $("cfg-summarize").checked;

  c.application ??= {};
  c.application.language = $("cfg-ui-language").value;
  c.application["automatic-updates"] = $("cfg-automatic-updates").checked;

  saveButton.disabled = true;
  $("save-note").textContent = t("Guardando y moviendo pesos…");
  try {
    await invoke("set_config", { config: c });
    await invoke("set_autostart_enabled", { enabled: $("cfg-autostart").checked });
    state.autostartEnabled = $("cfg-autostart").checked;
    state.config = c;
    scheduleAutomaticUpdateChecks();
    setLanguagePreference(c.application.language);
    state.modelsDirectory = await invoke("resolved_models_directory");
    state.models = await invoke("whisper_models");
    renderRequiredModelNotice(c.whisper.model);
    // Do not rely on the state event winning a race with modal closure. Fetch
    // the engine's current truth before rendering.
    const snapshot = await invoke("get_snapshot");
    state.status = snapshot.status;
    state.modelState = snapshot.modelState;
    state.webMeetings = snapshot.webMeetings;
    $("save-note").textContent = t("Guardado");
    setTimeout(() => ($("settings-modal").hidden = true), 550);
    renderStatus();
    // Saving settings must preserve the current section, including task filters
    // and the visible setup-guide step.
    if (state.currentPane === "tasks") await refreshTasks(true);
    else if (state.currentPane === "guide") await renderGuide();
    else await renderRoot();
  } catch (e) {
    $("save-note").textContent = t("No se pudo guardar");
    toast(String(e), t("ajustes"), true);
  } finally {
    saveButton.disabled = false;
  }
}

function updateSummarySettingsVisibility() {
  $("summary-settings-details").hidden = !$("cfg-summarize").checked;
}

async function toggleAutomaticFollow() {
  if (!state.config || !hasAutomaticFollowTarget()) return;
  const button = $("btn-toggle-follow");
  const enabled = !isAutomaticFollowEnabled();
  const config = structuredClone(state.config);
  config.discord["follow-automatically"] = enabled;
  button.disabled = true;
  button.textContent = enabled
    ? t("Activando seguimiento de Discord…")
    : t("Pausando seguimiento de Discord…");
  try {
    await invoke("set_config", { config });
    state.config = config;
    $("cfg-follow-automatically").checked = enabled;
    const snapshot = await invoke("get_snapshot");
    state.status = snapshot.status;
    state.modelState = snapshot.modelState;
    state.webMeetings = snapshot.webMeetings;
    renderStatus();
    await renderRoot();
    toast(
      enabled
        ? t("Kuali volverá a seguirte cuando entres a una llamada")
        : t("Seguimiento de Discord pausado; puedes entrar sin que Kuali te siga"),
      "Discord",
    );
  } catch (error) {
    toast(String(error), t("seguimiento de Discord"), true);
    renderStatus();
  } finally {
    button.disabled = false;
  }
}

async function deleteModel(model, button) {
  if (!model.downloaded) return;

  const selectedWarning = model.id === state.config?.whisper?.model
    ? t(" Es el modelo elegido actualmente, así que Kuali tendrá que descargarlo otra vez o usar otro antes de la próxima transcripción.")
    : "";
  const accepted = await askForConfirmation({
    kind: t("Eliminar pesos"),
    title: t("¿Eliminar este modelo?"),
    target: `${t(model.label)}\n${model.id}`,
    description: t(
      "Liberará aproximadamente {size} de {directory}. No eliminará Whisper, Silero ni los demás modelos.{selectedWarning} Esta acción no se puede deshacer.",
      {
        size: humanBytes(model.approxBytes),
        directory: state.modelsDirectory,
        selectedWarning,
      },
    ),
    action: t("Eliminar pesos"),
  });
  if (!accepted) return;

  const selected = $("cfg-model").value;
  button.disabled = true;
  button.textContent = t("Eliminando…");
  try {
    const removedBytes = await invoke("delete_model", { model: model.id });
    state.models = await invoke("whisper_models");
    renderRequiredModelNotice(selected);
    renderWhisperModelOptions(selected);
    const snapshot = await invoke("get_snapshot");
    state.modelState = snapshot.modelState;
    updateModelHint();
    renderInstalledModels();
    renderModelProgress();
    renderStatus();
    toast(
      removedBytes > 0
        ? t("{size} liberados", { size: humanBytes(removedBytes) })
        : t("El modelo ya no estaba en disco"),
      "Whisper",
    );
  } catch (error) {
    toast(String(error), "eliminar modelo", true);
    button.disabled = false;
    button.textContent = t("Eliminar");
  }
}

// --- startup --------------------------------------------------------------

function selectedGuideImageSource(image) {
  return currentLocale().startsWith("es")
    ? image.dataset.guideSrcEs
    : image.dataset.guideSrcEn;
}

function refreshGuideImages() {
  for (const figure of document.querySelectorAll("[data-guide-image]")) {
    const image = figure.querySelector("img");
    if (!image) continue;
    const source = selectedGuideImageSource(image);
    if (!source) {
      figure.hidden = true;
      continue;
    }
    if (image.getAttribute("src") !== source) {
      figure.hidden = true;
      image.setAttribute("src", source);
    } else if (image.complete) {
      figure.hidden = image.naturalWidth === 0;
    }
  }
}

function closeGuideImage() {
  const lightbox = $("guide-image-lightbox");
  if (lightbox.hidden) return;
  lightbox.hidden = true;
  $("guide-image-large").removeAttribute("src");
  $("guide-image-lightbox-caption").textContent = "";
  state.guideImageReturnFocus?.focus?.();
  state.guideImageReturnFocus = null;
}

function openGuideImage(button) {
  const image = button.querySelector("img");
  if (!image?.naturalWidth) return;
  const figure = button.closest("[data-guide-image]");
  state.guideImageReturnFocus = button;
  const largeImage = $("guide-image-large");
  largeImage.src = image.currentSrc || image.src;
  largeImage.alt = image.alt;
  $("guide-image-lightbox-caption").textContent =
    figure?.querySelector(".guide-example-caption")?.textContent?.trim() ?? "";
  $("guide-image-lightbox").hidden = false;
  requestAnimationFrame(() => $("btn-close-guide-image").focus());
}

function wireGuideImages() {
  for (const figure of document.querySelectorAll("[data-guide-image]")) {
    const image = figure.querySelector("img");
    const button = figure.querySelector(".guide-image-open");
    if (!image || !button) continue;
    const reveal = () => {
      figure.hidden = image.getAttribute("src") !== selectedGuideImageSource(image)
        || image.naturalWidth === 0;
    };
    image.addEventListener("load", reveal);
    image.addEventListener("error", () => {
      figure.hidden = true;
    });
    button.addEventListener("click", () => openGuideImage(button));
  }
  refreshGuideImages();
}

async function renderForLanguageChange() {
  closeGuideImage();
  localizeStaticDocument();
  refreshGuideImages();
  if (!$("factory-reset-modal").hidden && !state.factoryResetPending) {
    refreshFactoryResetConfirmation({ clear: true });
  }
  renderStatus();
  renderUpdateState();
  renderMeetingList();
  renderLiveMeetingList();
  if (state.currentPane === "meeting") renderMeeting();
  else if (state.currentPane === "tasks") {
    renderGlobalTaskFilters();
    renderGlobalTasks();
  } else if (state.currentPane === "guide") {
    await renderGuide();
  } else if (["idle", "setup"].includes(state.currentPane)) {
    await renderRoot();
  }
  if (!$("settings-modal").hidden) {
    renderProviders();
    renderWebhooks();
    renderInstalledModels();
  }
}

function wireUp() {
  let librarySearchTimer;
  let taskSearchTimer;
  wireGuideImages();
  window.addEventListener("kuali:languagechange", () => {
    renderForLanguageChange().catch((error) => toast(String(error), "Kuali", true));
  });
  $("context-select").addEventListener("click", toggleContextSelection);
  $("context-delete").addEventListener("click", deleteContextTarget);
  $("library-context-menu").addEventListener("keydown", (event) => {
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    const items = [$("context-select"), $("context-delete")].filter((item) => !item.disabled);
    if (items.length === 0) return;
    event.preventDefault();
    const current = items.indexOf(document.activeElement);
    let next;
    if (event.key === "Home") next = 0;
    else if (event.key === "End") next = items.length - 1;
    else if (event.key === "ArrowDown") next = (current + 1 + items.length) % items.length;
    else next = (current - 1 + items.length) % items.length;
    items[next].focus();
  });
  $("btn-confirm-cancel").addEventListener("click", () => settleConfirmation(false));
  $("btn-confirm-accept").addEventListener("click", () => settleConfirmation(true));
  $("confirm-modal").addEventListener("pointerdown", (event) => {
    if (event.target === event.currentTarget) settleConfirmation(false);
  });
  $("confirm-modal").addEventListener("keydown", (event) => {
    if (event.key !== "Tab") return;
    const controls = [$("btn-confirm-cancel"), $("btn-confirm-accept")];
    const current = controls.indexOf(document.activeElement);
    if (event.shiftKey && current <= 0) {
      event.preventDefault();
      controls.at(-1).focus();
    } else if (!event.shiftKey && current === controls.length - 1) {
      event.preventDefault();
      controls[0].focus();
    }
  });
  $("btn-close-guide-image").addEventListener("click", closeGuideImage);
  $("guide-image-lightbox").addEventListener("pointerdown", (event) => {
    if (event.target === event.currentTarget) closeGuideImage();
  });
  $("guide-image-lightbox").addEventListener("keydown", (event) => {
    if (event.key !== "Tab") return;
    event.preventDefault();
    $("btn-close-guide-image").focus();
  });
  $("btn-open-factory-reset").addEventListener("click", openFactoryResetDialog);
  $("factory-reset-input").addEventListener("input", () => refreshFactoryResetConfirmation());
  $("factory-reset-input").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !$("btn-confirm-factory-reset").disabled) {
      event.preventDefault();
      performFactoryReset();
    }
  });
  $("btn-cancel-factory-reset").addEventListener("click", closeFactoryResetDialog);
  $("btn-confirm-factory-reset").addEventListener("click", performFactoryReset);
  $("factory-reset-modal").addEventListener("pointerdown", (event) => {
    if (event.target === event.currentTarget) closeFactoryResetDialog();
  });
  $("factory-reset-modal").addEventListener("keydown", (event) => {
    if (event.key !== "Tab") return;
    const controls = [
      $("factory-reset-input"),
      $("btn-cancel-factory-reset"),
      $("btn-confirm-factory-reset"),
    ].filter((control) => !control.disabled);
    const current = controls.indexOf(document.activeElement);
    if (event.shiftKey && current <= 0) {
      event.preventDefault();
      controls.at(-1).focus();
    } else if (!event.shiftKey && current === controls.length - 1) {
      event.preventDefault();
      controls[0].focus();
    }
  });
  document.addEventListener("pointerdown", (event) => {
    const menu = $("library-context-menu");
    if (!menu.hidden && !menu.contains(event.target)) closeLibraryContextMenu();
    if (!event.target.closest(".task-filter-control")) closeTaskFilterPopovers();
  });
  $("btn-cancel-selection").addEventListener("click", () => setLibrarySelectionMode(false));
  $("btn-delete-selected").addEventListener("click", deleteSelectedMeetings);
  $("btn-select-visible").addEventListener("click", () => {
    const meetings = state.meetings.filter((meeting) => !isLiveMeeting(meeting.id));
    const allSelected =
      meetings.length > 0 && meetings.every((meeting) => state.selectedMeetingIds.has(meeting.id));
    for (const meeting of meetings) setMeetingSelection(meeting, !allSelected);
    renderMeetingList();
  });
  $("library-search").addEventListener("input", (event) => {
    clearTimeout(librarySearchTimer);
    state.libraryQuery = event.currentTarget.value;
    state.libraryScrollTop = 0;
    $("meeting-list").scrollTop = 0;
    $("library-search-status").textContent = state.libraryQuery.trim() ? "Buscando…" : "";
    librarySearchTimer = setTimeout(refreshMeetings, 180);
  });
  $("library-search").addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !event.currentTarget.value) return;
    event.stopPropagation();
    event.currentTarget.value = "";
    state.libraryQuery = "";
    state.libraryScrollTop = 0;
    $("meeting-list").scrollTop = 0;
    clearTimeout(librarySearchTimer);
    refreshMeetings();
  });
  $("meeting-list").addEventListener("scroll", (event) => {
    state.libraryScrollTop = event.currentTarget.scrollTop;
  }, { passive: true });
  $("btn-home").addEventListener("click", goHome);
  $("nav-home").addEventListener("click", goHome);
  $("nav-tasks").addEventListener("click", showTasks);
  $("btn-guide").addEventListener("click", showGuide);
  $("btn-finish-guide").addEventListener("click", finishInitialSetup);
  $("required-model-select").addEventListener("change", (event) =>
    renderRequiredModelNotice(event.currentTarget.value));
  $("btn-required-model").addEventListener("click", downloadRequiredModel);
  $("btn-discord-guide-back").addEventListener("click", () =>
    setDiscordGuideStep(state.discordGuideStep - 1));
  $("btn-discord-guide-next").addEventListener("click", advanceDiscordGuide);
  $("btn-meet-guide-back").addEventListener("click", () =>
    setMeetGuideStep(state.meetGuideStep - 1));
  $("btn-meet-guide-next").addEventListener("click", advanceMeetGuide);
  $("guide-token").addEventListener("input", () => {
    $("guide-token-error").textContent = "";
  });
  $("guide-discord-username").addEventListener("input", () => {
    $("guide-username-error").textContent = "";
  });
  $("btn-save-discord-guide").addEventListener("click", async () => {
    if (await saveDiscordFromGuide()) setDiscordGuideStep(2);
  });
  $("btn-open-discord-install").addEventListener("click", async () => {
    if (await openDiscordInstallFromGuide()) {
      toast(t("Autorización de Discord abierta"), t("guía"));
    }
  });
  $("btn-open-discord-portal").addEventListener("click", () =>
    invoke("open_setup_destination", { destination: "discord-developers" })
      .catch((error) => toast(String(error), "guía", true)));
  for (const button of document.querySelectorAll("[data-browser-store]")) {
    button.addEventListener("click", async () => {
      const browser = button.dataset.browserStore;
      try {
        await invoke("open_browser_extension_store", { browser });
        if (state.meetGuideStep === 0) setMeetGuideStep(1);
      } catch (error) {
        await copyText(KUALI_EXTENSION_STORE_URL, t("Enlace"));
        toast(t("No pude abrir {browser}. Pega el enlace copiado en ese navegador.", {
          browser,
        }), t("guía"), true);
      }
    });
  }
  for (const button of document.querySelectorAll("[data-browser]")) {
    button.addEventListener("click", async () => {
      const browser = button.dataset.browser;
      try {
        await invoke("open_browser_extensions", { browser });
        if (state.meetGuideStep === 0) setMeetGuideStep(1);
      } catch (error) {
        const urls = {
          chrome: "chrome://extensions",
          edge: "edge://extensions",
          brave: "brave://extensions",
          arc: "chrome://extensions",
        };
        await copyText(urls[browser], t("Dirección"));
        toast(t("No pude abrir {browser}. Pega la dirección copiada en ese navegador.", {
          browser,
        }), t("guía"), true);
      }
    });
  }
  $("btn-copy-extension-path").addEventListener("click", () =>
    copyText(state.extensionPath, "Ruta"));
  $("btn-reveal-extension").addEventListener("click", () =>
    invoke("reveal_browser_extension").catch((error) => toast(String(error), "extensión", true)));
  $("btn-settings-open-guide").addEventListener("click", () => {
    $("settings-modal").hidden = true;
    showGuide();
  });
  $("btn-check-update").addEventListener("click", () => checkForUpdates({ manual: true }));
  $("btn-install-update").addEventListener("click", () => installAvailableUpdate());
  $("btn-settings-install-update").addEventListener("click", () => installAvailableUpdate());

  for (const name of ["summary", "tasks"]) {
    $(`meeting-tab-${name}`).addEventListener("click", () => selectMeetingInsightTab(name));
  }
  document.querySelector(".meeting-panel-tabs").addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const next = event.key === "ArrowRight" || event.key === "End" ? "tasks" : "summary";
    selectMeetingInsightTab(next, true);
  });
  $("tasks-search").addEventListener("input", (event) => {
    clearTimeout(taskSearchTimer);
    state.taskFilters.query = event.currentTarget.value;
    state.taskRenderLimit = 250;
    taskSearchTimer = setTimeout(renderGlobalTasks, 120);
  });
  for (const [id, key] of [
    ["tasks-meeting-filter", "meeting"],
    ["tasks-status-filter", "status"],
  ]) {
    $(id).addEventListener("change", (event) => {
      state.taskFilters[key] = event.currentTarget.value;
      state.taskRenderLimit = 250;
      renderGlobalTasks();
    });
  }
  $("tasks-person-trigger").addEventListener("click", () => {
    const open = $("tasks-person-popover").hidden;
    setTaskFilterPopover("person", open);
  });
  $("tasks-person-search").addEventListener("input", renderTaskPersonOptions);
  $("btn-clear-task-people").addEventListener("click", () => {
    state.taskFilters.people.clear();
    state.taskRenderLimit = 250;
    renderTaskPersonOptions();
    renderGlobalTasks();
  });
  $("btn-close-task-people").addEventListener("click", closeTaskFilterPopovers);
  $("tasks-date-trigger").addEventListener("click", () => {
    const open = $("tasks-date-popover").hidden;
    setTaskFilterPopover("date", open);
    if (open) $("tasks-date-from").focus();
  });
  $("btn-clear-task-dates").addEventListener("click", () => {
    $("tasks-date-from").value = "";
    $("tasks-date-to").value = "";
    $("tasks-date-error").textContent = "";
    applyTaskDateRange();
  });
  $("btn-close-task-dates").addEventListener("click", applyTaskDateRange);
  $("btn-settings").addEventListener("click", openSettings);
  $("btn-open-settings").addEventListener("click", openSettings);
  $("btn-close-settings").addEventListener("click", () => ($("settings-modal").hidden = true));
  $("btn-cancel-settings").addEventListener("click", () => ($("settings-modal").hidden = true));
  $("btn-save-settings").addEventListener("click", saveSettings);
  $("cfg-summarize").addEventListener("change", updateSummarySettingsVisibility);
  $("btn-toggle-follow").addEventListener("click", toggleAutomaticFollow);
  $("cfg-web-enabled").addEventListener("change", (event) => {
    $("cfg-web-port").disabled = !event.currentTarget.checked;
  });
  const settingsTabs = [...document.querySelectorAll("[data-settings-tab]")];
  for (const tab of settingsTabs) {
    tab.addEventListener("click", () => selectSettingsTab(tab.dataset.settingsTab));
  }
  $("settings-modal").querySelector("[role='tablist']").addEventListener("keydown", (event) => {
    const current = settingsTabs.indexOf(document.activeElement);
    if (current < 0) return;
    let next = current;
    if (["ArrowDown", "ArrowRight"].includes(event.key)) next = (current + 1) % settingsTabs.length;
    else if (["ArrowUp", "ArrowLeft"].includes(event.key)) {
      next = (current - 1 + settingsTabs.length) % settingsTabs.length;
    } else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = settingsTabs.length - 1;
    else return;
    event.preventDefault();
    selectSettingsTab(settingsTabs[next].dataset.settingsTab, true);
  });
  $("btn-test-provider").addEventListener("click", testProvider);
  $("btn-add-webhook").addEventListener("click", () => {
    state.webhooks.push(newWebhook());
    renderWebhooks();
    $("webhook-list").lastElementChild?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  });
  $("btn-refresh-models").addEventListener("click", () => {
    const provider = selectedProvider();
    if (provider) refreshModels(provider.id);
  });
  // A pasted key makes the catalog reachable, so fetch it immediately without
  // requiring another button press.
  $("cfg-provider-key").addEventListener("change", () => {
    const provider = selectedProvider();
    if (provider && canListModels(provider)) refreshModels(provider.id, { quiet: true });
  });
  $("btn-toggle-provider-key").addEventListener("click", (event) => {
    const field = $("cfg-provider-key");
    const revealed = field.type === "text";
    field.type = revealed ? "password" : "text";
    event.currentTarget.textContent = revealed ? "Ver" : "Ocultar";
    event.currentTarget.setAttribute("aria-pressed", String(!revealed));
  });

  $("cfg-model").addEventListener("change", updateModelHint);
  $("cfg-models-directory").addEventListener("input", updateModelsDirectoryHint);
  $("btn-add-vocabulary").addEventListener("click", addCustomVocabulary);
  $("cfg-vocabulary-input").addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      addCustomVocabulary();
    }
  });
  $("btn-choose-models-directory").addEventListener("click", async () => {
    const button = $("btn-choose-models-directory");
    if (button.disabled) return;
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
    try {
      const path = await invoke("choose_models_directory");
      if (path) {
        $("cfg-models-directory").value = path;
        updateModelsDirectoryHint();
      }
    } catch (e) {
      toast(String(e), "carpeta de modelos", true);
    } finally {
      button.disabled = false;
      button.removeAttribute("aria-busy");
    }
  });

  $("btn-download").addEventListener("click", async () => {
    const model = $("cfg-model").value;
    if ($("cfg-vocabulary-input").value.trim()) addCustomVocabulary();
    $("btn-download").disabled = true;
    try {
      // Download into the directory shown in the form even if the user selected
      // it moments ago and has not saved Settings yet.
      const c = structuredClone(state.config);
      c.whisper.model = model;
      c.whisper["models-directory"] = $("cfg-models-directory").value.trim() || null;
      c.whisper.language = $("cfg-language").value;
      c.whisper["custom-vocabulary"] = [...state.customVocabulary];
      await invoke("set_config", { config: c });
      state.config = c;
      state.modelsDirectory = await invoke("resolved_models_directory");
      await invoke("download_model", { model });
      state.models = await invoke("whisper_models");
      renderRequiredModelNotice(model);
      renderWhisperModelOptions(model);
      updateModelHint();
      renderInstalledModels();
      toast(t("Modelo descargado"), "Whisper");
    } catch (e) {
      toast(String(e), "whisper", true);
    } finally {
      $("btn-download").disabled = false;
    }
  });

  $("btn-folder").addEventListener("click", () =>
    invoke("reveal_data_dir").catch((e) => toast(String(e), "carpeta", true)),
  );

  $("btn-delete").addEventListener("click", async () => {
    if (!state.viewing || isLiveMeeting(state.viewing.meta.id)) return;
    await deleteMeeting(state.viewing.meta, $("btn-delete"));
  });

  $("btn-leave-call").addEventListener("click", async () => {
    if (!state.viewing || !isLiveMeeting(state.viewing.meta.id) || isWebMeeting(state.viewing.meta)) return;
    const leavingMeetingId = state.viewing.meta.id;
  const accepted = await askForConfirmation({
    kind: t("Salir de la llamada"),
    title: t("¿Sacar a Kuali de esta llamada?"),
      target: `${meetingTitle(state.viewing.meta)}\n${shortDate(state.viewing.meta.startedAt)}`,
    description: t(
        "Se cerrará esta grabación y se preparará su resumen. Las demás reuniones continuarán grabándose.",
      ),
      action: t("Sacar de la llamada"),
    });
    if (!accepted) return;

    const button = $("btn-leave-call");
    button.disabled = true;
    button.textContent = t("Saliendo…");
    try {
      await invoke("leave_call");
      const snapshot = await invoke("get_snapshot");
      state.status = snapshot.status;
      state.modelState = snapshot.modelState;
      state.webMeetings = snapshot.webMeetings;
      applyLiveSnapshot(snapshot);
      await refreshMeetings();
      state.viewing = await invoke("load_meeting", { id: leavingMeetingId });
      renderMeeting();
      renderStatus();
      toast(t("Kuali salió de la llamada y sigue esperando en Discord"), t("llamada"));
    } catch (error) {
      toast(String(error), "salir de la llamada", true);
    } finally {
      button.disabled = false;
      button.textContent = t("Sacar de la llamada");
    }
  });

  for (const [id, format] of [
    ["btn-export-md", "markdown"],
    ["btn-export-json", "json"],
  ]) {
    $(id).addEventListener("click", async () => {
      if (!state.viewing) return;
      try {
        const path = await invoke("export_meeting", {
          meetingId: state.viewing.meta.id,
          format,
        });
        if (path) toast(`Exportado a ${path}`, "kuali");
      } catch (e) {
        toast(String(e), "exportar", true);
      }
    });
  }

  $("btn-resummarize").addEventListener("click", async () => {
    if (!state.viewing) return;
    const btn = $("btn-resummarize");
    btn.disabled = true;
    btn.textContent = t("Pensando…");
    try {
      state.viewing.summary = await invoke("resummarize", {
        meetingId: state.viewing.meta.id,
      });
      if (state.viewing.summary.title) {
        state.viewing.meta.displayTitle = state.viewing.summary.title;
      }
      state.tasksLoaded = false;
      renderMeeting();
      await refreshMeetings();
    } catch (e) {
      toast(String(e), "llm", true);
    } finally {
      btn.disabled = false;
      btn.textContent = t("Rehacer resumen");
    }
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !$("factory-reset-modal").hidden) {
      e.preventDefault();
      closeFactoryResetDialog();
      return;
    }
    if (e.key === "Escape" && !$("guide-image-lightbox").hidden) {
      e.preventDefault();
      closeGuideImage();
      return;
    }
    if (e.key === "Escape" && !$("confirm-modal").hidden) {
      e.preventDefault();
      settleConfirmation(false);
      return;
    }
    if (e.key === "Escape" && !$("library-context-menu").hidden) {
      e.preventDefault();
      closeLibraryContextMenu(true);
      return;
    }
    if (e.key === "Escape" && (!["tasks-person-popover", "tasks-date-popover"]
      .every((id) => $(id).hidden))) {
      e.preventDefault();
      closeTaskFilterPopovers();
      return;
    }
    if (e.key === "Escape") $("settings-modal").hidden = true;
    if ((e.metaKey || e.ctrlKey) && e.key === ",") {
      e.preventDefault();
      openSettings();
    }
  });
}

async function resumeConfiguredModelAfterSetup() {
  await refreshRequiredModelNotice(state.config.whisper.model);
  if (!initialSetupCompleted()) return;
  const configured = state.models.find((model) => model.id === state.config.whisper.model);
  if (configured?.downloaded) return;

  // Completed installations retain the previous automatic recovery behavior:
  // an interrupted or manually removed configured weight is fetched again on
  // launch. Fresh installations are excluded so the user can choose first.
  invoke("download_model", { model: state.config.whisper.model })
    .then(async () => {
      await refreshRequiredModelNotice(state.config.whisper.model);
      const snapshot = await invoke("get_snapshot");
      state.modelState = snapshot.modelState;
      renderStatus();
      renderRequiredModelNotice(state.config.whisper.model);
    })
    .catch((error) => toast(String(error), "Whisper", true));
}

async function boot() {
  const resetCompleted = await invoke("take_factory_reset_completed").catch(() => false);
  if (resetCompleted) localStorage.clear();
  wireUp();
  await listen("kuali://event", (e) => handleEvent(e.payload));
  await listen("kuali://navigate", (event) => {
    if (event.payload === "tasks") showTasks();
    else if (event.payload === "guide") showGuide();
    else goHome();
  });
  await listen("kuali://config-changed", async () => {
    state.config = await invoke("get_config");
    setLanguagePreference(state.config.application?.language ?? "auto");
    scheduleAutomaticUpdateChecks();
    await refreshRequiredModelNotice(state.config.whisper.model);
    renderStatus();
    if (["idle", "setup"].includes(state.currentPane)) await renderRoot();
    else if (state.currentPane === "guide") await renderGuide();
  });
  await listen("kuali://update-progress", (event) => {
    state.updateProgress = event.payload;
    if (state.updateStatus === "installing") renderUpdateState();
  });

  const [snapshot, config] = await Promise.all([invoke("get_snapshot"), invoke("get_config")]);
  setLanguagePreference(config.application?.language ?? "auto", { notify: false });
  refreshGuideImages();
  state.status = snapshot.status;
  state.modelState = snapshot.modelState;
  state.webMeetings = snapshot.webMeetings;
  state.config = config;
  applyLiveSnapshot(snapshot, true);
  scheduleAutomaticUpdateChecks();
  await resumeConfiguredModelAfterSetup();

  await refreshMeetings();
  refreshTasks().catch(() => {});
  renderStatus();
  renderUpdateState();
  const destination = location.hash.slice(1);
  if (!initialSetupCompleted()) await showGuide();
  else if (destination === "tasks") await showTasks();
  else if (destination === "guide") await showGuide();
  else if (destination.startsWith("meeting=")) await openMeeting(decodeURIComponent(destination.slice(8)));
  else await renderRoot();
  if (state.viewing) scrollTranscriptToEnd();
}

boot().catch((e) => toast(String(e), t("arranque"), true));
