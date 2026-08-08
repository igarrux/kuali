import { createHash } from "node:crypto";
import {
  copyFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifest = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
const packageJson = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));

if (manifest.version !== packageJson.version) {
  throw new Error(`Version mismatch: manifest ${manifest.version}, package ${packageJson.version}`);
}
if (manifest.manifest_version !== 3) throw new Error("Chrome Web Store builds must use Manifest V3");
if (!manifest.default_locale) throw new Error("The store build must declare a default locale");

const allowedPermissions = new Set(["storage", "tabCapture", "offscreen"]);
for (const permission of manifest.permissions || []) {
  if (!allowedPermissions.has(permission)) throw new Error(`Undocumented permission: ${permission}`);
}
for (const host of manifest.host_permissions || []) {
  if (host === "<all_urls>" || host.includes("localhost") || host.startsWith("http://")) {
    throw new Error(`Unsafe host permission: ${host}`);
  }
}

const files = [
  "LICENSE",
  "NOTICE",
  "manifest.json",
  "kuali-logo.svg",
  "offscreen.html",
  "popup.css",
  "popup.html",
  "popup.js",
  "privacy.css",
  "privacy.html",
  "privacy.js",
  "_locales/en/messages.json",
  "_locales/es_419/messages.json",
  "icons/icon16.png",
  "icons/icon32.png",
  "icons/icon48.png",
  "icons/icon128.png",
  "src/background.js",
  "src/capture-policy.js",
  "src/content.js",
  "src/health.js",
  "src/lifecycle.js",
  "src/meet-protocol.js",
  "src/offscreen.js",
  "src/page-capture.js",
  "src/pcm-worklet.js",
  "src/protocol.js",
].sort();

for (const relative of files) {
  const source = join(root, relative);
  if (!statSync(source).isFile()) throw new Error(`Missing package file: ${relative}`);
}

const stage = mkdtempSync(join(tmpdir(), "kuali-store-"));
const fixedTime = new Date("2026-01-01T00:00:00.000Z");
const outDir = join(root, "dist");
const archive = join(outDir, `kuali-chrome-${manifest.version}.zip`);

try {
  for (const relative of files) {
    const destination = join(stage, relative);
    mkdirSync(dirname(destination), { recursive: true });
    copyFileSync(join(root, relative), destination);
    utimesSync(destination, fixedTime, fixedTime);
  }

  mkdirSync(outDir, { recursive: true });
  rmSync(archive, { force: true });
  const zipped = spawnSync("zip", ["-X", "-q", archive, ...files], {
    cwd: stage,
    encoding: "utf8",
  });
  if (zipped.status !== 0) {
    throw new Error(zipped.stderr || zipped.stdout || "zip failed");
  }

  const digest = createHash("sha256").update(readFileSync(archive)).digest("hex");
  writeFileSync(`${archive}.sha256`, `${digest}  ${archive.split("/").at(-1)}\n`);
  console.log(`Created ${archive}`);
  console.log(`SHA-256 ${digest}`);
} finally {
  rmSync(stage, { recursive: true, force: true });
}
