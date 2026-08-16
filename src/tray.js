/* Kuali — menu bar panel.
 *
 * Same engine, same tokens, its own reading: what is happening now, what
 * happened last, and what is owed. Everything else lives in the main window. */

import { currentLocale, localizeStaticDocument, setLanguagePreference, t } from "./i18n.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

const state = {
  status: "offline",
  live: null,
  config: null,
  elapsedTimer: null,
};

const STATUS_TEXT = {
  offline: ["Desconectado", ""],
  watching: ["Esperando llamada", "watching"],
  joining: ["Entrando…", "working"],
  recording: ["Transcribiendo", "recording"],
  finalizing: ["Terminando transcripción…", "working"],
  summarizing: ["Sacando el resumen…", "working"],
};

function shortDate(iso) {
  return new Date(iso).toLocaleString(currentLocale(), {
    day: "2-digit",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function elapsed(fromIso) {
  const seconds = Math.max(0, Math.floor((Date.now() - new Date(fromIso)) / 1000));
  const minutes = Math.floor(seconds / 60);
  return `${String(minutes).padStart(2, "0")}:${String(seconds % 60).padStart(2, "0")}`;
}

function renderStatus() {
  const [text, dotClass] = STATUS_TEXT[state.status] ?? ["—", ""];
  $("tray-status-text").textContent = t(text);
  $("tray-status-dot").className = `status-dot ${dotClass}`;

  const following = state.config?.discord?.["follow-automatically"] !== false;
  $("tray-follow").checked = following;
  $("tray-follow-control").classList.toggle("paused", !following);
  $("tray-follow-state").textContent = following
    ? t("Kuali entra solo a tus llamadas")
    : t("En pausa · entra con /grabar");
}

function renderLive() {
  const live = state.live;
  $("tray-live").hidden = !live;
  // A call in progress takes the room the recent list would use; history is one
  // click away in the main window.
  $("tray-meetings-section").hidden = Boolean(live);
  clearInterval(state.elapsedTimer);
  state.elapsedTimer = null;
  if (!live) return;

  const participants = live.speakers.filter((speaker) => !speaker.isBot);
  $("tray-live-title").textContent = live.meta.displayTitle
    || participants.slice(0, 2).map((speaker) => speaker.displayName).join(", ")
    || live.meta.channelName;
  $("tray-live-detail").textContent = t(
    participants.length === 1 ? "{count} participante" : "{count} participantes",
    { count: participants.length },
  );

  const tick = () => ($("tray-elapsed").textContent = elapsed(live.meta.startedAt));
  tick();
  state.elapsedTimer = setInterval(tick, 1000);
}

function renderMeetings(metas) {
  const recent = metas.slice(0, 3);
  if (recent.length === 0) {
    const empty = document.createElement("p");
    empty.className = "tray-empty";
    empty.textContent = t("Todavía no hay reuniones guardadas.");
    $("tray-meetings").replaceChildren(empty);
    return;
  }

  $("tray-meetings").replaceChildren(...recent.map((meta) => {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "tray-row";

    const copy = document.createElement("span");
    copy.className = "tray-row-copy";
    const title = document.createElement("strong");
    title.textContent = meta.displayTitle?.trim() || `${meta.guildName} · ${meta.channelName}`;
    const detail = document.createElement("small");
    detail.textContent = shortDate(meta.startedAt);
    copy.append(title, detail);

    const go = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    go.setAttribute("class", "icon tray-row-go");
    go.setAttribute("aria-hidden", "true");
    const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", "#i-chevron-right");
    go.append(use);

    row.append(copy, go);
    row.addEventListener("click", () => open(`meeting=${meta.id}`));
    return row;
  }));
}

function renderTasks(tasks) {
  const pending = tasks.filter((item) => !item.task.done);
  $("tray-task-count").textContent = String(pending.length);
  $("tray-task-count").hidden = pending.length === 0;
  $("tray-task-summary").textContent = pending.length === 0
    ? t("Sin tareas pendientes")
    : pending[0].task.text;
}

async function open(destination) {
  await invoke("open_main_window", { destination });
}

async function refresh() {
  const [snapshot, config, metas] = await Promise.all([
    invoke("get_snapshot"),
    invoke("get_config"),
    invoke("list_meetings").catch(() => []),
  ]);
  state.status = snapshot.status;
  state.config = config;
  state.live = snapshot.currentMeetings?.at(-1) ?? snapshot.currentMeeting ?? null;
  setLanguagePreference(config.application?.language ?? "auto", { notify: false });
  localizeStaticDocument();

  renderStatus();
  renderLive();
  renderMeetings(metas);
  invoke("list_tasks").then(renderTasks).catch(() => {});
}

function wireUp() {
  $("tray-open").addEventListener("click", () => open("home"));
  $("tray-open-tasks").addEventListener("click", () => open("tasks"));
  $("tray-settings").addEventListener("click", () => open("settings"));
  $("tray-open-live").addEventListener("click", () => {
    if (state.live) open(`meeting=${state.live.meta.id}`);
  });
  $("tray-quit").addEventListener("click", () => invoke("quit_app"));
  $("tray-follow").addEventListener("change", async (event) => {
    const config = state.config;
    if (!config) return;
    const wanted = event.currentTarget.checked;
    config.discord["follow-automatically"] = wanted;
    renderStatus();
    try {
      await invoke("set_config", { config });
    } catch {
      config.discord["follow-automatically"] = !wanted;
      renderStatus();
    }
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") invoke("close_tray_panel");
  });
}

async function boot() {
  wireUp();
  // The panel is long-lived but rarely visible: refresh whenever it is shown,
  // and stay in sync while it is.
  await listen("kuali://panel-shown", refresh);
  await listen("kuali://event", refresh);
  await listen("kuali://config-changed", refresh);
  await refresh();
}

boot().catch(() => {});
