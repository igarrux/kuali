import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
const readBytes = (path) => readFileSync(new URL(`../${path}`, import.meta.url));

test("the store manifest has narrow, documented access", () => {
  const manifest = JSON.parse(read("manifest.json"));
  assert.equal(manifest.manifest_version, 3);
  assert.equal(manifest.default_locale, "en");
  assert.deepEqual(manifest.permissions.sort(), ["offscreen", "storage", "tabCapture"]);
  assert(!manifest.host_permissions.includes("<all_urls>"));
  assert(!manifest.host_permissions.some((host) => host.includes("localhost")));
  assert(manifest.host_permissions.includes("ws://127.0.0.1/*"));
  assert.equal(manifest.homepage_url, "https://github.com/igarrux/kuali");
});

test("capture requires disclosure and affirmative confirmation", () => {
  const popup = read("popup.html");
  const content = read("src/content.js");
  assert.match(popup, /participant-consent/);
  assert.match(popup, /captureDisclosure/);
  assert.match(popup, /privacy\.html/);
  assert.match(content, /consentCheck\.checked/);
  assert.match(content, /recordingIndicator/);
  assert.match(content, /capture-stop/);
});

test("both store locales contain a name and description", () => {
  for (const locale of ["en", "es_419"]) {
    const messages = JSON.parse(read(`_locales/${locale}/messages.json`));
    assert(messages.appName.message);
    assert(messages.appDescription.message.length >= 40);
  }
});

test("the package contains license and Vexa attribution", () => {
  assert.match(read("LICENSE"), /Apache License/);
  assert.match(read("NOTICE"), /Vexa/i);
});

test("store artwork uses Chrome Web Store dimensions", () => {
  for (const [path, width, height] of [
    ["store/assets/promo-small.png", 440, 280],
    ["store/assets/promo-marquee.png", 1400, 560],
    ["store/assets/screenshot-consent.png", 1280, 800],
    ["store/assets/screenshot-local.png", 1280, 800],
  ]) {
    const png = readBytes(path);
    assert.equal(png.subarray(1, 4).toString(), "PNG", `${path} must be PNG`);
    assert.equal(png.readUInt32BE(16), width, `${path} width`);
    assert.equal(png.readUInt32BE(20), height, `${path} height`);
  }
});

test("localized store summaries fit the 132-character limit", () => {
  for (const [path, heading] of [
    ["store/listing.en.md", "## Summary"],
    ["store/listing.es-419.md", "## Resumen"],
  ]) {
    const summary = read(path).split(heading, 2)[1].split("##", 1)[0].trim();
    assert(summary.length > 0 && summary.length <= 132, `${path}: ${summary.length}`);
  }
});
