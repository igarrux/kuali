// Shared by the real-browser E2E runners: Chrome for Testing download, the
// patched extension copy and child-process shutdown.

import { createWriteStream } from "node:fs";
import {
  access,
  chmod,
  cp,
  mkdir,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { basename, dirname, join, resolve, sep } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { once } from "node:events";
import { spawnSync } from "node:child_process";

export const scriptDir = dirname(fileURLToPath(import.meta.url));
export const extensionDir = resolve(scriptDir, "..");
export const workspaceRoot = resolve(extensionDir, "..");
export const targetDir = join(workspaceRoot, "target", "e2e");

export async function exists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

function chromePlatform() {
  if (process.platform === "darwin") return process.arch === "arm64" ? "mac-arm64" : "mac-x64";
  if (process.platform === "linux" && process.arch === "x64") return "linux64";
  if (process.platform === "win32") return process.arch === "ia32" ? "win32" : "win64";
  throw new Error(`Chrome for Testing no publica un binario para ${process.platform}/${process.arch}`);
}

function executableInside(directory, platform) {
  if (platform.startsWith("mac-")) {
    return join(
      directory,
      `chrome-${platform}`,
      "Google Chrome for Testing.app",
      "Contents",
      "MacOS",
      "Google Chrome for Testing",
    );
  }
  if (platform === "linux64") return join(directory, "chrome-linux64", "chrome");
  return join(directory, `chrome-${platform}`, "chrome.exe");
}

async function download(url, destination) {
  const response = await fetch(url);
  if (!response.ok || !response.body) {
    throw new Error(`Chrome for Testing devolvió HTTP ${response.status}`);
  }
  await pipeline(Readable.fromWeb(response.body), createWriteStream(destination));
}

export function extract(zipPath, destination) {
  let command;
  let args;
  if (process.platform === "darwin") {
    command = "ditto";
    args = ["-x", "-k", zipPath, destination];
  } else if (process.platform === "win32") {
    command = "powershell.exe";
    args = ["-NoProfile", "-Command", "Expand-Archive", "-LiteralPath", zipPath, "-DestinationPath", destination, "-Force"];
  } else {
    command = "unzip";
    args = ["-q", zipPath, "-d", destination];
  }
  const result = spawnSync(command, args, { stdio: "inherit" });
  if (result.status !== 0) throw new Error(`No se pudo descomprimir Chrome for Testing (${result.status})`);
}

export async function ensureBrowser(explicitPath) {
  if (explicitPath) {
    const path = resolve(explicitPath);
    if (!(await exists(path))) throw new Error(`No existe el navegador indicado: ${path}`);
    return path;
  }

  const platform = chromePlatform();
  const catalogUrl = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
  const response = await fetch(catalogUrl);
  if (!response.ok) throw new Error(`No pude consultar Chrome for Testing: HTTP ${response.status}`);
  const catalog = await response.json();
  const stable = catalog.channels?.Stable;
  const artifact = stable?.downloads?.chrome?.find((item) => item.platform === platform);
  if (!stable?.version || !artifact?.url) {
    throw new Error(`El catálogo oficial no contiene Chrome para ${platform}`);
  }

  const installDir = join(targetDir, "chrome-for-testing", stable.version);
  const executable = executableInside(installDir, platform);
  if (await exists(executable)) return executable;

  await mkdir(installDir, { recursive: true });
  const zipPath = join(installDir, basename(new URL(artifact.url).pathname));
  console.log(`Descargando Chrome for Testing ${stable.version} para el E2E…`);
  try {
    await download(artifact.url, `${zipPath}.part`);
    await rm(zipPath, { force: true });
    await rename(`${zipPath}.part`, zipPath);
    extract(zipPath, installDir);
    await rm(zipPath, { force: true });
    if (process.platform !== "win32") await chmod(executable, 0o755);
  } catch (error) {
    await rm(`${zipPath}.part`, { force: true });
    throw error;
  }
  if (!(await exists(executable))) throw new Error(`No apareció el ejecutable esperado: ${executable}`);
  return executable;
}

export async function patchedExtension(port, temporaryRoot) {
  const destination = join(temporaryRoot, "extension");
  await cp(extensionDir, destination, {
    recursive: true,
    filter: (source) => !source.includes(`${sep}e2e${sep}`) && !source.endsWith(`${sep}e2e`),
  });

  const backgroundPath = join(destination, "src", "background.js");
  const popupPath = join(destination, "popup.js");
  const popupHtmlPath = join(destination, "popup.html");
  const manifestPath = join(destination, "manifest.json");
  await writeFile(
    backgroundPath,
    (await readFile(backgroundPath, "utf8")).replace("const DEFAULT_PORT = 9099;", `const DEFAULT_PORT = ${port};`),
  );
  await writeFile(
    popupPath,
    (await readFile(popupPath, "utf8")).replace("{ kualiPort: 9099 }", `{ kualiPort: ${port} }`),
  );
  await writeFile(
    popupHtmlPath,
    (await readFile(popupHtmlPath, "utf8")).replace('value="9099"', `value="${port}"`),
  );
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  manifest.name = "Kuali E2E";
  await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  return destination;
}

export async function terminate(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  child.kill("SIGTERM");
  await Promise.race([once(child, "exit"), new Promise((resolve) => setTimeout(resolve, 3000))]);
  if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
}
