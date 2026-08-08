#!/usr/bin/env node

import { createWriteStream } from "node:fs";
import {
  access,
  chmod,
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve, sep } from "node:path";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { once } from "node:events";
import { spawn, spawnSync } from "node:child_process";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const extensionDir = resolve(scriptDir, "..");
const workspaceRoot = resolve(extensionDir, "..");
const targetDir = join(workspaceRoot, "target", "e2e");

function parseArgs(argv) {
  const options = {
    mode: "full",
    timeoutSeconds: 900,
    browser: process.env.KUALI_E2E_BROWSER || null,
    help: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--solo") options.mode = "solo";
    else if (arg === "--full") options.mode = "full";
    else if (arg === "--timeout-seconds") options.timeoutSeconds = Number(argv[++index]);
    else if (arg === "--browser") options.browser = argv[++index];
    else if (arg === "--help" || arg === "-h") options.help = true;
    else throw new Error(`Argumento desconocido: ${arg}`);
  }
  if (!Number.isInteger(options.timeoutSeconds) || options.timeoutSeconds < 30) {
    throw new Error("--timeout-seconds debe ser un entero de al menos 30");
  }
  return options;
}

function usage() {
  console.log(`Uso: npm run test:e2e:meet -- [--full|--solo] [opciones]

Opciones:
  --full                 Exige micrófono local + segundo participante (predeterminado)
  --solo                 Prueba solo el micrófono local; no valida separación remota/foto
  --timeout-seconds N    Tiempo máximo para operar Meet (predeterminado: 900)
  --browser RUTA         Chromium o Chrome for Testing ya instalado

El navegador de prueba y su perfil viven fuera de tu perfil normal. La primera
ejecución descarga Chrome for Testing dentro de target/e2e.`);
}

async function exists(path) {
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

function extract(zipPath, destination) {
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

async function ensureBrowser(explicitPath) {
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

async function patchedExtension(port, temporaryRoot) {
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

async function terminate(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  child.kill("SIGTERM");
  await Promise.race([once(child, "exit"), new Promise((resolve) => setTimeout(resolve, 3000))]);
  if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) return usage();
  await mkdir(targetDir, { recursive: true });

  const browserPromise = ensureBrowser(options.browser);
  // The Rust build may take longer than the catalog request. Attaching a
  // handler now prevents Node from treating an early download failure as an
  // unhandled rejection; `launch` still awaits and reports the same error.
  browserPromise.catch(() => {});
  const rustArgs = [
    "run", "-p", "kuali-engine", "--example", "meet_live_e2e", "--",
    options.mode === "solo" ? "--solo" : "--full",
    "--timeout-seconds", String(options.timeoutSeconds),
  ];
  const runner = spawn("cargo", rustArgs, {
    cwd: workspaceRoot,
    stdio: ["inherit", "pipe", "pipe"],
  });
  runner.stderr.pipe(process.stderr);

  let browser = null;
  let temporaryRoot = null;
  let stdoutBuffer = "";
  let launchStarted = false;
  let launchError = null;

  const launch = async (ready) => {
    if (launchStarted) return;
    launchStarted = true;
    const executable = await browserPromise;
    temporaryRoot = await mkdtemp(join(tmpdir(), "kuali-meet-e2e-"));
    const extension = await patchedExtension(ready.port, temporaryRoot);
    const profile = join(temporaryRoot, "chrome-profile");
    await mkdir(profile, { recursive: true });
    browser = spawn(executable, [
      `--user-data-dir=${profile}`,
      "--remote-debugging-port=0",
      `--disable-extensions-except=${extension}`,
      `--load-extension=${extension}`,
      "--no-first-run",
      "--no-default-browser-check",
      "--new-window",
      "https://meet.google.com/",
    ], { stdio: "ignore" });
    browser.once("error", (error) => {
      launchError = error;
      runner.kill("SIGINT");
    });

    console.log(`\nChrome for Testing abrió con Kuali E2E y el puerto ${ready.port} ya configurado.`);
    console.log("1. Entra a una reunión de Meet y permite el micrófono.");
    if (ready.mode === "full") {
      console.log("2. Une un segundo participante (otro perfil o tu teléfono) y deja visible su ficha.");
      console.log("3. En la pestaña de Meet: Extensiones → Kuali E2E → Capturar esta reunión.");
      console.log("4. Lee la frase local; después lee la frase remota desde el segundo dispositivo.");
    } else {
      console.log("2. En la pestaña de Meet: Extensiones → Kuali E2E → Capturar esta reunión.");
      console.log("3. Lee la frase local completa.");
    }
    console.log("5. Espera a ver texto [en vivo], y pulsa Detener captura.\n");
  };

  runner.stdout.on("data", (chunk) => {
    const text = chunk.toString();
    process.stdout.write(text);
    stdoutBuffer += text;
    let newline;
    while ((newline = stdoutBuffer.indexOf("\n")) !== -1) {
      const line = stdoutBuffer.slice(0, newline);
      stdoutBuffer = stdoutBuffer.slice(newline + 1);
      if (line.startsWith("KUALI_E2E_READY ")) {
        const ready = JSON.parse(line.slice("KUALI_E2E_READY ".length));
        launch(ready).catch((error) => {
          launchError = error;
          console.error(`No pude abrir el navegador E2E: ${error.message}`);
          runner.kill("SIGINT");
        });
      }
    }
  });

  const onInterrupt = () => runner.kill("SIGINT");
  process.once("SIGINT", onInterrupt);
  const [code, signal] = await once(runner, "exit");
  process.removeListener("SIGINT", onInterrupt);
  await terminate(browser);
  if (temporaryRoot?.startsWith(`${tmpdir()}${sep}`)) {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
  if (launchError) throw launchError;
  if (signal) process.exitCode = 1;
  else process.exitCode = code ?? 1;
}

main().catch((error) => {
  console.error(error.stack || error.message || String(error));
  process.exitCode = 1;
});
