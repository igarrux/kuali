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

test("signed updates are checked at startup and deferred during active work", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
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
  assert.equal(tauriConfig.bundle.createUpdaterArtifacts, true);
  assert.match(tauriConfig.plugins.updater.pubkey, /^[A-Za-z0-9+/=]+$/);
  assert.deepEqual(tauriConfig.plugins.updater.endpoints, [
    "https://github.com/igarrux/kuali/releases/latest/download/latest.json",
  ]);
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
  assert.match(app, /\$\("model-required"\)\.hidden = !missingWeights && !downloading/);
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

test("the curated model catalog keeps the three quality tiers distinct", () => {
  setLanguagePreference("en", { notify: false });
  assert.equal(
    t("Large v3 — máxima precisión, más lento y mayor uso de memoria"),
    "Large v3 — highest accuracy, slower and higher memory use",
  );
  assert.equal(
    t("Large v3 Q5 — mayor precisión, más memoria"),
    "Large v3 Q5 — higher accuracy, higher memory use",
  );
  assert.equal(
    t("Large v3 Turbo Q5 — recomendado: rápido y eficiente"),
    "Large v3 Turbo Q5 — recommended: fast and efficient",
  );
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../src-tauri/src/commands.rs", import.meta.url), "utf8");
  assert.match(app, /model\.selectable !== false/);
  assert.match(app, /selectableModels\.map/);
  assert.match(app, /return selectableWhisperModels\(\)\.some\(\(model\) => model\.downloaded\)/);
  assert.match(commands, /selectable: model\.is_selectable\(\)/);
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

test("finishing setup resets both guides before returning home", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const closeGuide = app.slice(
    app.indexOf("async function closeCompletedGuide()"),
    app.indexOf("async function finishInitialSetup()"),
  );
  const finishSetup = app.slice(
    app.indexOf("async function finishInitialSetup()"),
    app.indexOf("function renderDiscordGuideStep"),
  );

  assert.match(closeGuide, /state\.discordGuideStep = 0/);
  assert.match(closeGuide, /state\.meetGuideStep = 0/);
  assert.match(closeGuide, /await goHome\(\)/);
  assert.equal((finishSetup.match(/return closeCompletedGuide\(\)/g) ?? []).length, 2);
});
