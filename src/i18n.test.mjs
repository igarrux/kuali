import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  currentLanguage,
  detectLanguage,
  resolveLanguage,
  setLanguagePreference,
  t,
} from "./i18n.js";

test("Spanish and all its regional variants resolve to Spanish", () => {
  for (const locale of ["es", "es-ES", "es-MX", "es-419", "es_CO"]) {
    assert.equal(detectLanguage([locale]), "es");
  }
});

test("non-Spanish primary locales use English", () => {
  assert.equal(detectLanguage(["en-US"]), "en");
  assert.equal(detectLanguage(["fr-FR", "es-ES"]), "en");
  assert.equal(detectLanguage([]), "en");
});

test("an explicit preference overrides automatic detection", () => {
  assert.equal(resolveLanguage("en", ["es-MX"]), "en");
  assert.equal(resolveLanguage("es", ["en-US"]), "es");
  assert.equal(resolveLanguage("auto", ["es-CO"]), "es");
});

test("runtime translations interpolate variables", () => {
  setLanguagePreference("en", { notify: false });
  assert.equal(currentLanguage(), "en");
  assert.equal(t("Puerto {port} no disponible", { port: 9099 }), "Port 9099 is unavailable");
  assert.equal(
    t("Dejar Kuali como recién instalado"),
    "Reset Kuali as if newly installed",
  );
  setLanguagePreference("es", { notify: false });
  assert.equal(t("Puerto {port} no disponible", { port: 9099 }), "Puerto 9099 no disponible");
});

test("factory reset requires a typed confirmation dialog", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.match(html, /id="factory-reset-modal"/);
  assert.match(html, /id="factory-reset-input"/);
  assert.match(html, /id="btn-confirm-factory-reset" disabled/);
  assert.match(html, /Kuali conservará Silero, el motor de Whisper, la extensión y cualquier archivo ajeno\./);
});

test("summaries and tasks have an explicit privacy switch", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(html, /id="cfg-summarize"/);
  assert.match(html, /id="cfg-summarize"[^>]*checked/);
  assert.match(html, /ninguna transcripción se envía a un LLM/);
  assert.match(app, /btn-resummarize"\)\.hidden = live \|\| !summariesEnabled\(\)/);
});

test("saved meetings expose an accessible index status and retry action", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");

  assert.match(html, /id="meeting-index-status"[\s\S]*?role="status"/);
  assert.match(html, /aria-live="polite"/);
  assert.match(html, /aria-atomic="true"/);
  assert.match(html, /id="btn-reindex-meeting"/);
  assert.match(app, /invoke\("meeting_index_status", \{ id: meetingId \}\)/);
  assert.match(app, /invoke\("reindex_meeting", \{ id: meetingId \}\)/);
  assert.match(app, /case "meetingIndexChanged"/);
  assert.match(app, /case "meetingIndexChanged":\s+void refreshQuestionsStatus\(\)/);
  assert.match(app, /meetingIndexRequestIsCurrent\(meetingId, request\)/);
  assert.match(app, /button\.setAttribute\("aria-busy", "true"\)/);
  assert.match(app, /if \(!meeting \|\| isLiveMeeting\(meeting\.meta\.id\)\)/);
});

test("meeting index states are translated without technical passage counts", () => {
  setLanguagePreference("en", { notify: false });
  assert.equal(t("Indexada"), "Indexed");
  assert.equal(t("Indexación pendiente"), "Indexing pending");
  assert.equal(t("No indexada"), "Not indexed");
  assert.equal(t("Índice no disponible"), "Index unavailable");

  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const formatter = app.slice(
    app.indexOf("function meetingIndexMessage("),
    app.indexOf("function renderMeetingIndex("),
  );
  assert.doesNotMatch(formatter, /passages|pendingPassages|fragmentos?|passages?/);
  setLanguagePreference("es", { notify: false });
});

test("signed updates are checked at startup and deferred during active work", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../src-tauri/src/commands.rs", import.meta.url), "utf8");
  const tauriConfig = JSON.parse(
    readFileSync(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"),
  );
  assert.match(html, /id="cfg-automatic-updates"/);
  assert.match(html, /id="cfg-automatic-updates"[^>]*checked/);
  assert.match(html, /id="btn-check-update"/);
  assert.match(html, /id="btn-install-update"/);
  assert.match(html, /id="app-version"/);
  assert.match(app, /invoke\("app_version"\)/);
  assert.match(app, /UPDATE_CHECK_INTERVAL_MS = 6 \* 60 \* 60 \* 1000/);
  assert.match(app, /scheduleUpdateChecks\(\);\s+void checkForUpdates\(\);/);
  assert.doesNotMatch(app, /UPDATE_BOOT_DELAY_MS|updateBootTimer/);
  assert.match(
    app,
    /function maybeInstallUpdateAutomatically\(\)[\s\S]*?\["automatic-updates"\] === false/,
  );
  assert.doesNotMatch(
    app,
    /function scheduleUpdateChecks\(\)[\s\S]*?\["automatic-updates"\] === false/,
  );
  assert.match(app, /state\.liveMeetings\.size === 0/);
  assert.match(commands, /if !engine\.safe_for_update\(\)/);
  assert.match(commands, /while !engine\.safe_for_update\(\)/);
  assert.match(commands, /kuali:\/\/update-waiting/);
  assert.equal(tauriConfig.bundle.createUpdaterArtifacts, true);
  assert.match(tauriConfig.plugins.updater.pubkey, /^[A-Za-z0-9+/=]+$/);
  assert.deepEqual(tauriConfig.plugins.updater.endpoints, [
    "https://github.com/igarrux/kuali/releases/latest/download/latest.json",
  ]);
});

test("connected Discord settings stay protected until the user chooses to edit", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(html, /id="btn-edit-discord"/);
  assert.match(html, /id="btn-cancel-edit-discord"/);
  assert.match(html, /id="btn-add-discord-server"/);
  assert.match(app, /state\.discordEditing = !\(/);
  assert.match(app, /\$\(id\)\.readOnly = locked/);
  assert.match(app, /function cancelDiscordSettingsEdit\(\)/);
  assert.match(app, /invoke\("open_discord_install"/);
});

test("browser onboarding prefers the verified store and keeps manual installation as a fallback", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../src-tauri/src/commands.rs", import.meta.url), "utf8");
  const storeId = "cgojkmdggflcggedmapamcmkelgaahhp";

  assert.equal((html.match(/data-browser-store=/g) ?? []).length, 4);
  assert.equal((html.match(/data-meet-guide-step=/g) ?? []).length, 3);
  assert.match(html, /aria-valuemax="3"/);
  assert.match(html, /<summary>Instalación manual<\/summary>/);
  assert.match(app, /invoke\("open_browser_extension_store", \{ browser \}\)/);
  assert.match(app, new RegExp(storeId));
  assert.match(commands, new RegExp(storeId));
});

test("browser guidance distinguishes stable and experimental platform support", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const supportNotice = "Google Meet tiene soporte estable. Microsoft Teams y Zoom tienen soporte experimental y parcial.";

  assert.equal((html.match(new RegExp(supportNotice, "g")) ?? []).length, 2);
  setLanguagePreference("en", { notify: false });
  assert.equal(
    t(supportNotice),
    "Google Meet has stable support. Microsoft Teams and Zoom have experimental, partial support.",
  );
  setLanguagePreference("es", { notify: false });
});

test("a persistent model notice handles downloads and gates initial setup", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const main = readFileSync(new URL("../src-tauri/src/main.rs", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../src-tauri/src/commands.rs", import.meta.url), "utf8");
  const engine = readFileSync(
    new URL("../crates/kuali-engine/src/engine.rs", import.meta.url),
    "utf8",
  );
  const tauriConfig = JSON.parse(
    readFileSync(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"),
  );

  assert.match(html, /id="model-required"/);
  assert.match(html, /id="required-model-select"/);
  assert.match(html, /id="btn-required-model"/);
  assert.match(html, /id="required-model-progress-bar"/);
  assert.ok(html.indexOf('id="model-required"') < html.indexOf('id="pane-setup"'));
  assert.match(app, /panel\.hidden = !missingWeights && !downloading/);
  assert.match(app, /selector\.hidden = downloading/);
  assert.match(html, /id="required-model-current"/);
  assert.match(app, /Kuali sigue capturando el audio de la llamada/);
  assert.match(app, /La descarga continúa aunque cambies de sección dentro de Kuali/);
  assert.match(app, /if \(!model\?\.downloaded\) \{/);
  assert.match(app, /await invoke\("download_model", \{ model: model\.id \}\)/);
  assert.match(app, /state\.modelState\.model/);
  assert.match(app, /invoke\("cancel_model_download"\)/);
  assert.match(app, /Tus modelos instalados siguen disponibles/);
  assert.equal((app.match(/localStorage\.setItem\("kuali\.onboarding\.completed"/g) ?? []).length, 1);
  assert.doesNotMatch(main, /download_configured_model_if_missing/);
  assert.match(main, /commands::cancel_model_download/);
  assert.match(commands, /pub fn cancel_model_download/);
  assert.match(engine, /if download_configured_model \{/);
  assert.doesNotMatch(JSON.stringify(tauriConfig.bundle.resources), /ggml-[^"]+\.bin/i);
});

test("the curated model catalog exposes four clear performance tiers", () => {
  setLanguagePreference("en", { notify: false });
  assert.deepEqual(
    ["Ligero", "Equilibrado", "Preciso", "Máxima precisión"].map((name) => t(name)),
    ["Light", "Balanced", "Precise", "Highest accuracy"],
  );
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../src-tauri/src/commands.rs", import.meta.url), "utf8");
  assert.match(app, /model\.selectable !== false/);
  assert.match(app, /selectableModels\.map/);
  assert.match(html, /aria-haspopup="listbox"/);
  assert.match(app, /role", "option"/);
  assert.match(app, /model\.estimatedRamBytes/);
  assert.match(app, /return selectableWhisperModels\(\)\.some\(\(model\) => model\.downloaded\)/);
  assert.match(commands, /selectable: model\.is_selectable\(\)/);
  assert.match(commands, /estimated_ram_bytes: model\.estimated_ram_bytes\(\)/);
  setLanguagePreference("es", { notify: false });
});

test("a corrupt model is recovered only after Whisper rejects it", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const engine = readFileSync(
    new URL("../crates/kuali-engine/src/engine.rs", import.meta.url),
    "utf8",
  );
  const model = readFileSync(
    new URL("../crates/kuali-stt/src/model.rs", import.meta.url),
    "utf8",
  );

  assert.match(engine, /remove_if_corrupt/);
  assert.match(engine, /ModelRecoveryStarted/);
  assert.match(engine, /load_model_without_gpu/);
  assert.match(model, /same-size corruption after download/);
  assert.match(app, /case "modelRecoveryStarted"/);

  setLanguagePreference("en", { notify: false });
  assert.equal(
    t("El archivo de {model} estaba dañado. Kuali descargará una copia limpia y conservará el audio de la llamada.", {
      model: "Large v3",
    }),
    "The Large v3 file was corrupted. Kuali will download a clean copy and preserve the call audio.",
  );
  setLanguagePreference("es", { notify: false });
});

test("webhooks use Standard Webhooks without the legacy Kuali protocol", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const implementation = readFileSync(
    new URL("../crates/kuali-engine/src/webhooks.rs", import.meta.url),
    "utf8",
  );
  const documentation = ["../README.md", "../README.es.md"]
    .map((path) => readFileSync(new URL(path, import.meta.url), "utf8"))
    .join("\n");

  for (const source of [html, implementation, documentation]) {
    assert.doesNotMatch(source, /X-Kuali-(?:Event|Delivery|Timestamp|Attempt|Signature)/i);
    assert.doesNotMatch(source, /sha256=/i);
  }
  assert.match(html, /webhook-signature: v1,…/);
  assert.match(app, /return `whsec_\$\{btoa\(binary\)\}`/);
  assert.match(implementation, /\.header\("webhook-id", &delivery\.id\)/);
  assert.match(implementation, /\.header\("webhook-timestamp", &timestamp\)/);
  assert.match(implementation, /"webhook-signature"/);
  assert.match(implementation, /mac\.update\(message_id\.as_bytes\(\)\)/);
  assert.match(documentation, /Standard Webhooks 1\.0/);
});

test("visible placeholders do not contain a contributor's personal examples", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.doesNotMatch(html, /@garrux|WaitingRoom/);
  assert.match(html, /placeholder="@tu_usuario"/);
  assert.match(html, /placeholder="Ej\.: nombre del proyecto…"/);
});

test("English-only terminology hints are shown only in the Spanish interface", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");
  assert.equal((html.match(/data-language-only="es"/g) ?? []).length, 1);
  assert.match(styles, /html\[lang="en"\] \[data-language-only="es"\] \{ display: none; \}/);
  assert.doesNotMatch(
    html,
    /data-language-only="es"[^>]*>[^<]*<span[^>]*>✓<\/span>/,
  );
});

test("the simplified Discord guide includes the three necessary examples", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const files = [
    "discord-02-new-application.png",
    "discord-03-reset-token.png",
    "discord-copy-username.png",
  ];
  for (const file of files) {
    assert.match(html, new RegExp(`data-guide-src-es="assets/guides/discord/${file}"`));
    assert.match(html, new RegExp(`data-guide-src-en="assets/guides/discord/en-${file}"`));
  }
  assert.equal((html.match(/<summary>Ver ejemplo<\/summary>/g) ?? []).length, files.length);
  assert.equal((html.match(/class="guide-image-open"/g) ?? []).length, files.length);
  assert.match(html, /pulsa el icono de copiar junto a tu usuario/);
  assert.doesNotMatch(html, /Copiar ID del usuario|Copy User ID/);
});

test("Discord onboarding opens an exact authorization flow in three steps", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../src-tauri/src/commands.rs", import.meta.url), "utf8");
  const installation = readFileSync(
    new URL("../crates/kuali-discord/src/installation.rs", import.meta.url),
    "utf8",
  );

  assert.equal((html.match(/data-discord-guide-step=/g) ?? []).length, 3);
  assert.match(html, /id="discord-guide-progress"[\s\S]*?aria-valuemax="3"/);
  assert.match(html, /id="btn-open-discord-install"/);
  assert.doesNotMatch(html, /Configura el enlace y los ámbitos|Elige los permisos mínimos/);
  assert.match(app, /invoke\("open_discord_install", \{ botToken: token \}\)/);
  assert.match(commands, /async fn open_discord_install/);
  assert.match(installation, /oauth2\/applications\/@me/);
});

test("Discord authorization actions disappear after the bot connects", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");

  assert.match(app, /\$\("btn-open-discord-install"\)\.hidden = discordReady/);
  assert.match(app, /\$\("btn-save-discord-guide"\)\.hidden = discordReady/);
  assert.match(app, /Discord está conectado\. Ya puedes terminar la guía\./);
  assert.match(html, /Adjuntar archivos/);
  assert.match(html, /Insertar enlaces/);
  assert.match(html, /Publicar tareas y accesos al resumen/);
  assert.match(app, /classList\.toggle\("guide-success-note", discordReady\)/);
});

test("answers are rendered as markdown without ever becoming markup", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const renderer = app.slice(
    app.indexOf("function renderMarkdown("),
    app.indexOf("function resizeAskField("),
  );

  // The answer is written by a model that just read meeting transcripts, and a
  // transcript is untrusted: anyone in a call can say anything. Assigning it as
  // HTML would turn a spoken sentence into markup.
  assert.doesNotMatch(renderer, /innerHTML|outerHTML|insertAdjacentHTML/);
  // Text always arrives through textContent or a text node.
  assert.match(renderer, /createTextNode/);
  assert.match(renderer, /textContent = /);
  // The subset the prompt asks for is the subset that is rendered.
  for (const tag of ["ul", "ol", "li", "strong", "em", "code", "p"]) {
    assert.match(renderer, new RegExp(`"${tag}"`), `missing support for <${tag}>`);
  }
});

test("configuration keys use the kebab-case the engine actually reads", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  // These config sections serialize with `rename_all = "kebab-case"` in Rust,
  // and serde drops an unknown field in silence instead of rejecting it. A
  // camelCase key therefore fails invisibly: the setting simply never takes
  // effect, with no error anywhere to explain why.
  const sections = "llm|discord|whisper|application|meet|integrations";
  const camelCase = [
    ...app.matchAll(new RegExp(`\\b(?:config|c|cfg)\\.(?:${sections})\\.([a-z]+[A-Z]\\w*)`, "g")),
  ].map((match) => match[1]);

  assert.deepEqual(camelCase, [], `use kebab-case and bracket access instead`);
  // The switch that gates questions is the one this test was written for.
  assert.match(app, /config\.llm\["meeting-questions"\] = true/);
});

test("finishing setup resets both guides and recommends questions once", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const closeGuide = app.slice(
    app.indexOf("async function closeCompletedGuide("),
    app.indexOf("async function finishInitialSetup()"),
  );
  const finishSetup = app.slice(
    app.indexOf("async function finishInitialSetup()"),
    app.indexOf("function renderDiscordGuideStep"),
  );

  assert.match(closeGuide, /state\.discordGuideStep = 0/);
  assert.match(closeGuide, /state\.meetGuideStep = 0/);
  // Home stays the default landing place; setup passes a destination only to
  // avoid landing home and immediately jumping elsewhere.
  assert.match(closeGuide, /destination = goHome/);
  assert.match(closeGuide, /await destination\(\)/);

  // Both paths still leave through the same door.
  assert.equal((finishSetup.match(/closeCompletedGuide\(/g) ?? []).length, 2);
  // The offer is shown, never accepted on the user's behalf.
  assert.match(finishSetup, /questions\.enabled \? showAsk : goHome/);
  assert.doesNotMatch(finishSetup, /prepare_questions/);
});

test("no element id is claimed twice", () => {
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const seen = new Map();
  const repeated = [];
  for (const [, id] of markup.matchAll(/\sid="([^"]+)"/g)) {
    if (seen.has(id)) repeated.push(id);
    seen.set(id, true);
  }

  // `$` resolves an id to whichever element comes first in the document, so a
  // second element claiming a taken id silently steals every listener and every
  // `disabled` toggle written for the first one.
  assert.deepEqual([...new Set(repeated)], []);
});

test("asking is reachable from the top bar, not the sidebar", () => {
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const topbar = markup.slice(
    markup.indexOf('<div class="topbar-actions">'),
    markup.indexOf("</header>"),
  );
  const sidebarNav = markup.slice(
    markup.indexOf('<nav class="app-nav"'),
    markup.indexOf("</nav>"),
  );

  assert.match(topbar, /id="nav-ask"/);
  assert.match(topbar, /#i-sparkles/);
  assert.doesNotMatch(sidebarNav, /nav-ask/);
});

test("Ask keeps a bounded conversation context and offers an accessible reset", () => {
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const reset = markup.slice(
    markup.indexOf('id="btn-new-ask-conversation"') - 160,
    markup.indexOf('id="btn-new-ask-conversation"') + 320,
  );

  assert.match(reset, /<button/);
  assert.match(reset, /aria-controls="ask-thread"/);
  assert.match(reset, /\shidden/);
  assert.match(reset, /#i-refresh/);
  assert.match(reset, /Nueva conversación/);
  assert.match(app, /askHistory: \[\]/);
  assert.match(app, /const ASK_HISTORY_LIMIT = 6/);
  assert.match(app, /invoke\("ask_meetings", \{\s*question,\s*history: askHistoryPayload\(\)/);
  assert.match(app, /state\.askHistory\.slice\(-ASK_HISTORY_LIMIT\)/);
  assert.match(app, /new Set\([\s\S]*?citation\?\.meetingId/);
  assert.match(app, /state\.askHistory\.splice\(0, state\.askHistory\.length - ASK_HISTORY_LIMIT\)/);

  const resetHandler = app.slice(
    app.indexOf("function resetAskConversation("),
    app.indexOf("function appendAskTurn("),
  );
  assert.match(resetHandler, /state\.askHistory = \[\]/);
  assert.match(resetHandler, /\$\("ask-thread"\)\.replaceChildren\(\)/);
  assert.match(resetHandler, /\$\("ask-suggestions"\)\.hidden = !state\.questions\?\.ready/);
  assert.match(resetHandler, /field\.focus\(\)/);
  assert.match(app, /btn-new-ask-conversation"\)\.addEventListener\("click", resetAskConversation\)/);

  const submit = app.indexOf("async function submitQuestion");
  const failure = app.slice(
    app.indexOf("} catch (error) {", submit),
    app.indexOf("} finally {", submit),
  );
  assert.doesNotMatch(failure, /rememberAskTurn/);
});

test("the new Ask conversation control is translated", () => {
  setLanguagePreference("en", { notify: false });
  assert.equal(t("Nueva conversación"), "New conversation");
  setLanguagePreference("es", { notify: false });
});

test("summary completion reloads automatic tags and folder only for the open meeting", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const refresh = app.slice(
    app.indexOf("async function refreshMeetingAfterSummary("),
    app.indexOf("function renderLibraryGrouping("),
  );
  const event = app.slice(
    app.indexOf('case "summaryReady"'),
    app.indexOf('case "meetingIndexChanged"'),
  );

  assert.match(
    refresh,
    /await Promise\.all\(\[refreshMeetings\(\), refreshFolders\(\), refreshTagCatalog\(\)\]\)/,
  );
  assert.equal((refresh.match(/state\.viewing !== opened/g) ?? []).length, 2);
  assert.equal((refresh.match(/meetingMetadataRevision\(meetingId\) !== metadataRevision/g) ?? []).length, 2);
  assert.equal((refresh.match(/meetingMetadataEditActive\(meetingId\)/g) ?? []).length, 2);
  assert.match(refresh, /saved = await invoke\("load_meeting", \{ id: meetingId \}\)/);
  assert.match(refresh, /state\.viewing = saved;\s*renderMeeting\(\)/);
  assert.match(event, /meeting\.summary = event\.summary/);
  assert.match(event, /void refreshMeetingAfterSummary\(event\.meetingId\)/);

  assert.match(app, /beginMeetingMetadataEdit\(ids\)[\s\S]*?set_meeting_folder/);
  assert.match(app, /beginMeetingMetadataEdit\(ids\)[\s\S]*?set_meeting_tags/);
  assert.match(app, /finally \{\s*finishMeetingMetadataEdit\(ids\)/);
});

test("Ask readiness fails closed and explains index health without a false setup cause", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const refresh = app.slice(
    app.indexOf("async function refreshQuestionsStatus("),
    app.indexOf("function renderQuestionGate("),
  );
  const gate = app.slice(
    app.indexOf("function renderQuestionGate("),
    app.indexOf("function humanDuration("),
  );
  const events = app.slice(
    app.indexOf("function handleEvent("),
    app.indexOf("function upsertUtterance("),
  );

  for (const condition of [
    "status.ready === true",
    "status.enabled === true",
    "status.modelReady === true",
    "status.indexAvailable === true",
    "status.indexCurrent === true",
    "status.pendingPassages === 0",
    "status.updating === false",
  ]) {
    assert.match(refresh, new RegExp(condition.replaceAll(".", "\\.")));
  }
  assert.match(refresh, /request !== state\.questionStatusRequest/);
  assert.match(gate, /gate\.setAttribute\("aria-busy", String\(busy\)\)/);
  assert.match(gate, /title\.setAttribute\("aria-live", "polite"\)/);
  assert.match(gate, /if \(status\.indexAvailable !== true\)/);
  assert.match(gate, /if \(status\.indexCurrent !== true\)/);
  assert.match(gate, /action\.hidden = true;\s*action\.disabled = true/);
  assert.match(gate, /Hay reuniones sin indexar/);
  assert.match(gate, /Actualizando la memoria de reuniones/);
  assert.match(events, /case "questionsStatusChanged":\s+void refreshQuestionsStatus\(\)/);
});

test("question index health messages are translated", () => {
  setLanguagePreference("en", { notify: false });
  assert.equal(t("Actualizando la memoria de reuniones"), "Updating meeting memory");
  assert.equal(t("El índice de reuniones no está disponible"), "The meeting index is unavailable");
  assert.equal(t("Hay reuniones sin indexar"), "Some meetings are not indexed");
  assert.equal(t("Terminar indexación"), "Finish indexing");
  setLanguagePreference("es", { notify: false });
});
