#!/usr/bin/env node

import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, sep } from "node:path";
import { once } from "node:events";
import { spawn } from "node:child_process";
import {
  ensureBrowser,
  patchedExtension,
  targetDir,
  terminate,
  workspaceRoot,
} from "./chrome-for-testing.mjs";

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
