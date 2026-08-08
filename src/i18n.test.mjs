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
  assert.match(html, /ninguna transcripción se envía a un LLM/);
  assert.match(app, /btn-resummarize"\)\.hidden = live \|\| !summariesEnabled\(\)/);
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
  assert.equal((html.match(/data-language-only="es"/g) ?? []).length, 2);
  assert.match(styles, /html\[lang="en"\] \[data-language-only="es"\] \{ display: none; \}/);
  assert.doesNotMatch(
    html,
    /data-language-only="es"[^>]*>[^<]*<span[^>]*>✓<\/span>/,
  );
});

test("every Discord screenshot slot uses its documented filename", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const files = [
    "discord-02-new-application.png",
    "discord-03-reset-token.png",
    "discord-04-guild-install.png",
    "discord-05-install-link-scopes.png",
    "discord-06-bot-permissions.png",
  ];
  for (const file of files) {
    assert.match(html, new RegExp(`data-guide-src-es="assets/guides/discord/${file}"`));
    assert.match(html, new RegExp(`data-guide-src-en="assets/guides/discord/en-${file}"`));
  }
  assert.equal((html.match(/<summary>Ver ejemplo<\/summary>/g) ?? []).length, files.length);
  assert.equal((html.match(/class="guide-image-open"/g) ?? []).length, files.length);
  assert.doesNotMatch(html, /discord-07-invite-bot\.png/);
});
