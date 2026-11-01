#!/usr/bin/env node
// Fully automated Yandex Telemost E2E: three real browsers join one meeting,
// the recorder starts Kuali from the in-page suggestion, and the Rust probe
// (meet_live_e2e) checks the wire and the Russian transcript.

import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";
import { once } from "node:events";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  ensureBrowser,
  exists,
  patchedExtension,
  targetDir,
  terminate,
  workspaceRoot,
} from "./chrome-for-testing.mjs";
import {
  CdpClient,
  FrameContexts,
  callFunction,
  clickNode,
  evaluate,
  findElement,
  readDevToolsActivePort,
  releaseObject,
  shadowQuery,
  waitFor,
} from "./cdp.mjs";

const ROLES = ["a", "b", "recorder"];
const NAMES = { a: "Анна Тестовая", b: "Борис Тестовый", recorder: "Kuali Recorder" };
// Exact phrases for generated audio; the probe's expected terms depend on them.
// No term opens its phrase: Telemost's server VAD flags speech ~0.35 s late.
export const PHRASES = {
  a: { text: "Давайте обсудим бюджет на следующий квартал и расходы на маркетинг.", term: "бюджет" },
  b: { text: "Напоминаю, релиз новой версии назначен на пятницу.", term: "релиз" },
  recorder: { text: "Я проверю настройки тестового сервера сегодня вечером.", term: "сервер" },
};
// Silence between speakers: server VAD turns off ~1.5 s after speech ends.
const SPEAKER_GAP_MS = 3_500;
const CALL_FRAME = /\/private-join\//;
const TELEMOST_ORIGINS = [
  "https://telemost.yandex.ru",
  "https://telemost.yandex.com",
  "https://telemost.360.yandex.ru",
  "https://telemost.360.yandex.com",
];
const testid = (id) => `[data-testid="${id}"]`;
const MIC_ON_LABEL = /^(Выключить микрофон|Turn off (the )?microphone)$/i;
const MIC_OFF_LABEL = /^(Включить микрофон|Turn on (the )?microphone)$/i;
// Ordered alternatives: stable data-testid first, localized text as fallback.
const UI = {
  continueInBrowser: [
    { selector: testid("meeting-continue-in-browser-continue") },
    { selector: "button, a, [role='button']", text: /^(Продолжить в браузере|Continue in (the )?browser)$/i },
  ],
  joinButton: [
    { selector: testid("enter-conference-button") },
    { selector: "button", text: /^(Подключиться|Join)$/i },
  ],
  nameInput: [
    { selector: `[class*="displayNameBlock_"] input${testid("orb-textinput-input")}` },
    { selector: "[class*='displayNameBlock_'] input" },
  ],
  leave: [
    { selector: testid("end-call-alt-button") },
    { selector: "button", label: /^(Выйти из встречи|Leave (the )?(meeting|call))$/i },
  ],
  leaveConfirm: [{ selector: "button", text: /^(Выйти|Покинуть встречу|Leave)$/i }],
  // The participants panel reuses turn-on-mic-button for both states, so
  // prefer the toolbar and always check the label outside it.
  micOn: [
    { selector: `${testid("call-toolbar")} ${testid("turn-off-mic-button")}` },
    { selector: testid("turn-off-mic-button"), label: MIC_ON_LABEL },
  ],
  micOff: [
    { selector: `${testid("call-toolbar")} ${testid("turn-on-mic-button")}` },
    { selector: testid("turn-on-mic-button"), label: MIC_OFF_LABEL },
  ],
};
const SCHEDULE = {
  bMuteSeconds: 25,
  selfMuteSeconds: 20,
  afterUnmuteSeconds: 15,
  afterBLeaveSeconds: 10,
  autoStopSeconds: 15,
  suggestionSeconds: 45,
  probeFinishSeconds: 240,
};
const SLACK_MS = 2_000;

let interrupted = false;
const log = (message) => console.log(`[telemost-e2e] ${message}`);
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
// For races: an unref'd timer must not keep the runner alive after the winner.
const timeout = (ms) => new Promise((resolve) => setTimeout(resolve, ms).unref());

async function pauseUntil(epochMs, reason) {
  if (reason) log(`${reason} (${Math.max(0, Math.round((epochMs - Date.now()) / 1000))} s)…`);
  while (Date.now() < epochMs) {
    if (interrupted) throw new Error("Interrumpido por el usuario");
    await delay(Math.min(500, epochMs - Date.now()));
  }
}

const pause = (seconds, reason) => pauseUntil(Date.now() + seconds * 1000, reason);

function parseArgs(argv) {
  const options = {
    meetingUrl: null,
    hostCdp: null,
    browser: process.env.KUALI_E2E_BROWSER || null,
    timeoutSeconds: 900,
    talkSeconds: null,
    keepBrowsers: false,
    recorderProfile: null,
    audio: { a: null, b: null, recorder: null },
    help: false,
  };
  const value = (index, flag) => {
    const next = argv[index + 1];
    if (next === undefined || next.startsWith("--")) throw new Error(`${flag} necesita un valor`);
    return next;
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--meeting-url") options.meetingUrl = value(index++, arg);
    else if (arg === "--host-cdp") options.hostCdp = value(index++, arg);
    else if (arg === "--browser") options.browser = value(index++, arg);
    else if (arg === "--timeout-seconds") options.timeoutSeconds = Number(value(index++, arg));
    else if (arg === "--talk-seconds") options.talkSeconds = Number(value(index++, arg));
    else if (arg === "--keep-browsers") options.keepBrowsers = true;
    else if (arg === "--recorder-profile") options.recorderProfile = resolve(value(index++, arg));
    else if (arg === "--audio-a") options.audio.a = resolve(value(index++, arg));
    else if (arg === "--audio-b") options.audio.b = resolve(value(index++, arg));
    else if (arg === "--audio-self") options.audio.recorder = resolve(value(index++, arg));
    else if (arg === "--help" || arg === "-h") options.help = true;
    else throw new Error(`Argumento desconocido: ${arg}`);
  }
  if (options.help) return options;
  if (!options.meetingUrl) throw new Error("Falta --meeting-url https://telemost.yandex.ru/j/<id>");
  const url = new URL(options.meetingUrl);
  if (!TELEMOST_ORIGINS.includes(url.origin) || !/^\/j\/\d+/.test(url.pathname)) {
    throw new Error(`--meeting-url no parece un enlace de Telemost: ${options.meetingUrl}`);
  }
  if (!Number.isInteger(options.timeoutSeconds) || options.timeoutSeconds < 300) {
    throw new Error("--timeout-seconds debe ser un entero de al menos 300");
  }
  if (options.talkSeconds !== null && (!Number.isInteger(options.talkSeconds) || options.talkSeconds < 20)) {
    throw new Error("--talk-seconds debe ser un entero de al menos 20");
  }
  return options;
}

function usage() {
  console.log(`Uso: npm run test:e2e:telemost -- --meeting-url https://telemost.yandex.ru/j/<id> [opciones]

Opciones:
  --meeting-url URL        Reunión de Telemost (obligatoria; el enlace sigue vivo tras vaciarse)
  --host-cdp URL           Chrome ya abierto y con sesión de Yandex (p. ej. http://127.0.0.1:9222)
                           que actúa como hablante A en una ventana nueva; nunca se cierra
  --browser RUTA           Chromium o Chrome for Testing ya instalado
  --timeout-seconds N      Límite de la sonda Rust (predeterminado: 900)
  --talk-seconds N         Conversación antes de las pruebas de silencio (predeterminado: 3 ciclos)
  --audio-a / --audio-b / --audio-self WAV
                           Voz de cada participante (PCM 16 bits mono); sin ellos se genera
                           con «say -v Milena» en target/e2e/audio
  --recorder-profile DIR   Perfil persistente del grabador (no se borra)
  --keep-browsers          No cierra los navegadores al terminar (depuración)

KUALI_E2E_CHROME_ARGS (array JSON) añade flags a los Chrome lanzados, p. ej.
--host-resolver-rules para ensayar contra un Telemost simulado.

La sonda transcribe en ruso (--language ru) sin tocar config.toml; usa el
modelo de Whisper elegido en Kuali.

Cada voz suena en su propia ventana de un ciclo común de reloj, así nunca se
solapan. Chrome entrega silencio con --use-file-for-fake-audio-capture, por eso
el audio entra sustituyendo getUserMedia en la página.`);
}

export function readPcm16Wav(buffer) {
  if (buffer.toString("ascii", 0, 4) !== "RIFF" || buffer.toString("ascii", 8, 12) !== "WAVE") {
    throw new Error("WAV inválido");
  }
  let format = null;
  for (let offset = 12; offset + 8 <= buffer.length;) {
    const id = buffer.toString("ascii", offset, offset + 4);
    const size = buffer.readUInt32LE(offset + 4);
    const body = offset + 8;
    if (id === "fmt ") {
      format = {
        audioFormat: buffer.readUInt16LE(body),
        channels: buffer.readUInt16LE(body + 2),
        sampleRate: buffer.readUInt32LE(body + 4),
        bits: buffer.readUInt16LE(body + 14),
      };
    } else if (id === "data") {
      if (!format || format.audioFormat !== 1 || format.channels !== 1 || format.bits !== 16) {
        throw new Error("Se esperaba WAV PCM 16 bits mono");
      }
      return { sampleRate: format.sampleRate, pcm: buffer.subarray(body, Math.min(buffer.length, body + size)) };
    }
    offset = body + size + (size % 2);
  }
  throw new Error("WAV sin bloque data");
}

export function pcm16Wav(pcm, sampleRate) {
  const header = Buffer.alloc(44);
  header.write("RIFF", 0, "ascii");
  header.writeUInt32LE(36 + pcm.length, 4);
  header.write("WAVEfmt ", 8, "ascii");
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(1, 22);
  header.writeUInt32LE(sampleRate, 24);
  header.writeUInt32LE(sampleRate * 2, 28);
  header.writeUInt16LE(2, 32);
  header.writeUInt16LE(16, 34);
  header.write("data", 36, "ascii");
  header.writeUInt32LE(pcm.length, 40);
  return Buffer.concat([header, pcm]);
}

/** Speech without leading/trailing silence (keeps `padMs` around it). */
export function trimSilence(pcm, sampleRate, { threshold = 300, padMs = 80 } = {}) {
  const samples = Math.floor(pcm.length / 2);
  let first = -1;
  let last = -1;
  for (let index = 0; index < samples; index += 1) {
    if (Math.abs(pcm.readInt16LE(index * 2)) > threshold) {
      if (first === -1) first = index;
      last = index;
    }
  }
  if (first === -1) throw new Error("El WAV no contiene voz");
  const pad = Math.round((padMs / 1000) * sampleRate);
  return pcm.subarray(Math.max(0, first - pad) * 2, Math.min(samples, last + pad + 1) * 2);
}

/**
 * Common wall-clock cycle: each role speaks once per cycle inside its own
 * slot, separated from the next speaker by SPEAKER_GAP_MS of silence.
 */
export function voiceSchedule(speechMs, t0) {
  const slotMs = Math.ceil((Math.max(...ROLES.map((role) => speechMs[role])) + SPEAKER_GAP_MS) / 100) * 100;
  return {
    t0,
    slotMs,
    cycleMs: slotMs * ROLES.length,
    offsets: Object.fromEntries(ROLES.map((role, index) => [role, index * slotMs])),
    speechMs: { ...speechMs },
  };
}

/** First speech start of `role` at or after `afterMs` (epoch ms). */
export function nextSpeechStart(schedule, role, afterMs) {
  const base = schedule.t0 + schedule.offsets[role];
  const cycles = Math.max(0, Math.ceil((afterMs - base) / schedule.cycleMs));
  return base + cycles * schedule.cycleMs;
}

async function generateSpeech(role, destination) {
  const { text } = PHRASES[role];
  const stampPath = `${destination}.txt`;
  if (await exists(destination) && (await readFile(stampPath, "utf8").catch(() => "")) === text) return destination;
  if (process.platform !== "darwin") {
    throw new Error("Sin --audio-a/--audio-b/--audio-self solo puedo generar el audio en macOS (say + afconvert)");
  }
  const aiff = `${destination}.aiff`;
  const raw = `${destination}.raw.wav`;
  for (const [command, args] of [
    ["say", ["-v", "Milena", "-o", aiff, text]],
    ["afconvert", ["-f", "WAVE", "-d", "LEI16@48000", "-c", "1", aiff, raw]],
  ]) {
    const result = spawnSync(command, args, { stdio: "inherit" });
    if (result.status !== 0) throw new Error(`${command} falló (${result.status ?? result.error?.message}); ¿está instalada la voz Milena?`);
  }
  const { pcm, sampleRate } = readPcm16Wav(await readFile(raw));
  await writeFile(destination, pcm16Wav(trimSilence(pcm, sampleRate), sampleRate));
  await writeFile(stampPath, text);
  await rm(aiff, { force: true });
  await rm(raw, { force: true });
  return destination;
}

/** Per role: trimmed speech as base64 WAV and its duration. */
export async function loadVoices(audio) {
  const directory = join(targetDir, "audio");
  await mkdir(directory, { recursive: true });
  const voices = {};
  for (const role of ROLES) {
    let path = audio[role];
    if (path) {
      if (!(await exists(path))) throw new Error(`No existe el audio indicado: ${path}`);
    } else {
      path = await generateSpeech(role, join(directory, `telemost-${role}.wav`));
    }
    const { pcm, sampleRate } = readPcm16Wav(await readFile(path));
    const speech = trimSilence(pcm, sampleRate);
    voices[role] = {
      path,
      speechMs: Math.round((speech.length / 2 / sampleRate) * 1000),
      base64: pcm16Wav(speech, sampleRate).toString("base64"),
    };
  }
  return voices;
}

/**
 * Runs in every frame before Telemost (Page.addScriptToEvaluateOnNewDocument).
 * Tracks AudioContexts so they can be resumed under a user gesture, and
 * replaces the microphone with the role's speech placed in its slot of the
 * common cycle. Every getUserMedia call (Telemost re-opens the mic on unmute,
 * Kuali opens its own) gets a clone of the same scheduled track.
 */
function voiceInjection(config) {
  if (window.__kualiE2e) return;
  const Native = window.AudioContext;
  const contexts = [];
  if (Native) {
    window.AudioContext = class extends Native {
      constructor(...args) {
        super(...args);
        contexts.push(this);
      }
    };
  }
  window.__kualiE2e = {
    contexts,
    calls: 0,
    resume: () => Promise.all(contexts.map((context) => (context.state === "suspended" ? context.resume() : null))),
  };
  const devices = navigator.mediaDevices;
  if (!config || !Native || !devices?.getUserMedia) return;
  const original = devices.getUserMedia.bind(devices);
  let voice = null;
  const startVoice = async () => {
    const context = new Native();
    contexts.push(context);
    const bytes = Uint8Array.from(atob(config.audioBase64), (char) => char.charCodeAt(0));
    const speech = await context.decodeAudioData(bytes.buffer);
    const rate = context.sampleRate;
    const cycle = context.createBuffer(1, Math.round((config.cycleMs / 1000) * rate), rate);
    const offset = Math.round((config.offsetMs / 1000) * rate);
    cycle.getChannelData(0).set(speech.getChannelData(0).subarray(0, cycle.length - offset), offset);
    const destination = context.createMediaStreamDestination();
    let source = null;
    const sync = () => {
      try { source?.stop(); } catch {}
      source = context.createBufferSource();
      source.buffer = cycle;
      source.loop = true;
      source.connect(destination);
      const phase = (((Date.now() - config.t0) % config.cycleMs) + config.cycleMs) % config.cycleMs;
      source.start(0, phase / 1000);
    };
    // Contexts made before user activation start suspended; realign on resume.
    context.addEventListener("statechange", () => {
      if (context.state === "running") sync();
    });
    sync();
    return destination.stream.getAudioTracks()[0];
  };
  let canvas = null;
  const canvasTrack = () => {
    if (!canvas) {
      canvas = document.createElement("canvas");
      canvas.width = 640;
      canvas.height = 360;
      const paint = canvas.getContext("2d");
      const draw = () => {
        paint.fillStyle = "#16324a";
        paint.fillRect(0, 0, 640, 360);
        paint.fillStyle = "#ffffff";
        paint.font = "32px sans-serif";
        paint.fillText(config.label, 40, 170);
        paint.fillText(new Date().toISOString().slice(11, 19), 40, 220);
      };
      draw();
      setInterval(draw, 250);
      canvas.stream = canvas.captureStream(5);
    }
    return canvas.stream.getVideoTracks()[0].clone();
  };
  const video = async (constraints) => (config.canvasVideo
    ? new MediaStream([canvasTrack()])
    : original({ video: constraints }).catch(() => new MediaStream()));
  devices.getUserMedia = async (constraints = {}) => {
    window.__kualiE2e.calls += 1;
    if (!constraints.audio) return constraints.video ? video(constraints.video) : original(constraints);
    voice ??= startVoice();
    const stream = constraints.video ? await video(constraints.video) : new MediaStream();
    stream.addTrack((await voice).clone());
    return stream;
  };
}

// ------------------------------------------------------------- participants

const CHROME_FLAGS = [
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-search-engine-choice-screen",
  "--use-mock-keychain",
  "--password-store=basic",
  "--autoplay-policy=no-user-gesture-required",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-backgrounding-occluded-windows",
  "--disable-renderer-backgrounding",
  "--disable-background-timer-throttling",
  "--disable-features=Translate",
  "--accept-lang=ru-RU,ru",
  "--lang=ru",
  "--window-size=1200,820",
];

// JSON array of extra flags for launched Chrome (e.g. a local Telemost mock).
function extraChromeArgs() {
  const raw = process.env.KUALI_E2E_CHROME_ARGS;
  if (!raw) return [];
  const args = JSON.parse(raw);
  if (!Array.isArray(args) || !args.every((arg) => typeof arg === "string")) {
    throw new Error("KUALI_E2E_CHROME_ARGS debe ser un array JSON de cadenas");
  }
  return args;
}

export class Participant {
  constructor({ role, name, client, page, frames, child = null, hostTarget = false }) {
    Object.assign(this, { role, name, client, page, frames, child, hostTarget });
    this.extensionId = null;
    this.worker = null;
  }

  static async launch({ role, name, executable, extension = null, profileDir, windowIndex }) {
    await mkdir(profileDir, { recursive: true });
    await rm(join(profileDir, "DevToolsActivePort"), { force: true });
    const args = [
      `--user-data-dir=${profileDir}`,
      "--remote-debugging-port=0",
      ...CHROME_FLAGS,
      `--window-position=${40 + windowIndex * 80},${40 + windowIndex * 60}`,
      ...extraChromeArgs(),
      ...(extension
        ? [`--disable-extensions-except=${extension}`, `--load-extension=${extension}`]
        : ["--disable-extensions"]),
      "about:blank",
    ];
    const child = spawn(executable, args, { stdio: "ignore" });
    child.once("error", (error) => log(`${name}: el navegador falló: ${error.message}`));
    const { port } = await readDevToolsActivePort(profileDir);
    const client = await CdpClient.connect(`http://127.0.0.1:${port}`);
    for (const origin of TELEMOST_ORIGINS) {
      await client.send("Browser.grantPermissions", { origin, permissions: ["audioCapture", "videoCapture"] }).catch(() => {});
    }
    const target = await client.waitForTarget((item) => item.type === "page" && item.url === "about:blank");
    const page = await client.attach(target.targetId);
    const frames = await FrameContexts.attach(page);
    const participant = new Participant({ role, name, client, page, frames, child });
    if (extension) {
      const worker = await client.waitForTarget(
        (item) => item.type === "service_worker" && item.url.endsWith("/src/background.js"),
        { timeoutMs: 15_000 },
      );
      participant.extensionId = new URL(worker.url).host;
      participant.collectConsole(page, "page");
      await participant.workerSession();
    }
    return participant;
  }

  /** New window in a running, signed-in Chrome; never closes that browser. */
  static async fromHost({ endpoint, name }) {
    const client = await CdpClient.connect(endpoint);
    const { targetId } = await client.send("Target.createTarget", { url: "about:blank", newWindow: true });
    const page = await client.attach(targetId);
    const frames = await FrameContexts.attach(page);
    return new Participant({ role: "a", name, client, page, frames, hostTarget: true });
  }

  /** Runs before navigation and covers every frame, the call iframe too. */
  async installVoice(voice, schedule, { canvasVideo = false } = {}) {
    const config = voice && {
      audioBase64: voice.base64,
      t0: schedule.t0,
      cycleMs: schedule.cycleMs,
      offsetMs: schedule.offsets[this.role],
      canvasVideo,
      label: this.name,
    };
    await this.page.send("Page.addScriptToEvaluateOnNewDocument", { source: `(${voiceInjection})(${JSON.stringify(config)});` });
  }

  callContext() {
    return this.frames.contextFor(CALL_FRAME);
  }

  /** objectId of the first alternative in the call iframe (or top document). */
  async locate(alternatives, { inCall = true, includeHidden = false } = {}) {
    const contextId = inCall ? await this.callContext() : undefined;
    if (inCall && !contextId) return null;
    for (const spec of alternatives) {
      const element = await findElement(this.page, { contextId, includeHidden, ...spec });
      if (element) return element;
    }
    return null;
  }

  async has(alternatives, options) {
    const element = await this.locate(alternatives, options);
    await releaseObject(this.page, element);
    return !!element;
  }

  async click(alternatives, { inCall = true, description = "el control" } = {}) {
    const element = await this.locate(alternatives, { inCall });
    if (!element) throw new Error(`${this.name}: no encontré ${description}`);
    try {
      await clickNode(this.page, { objectId: element });
    } finally {
      await releaseObject(this.page, element);
    }
  }

  inCall() {
    return this.has(UI.leave);
  }

  async join(url, displayName, timeoutMs = 120_000) {
    log(`${this.name}: abriendo ${url}`);
    await this.page.send("Page.navigate", { url });
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (interrupted) throw new Error("Interrumpido por el usuario");
      if (await this.callContext()) {
        if (await this.inCall()) {
          log(`${this.name}: dentro de la llamada`);
          return;
        }
        const button = await this.locate(UI.joinButton);
        if (button) {
          const input = displayName ? await this.locate(UI.nameInput) : null;
          if (input) {
            await callFunction(this.page, input, `function (value) {
              const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
              setter.call(this, value);
              this.dispatchEvent(new Event("input", { bubbles: true }));
            }`, [displayName]);
            await releaseObject(this.page, input);
            await delay(300);
          }
          const clicked = await clickNode(this.page, { objectId: button }).then(() => true, (error) => {
            log(`${this.name}: no pude pulsar «Подключиться» (${error.message}); reintento`);
            return false;
          });
          await releaseObject(this.page, button);
          if (clicked) {
            log(`${this.name}: pulsé «Подключиться»${input ? ` como «${displayName}»` : ""}`);
            // The button stays up while Telemost connects; clicking it again
            // then hits a node that is being removed.
            await waitFor(async () => (await this.inCall()) || !(await this.has(UI.joinButton)), {
              timeoutMs: 20_000,
              intervalMs: 500,
            }).catch(() => {});
          }
          continue;
        }
      } else if (await this.has(UI.continueInBrowser, { inCall: false })) {
        await this.click(UI.continueInBrowser, { inCall: false, description: "«Продолжить в браузере»" });
        log(`${this.name}: pulsé «Продолжить в браузере»`);
        await delay(2_000);
        continue;
      }
      await delay(500);
    }
    throw new Error(`${this.name}: no logré entrar a la llamada en ${timeoutMs / 1000} s`);
  }

  /** true = microphone on, false = muted, null = no toolbar button. */
  async micOn() {
    if (await this.has(UI.micOn)) return true;
    if (await this.has(UI.micOff)) return false;
    return null;
  }

  async setMic(on) {
    const before = await this.micOn();
    if (before === null) throw new Error(`${this.name}: no encontré el botón de micrófono de la barra`);
    if (before === on) return { at: Date.now(), confirmed: true, already: true };
    await this.click(before ? UI.micOn : UI.micOff, { description: "el botón de micrófono" });
    const at = Date.now();
    const confirmed = await waitFor(async () => (await this.micOn()) === on, { timeoutMs: 5_000, intervalMs: 250 })
      .then(() => true, () => false);
    log(`${this.name}: micrófono ${on ? "abierto" : "silenciado"}${confirmed ? "" : " (sin confirmar en la interfaz)"}`);
    return { at, confirmed, already: false };
  }

  async leave() {
    if (!(await this.inCall().catch(() => false))) return null;
    await this.click(UI.leave, { description: "«Выйти из встречи»" });
    const at = Date.now();
    await delay(1_500);
    // A confirmation dialog, if any, sits over a still-active call.
    if (await this.inCall().catch(() => false) && await this.has(UI.leaveConfirm).catch(() => false)) {
      await this.click(UI.leaveConfirm, { description: "la confirmación de salida" }).catch(() => {});
    }
    log(`${this.name}: salió de la llamada`);
    return at;
  }

  /** Visible names in the call UI (text, title and aria-label). */
  async seesNames(names) {
    const contextId = await this.callContext();
    if (!contextId) return false;
    const text = await evaluate(this.page, `(() => {
      const parts = [document.body ? document.body.innerText : ""];
      for (const element of document.querySelectorAll("[title], [aria-label]")) {
        parts.push(element.getAttribute("title") || "", element.getAttribute("aria-label") || "");
      }
      return parts.join("\\n");
    })()`, { contextId });
    return names.every((name) => text.includes(name) || text.includes(name.split(" ")[0]));
  }

  /** Grants activation with a userGesture evaluate, then resumes audio. */
  async resumeAudio() {
    const results = [];
    for (const contextId of [undefined, await this.callContext()]) {
      results.push(await evaluate(this.page, `window.__kualiE2e
        ? window.__kualiE2e.resume().then(() => window.__kualiE2e.contexts.map((context) => context.state).join(","))
        : "sin inyección"`, { contextId, userGesture: true }).catch((error) => error.message));
    }
    return results;
  }

  // --- Kuali extension (recorder only)

  async suggestionHost() {
    const contextId = await this.callContext();
    if (!contextId) return null;
    return findElement(this.page, { contextId, selector: "#kuali-recording-suggestion", includeHidden: true });
  }

  async clickInShadow(host, selector, description) {
    const element = await shadowQuery(this.page, host, selector);
    if (!element) throw new Error(`no encontré ${description}`);
    try {
      await clickNode(this.page, { objectId: element });
    } finally {
      await releaseObject(this.page, element);
    }
  }

  /** «Revisar» → consent → «Grabar y transcribir» in the closed shadow root. */
  async startViaSuggestion(host) {
    await delay(500);
    await this.clickInShadow(host, ".record", "el botón «Revisar» de la sugerencia");
    await delay(400);
    await this.clickInShadow(host, ".consent-check input", "la casilla de consentimiento");
    const confirm = await shadowQuery(this.page, host, ".confirm");
    if (!confirm) throw new Error("la sugerencia no tiene el botón de confirmar");
    try {
      await waitFor(() => callFunction(this.page, confirm, "function () { return !this.disabled; }"), {
        timeoutMs: 3_000,
        intervalMs: 150,
        description: "que se habilite «Grabar y transcribir»",
      });
      await clickNode(this.page, { objectId: confirm });
    } finally {
      await releaseObject(this.page, confirm);
    }
  }

  async indicatorShown() {
    return evaluate(this.page, "!!document.getElementById('kuali-recording-indicator')").catch(() => false);
  }

  async workerSession() {
    if (this.worker && !this.worker.client.closed) {
      const alive = await evaluate(this.worker, "1").then(() => true, () => false);
      if (alive) return this.worker;
    }
    const target = await this.client.waitForTarget(
      (item) => item.type === "service_worker" && item.url.startsWith(`chrome-extension://${this.extensionId}/`),
      { timeoutMs: 10_000 },
    );
    this.worker = await this.client.attach(target.targetId);
    await this.worker.send("Runtime.enable").catch(() => {});
    this.collectConsole(this.worker, "service-worker");
    return this.worker;
  }

  /** Kuali console output (page, iframes, service worker) for the report. */
  collectConsole(session, source) {
    this.consoleLog ??= [];
    const relevant = (text, url) => source === "service-worker"
      || /chrome-extension:\/\/|kuali/i.test(`${url || ""} ${text || ""}`);
    const push = (level, text, url) => {
      if (this.consoleLog.length >= 2000 || !relevant(text, url)) return;
      this.consoleLog.push({ ts: Date.now(), source, level, text: String(text).slice(0, 1000), url: url || null });
    };
    session.on("Runtime.consoleAPICalled", ({ type, args, stackTrace }) => {
      push(type, args.map((arg) => arg.value ?? arg.description ?? arg.type).join(" "), stackTrace?.callFrames?.[0]?.url);
    });
    session.on("Runtime.exceptionThrown", ({ exceptionDetails: details }) => {
      push("exception", details.exception?.description || details.text, details.url || details.stackTrace?.callFrames?.[0]?.url);
    });
    session.on("Log.entryAdded", ({ entry }) => push(entry.level, entry.text, entry.url));
    session.send("Log.enable").catch(() => {});
  }

  async setKualiPort(port) {
    await evaluate(await this.workerSession(), `chrome.storage.local.set({ kualiPort: ${Number(port)} }).then(() => true)`);
  }

  /** Opens the real action popup for this window's active tab. */
  async openPopup() {
    const popupUrl = `chrome-extension://${this.extensionId}/popup.html`;
    for (const stale of (await this.client.targets()).filter((item) => item.url === popupUrl)) {
      await this.client.send("Target.closeTarget", { targetId: stale.targetId }).catch(() => {});
    }
    await this.page.send("Page.bringToFront").catch(() => {});
    await evaluate(await this.workerSession(), "chrome.action.openPopup().then(() => true)");
    const target = await this.client.waitForTarget((item) => item.url === popupUrl, { timeoutMs: 5_000 });
    const popup = await this.client.attach(target.targetId);
    await popup.send("Runtime.enable");
    await popup.send("DOM.enable");
    await waitFor(() => evaluate(popup, "document.readyState === 'complete' && !!document.getElementById('status')"), {
      timeoutMs: 5_000,
      intervalMs: 150,
      description: "el popup",
    });
    // The popup renders after its async capture-state round trip.
    await delay(800);
    return popup;
  }

  async closePopup(popup) {
    await evaluate(popup, "window.close()").catch(() => {});
    await popup.detach();
  }

  async popupState() {
    const popup = await this.openPopup();
    try {
      return await evaluate(popup, `({
        platform: document.getElementById("platform").textContent,
        state: document.getElementById("status").dataset.state,
        status: document.getElementById("status").textContent,
      })`);
    } finally {
      await this.closePopup(popup);
    }
  }

  async startViaPopup() {
    const popup = await this.openPopup();
    try {
      const consent = await findElement(popup, { selector: "#participant-consent" });
      if (!consent) throw new Error("el popup no muestra la casilla de consentimiento (¿pestaña no reconocida?)");
      await clickNode(popup, { objectId: consent });
      await releaseObject(popup, consent);
      await waitFor(() => evaluate(popup, "!document.getElementById('toggle').disabled"), {
        timeoutMs: 5_000,
        intervalMs: 200,
        description: "que se habilite el botón del popup",
      });
      const toggle = await findElement(popup, { selector: "#toggle" });
      await clickNode(popup, { objectId: toggle });
      await releaseObject(popup, toggle);
    } finally {
      await delay(500);
      await this.closePopup(popup);
    }
  }

  /** Manual stop after a failed auto-stop: indicator, popup, extension page. */
  async forceStop() {
    try {
      const host = await findElement(this.page, { selector: "#kuali-recording-indicator", includeHidden: true });
      if (host) {
        try {
          await this.clickInShadow(host, ".toggle", "el botón de expandir del indicador");
          await delay(300);
          await this.clickInShadow(host, ".stop", "el botón Detener del indicador");
        } finally {
          await releaseObject(this.page, host);
        }
        return "indicador";
      }
    } catch (error) {
      log(`No pude detener desde el indicador: ${error.message}`);
    }
    try {
      const popup = await this.openPopup();
      try {
        if (await evaluate(popup, "document.getElementById('status').dataset.state") !== "idle") {
          const toggle = await findElement(popup, { selector: "#toggle" });
          await clickNode(popup, { objectId: toggle });
          return "popup";
        }
      } finally {
        await this.closePopup(popup);
      }
    } catch (error) {
      log(`No pude detener desde el popup: ${error.message}`);
    }
    const tabId = await this.tabId();
    const { targetId } = await this.client.send("Target.createTarget", { url: `chrome-extension://${this.extensionId}/privacy.html` });
    const page = await this.client.attach(targetId);
    await delay(1_000);
    await evaluate(page, `chrome.runtime.sendMessage({ type: "capture-stop", tabId: ${tabId} }).then(() => true, () => true)`);
    await this.client.send("Target.closeTarget", { targetId }).catch(() => {});
    return "mensaje a la extensión";
  }

  async tabId() {
    const worker = await this.workerSession();
    return evaluate(worker, `chrome.tabs.query({}).then((tabs) => (tabs.find((tab) => /telemost/.test(tab.url || "")) || {}).id ?? -1)`);
  }

  async close() {
    if (this.hostTarget) {
      await this.client.send("Target.closeTarget", { targetId: this.page.targetId }).catch(() => {});
      this.client.close();
      return;
    }
    this.client?.close();
    await terminate(this.child);
  }
}

// --------------------------------------------------------------- assessment

const nameMatches = (value, name) => {
  const text = String(value || "").toLowerCase();
  return !!name && (text.includes(name.toLowerCase()) || text.includes(name.split(" ")[0].toLowerCase()));
};

export function channelsOf(report, predicate) {
  return Object.values(report?.wire?.channels || {}).filter(predicate);
}

/** Active seconds before, inside (with slack) and after a muted window. */
export function muteWindow(activeSeconds, fromMs, toMs, { slackMs = SLACK_MS, contextMs = 30_000 } = {}) {
  const seconds = [...new Set(activeSeconds)].sort((left, right) => left - right);
  const inside = seconds.filter((second) => second * 1000 >= fromMs + slackMs && (second + 1) * 1000 <= toMs - slackMs);
  const before = seconds.filter((second) => second * 1000 >= fromMs - contextMs && (second + 1) * 1000 <= fromMs);
  const after = seconds.filter((second) => second * 1000 >= toMs + 1000 && second * 1000 <= toMs + contextMs);
  return { before: before.length, inside, after: after.length };
}

export function participantLeft(timeline, { sinceMs, channels, participantIds, name }) {
  return (timeline || []).find((entry) => {
    if (entry.tsMs < sinceMs) return false;
    if (entry.kind === "participant-left") {
      return channels.includes(entry.channel)
        || participantIds.includes(entry.participantId)
        || nameMatches(entry.displayName, name);
    }
    if (entry.kind === "roster-state" && Array.isArray(entry.participants)) {
      return entry.participants.length > 0 && !entry.participants.some((person) => nameMatches(person.displayName, name));
    }
    return false;
  }) || null;
}

export function activeSpeakerNames(timeline) {
  const names = new Set();
  for (const entry of timeline || []) {
    if (entry.kind !== "active-speakers") continue;
    for (const person of entry.participants || []) if (person.displayName) names.add(person.displayName);
  }
  return [...names];
}

function check(name, passed, detail, level = "error") {
  return { name, passed: !!passed, level: passed ? "ok" : level, detail };
}

function muteCheck(name, channels, mute, unmute, schedule, error) {
  if (!mute || !unmute) return check(name, false, error || "no se ejecutó");
  if (!channels.length) return check(name, false, "no hubo canal con ese participante");
  const window = muteWindow(channels.flatMap((channel) => channel.activeSeconds || []), mute.at, unmute.at, {
    contextMs: schedule.cycleMs + 6_000,
  });
  return check(name, window.inside.length === 0 && window.before > 0 && window.after > 0,
    `canales ${channels.map((channel) => channel.channel).join(",")}: ${window.before} s con voz antes, `
    + `${window.inside.length} dentro${window.inside.length ? ` (${window.inside.join(",")})` : ""}, ${window.after} después`
    + `${mute.confirmed && unmute.confirmed ? "" : " · la interfaz no confirmó el cambio"}`);
}

function assess({ report, events, hostMode, ingest, schedule }) {
  const results = [];
  const timeline = report?.wire?.timeline || [];
  results.push(check("Sugerencia de Kuali mostrada", events.suggestionShownAt,
    events.suggestionShownAt
      ? `a los ${Math.round((events.suggestionShownAt - events.recorderInCallAt) / 1000)} s de entrar`
      : `no apareció en ${SCHEDULE.suggestionSeconds} s`));
  results.push(check("Captura iniciada desde la sugerencia", events.startedVia === "suggestion",
    events.startedVia
      ? `iniciada vía ${events.startedVia}${events.suggestionError ? ` (sugerencia: ${events.suggestionError})` : ""}`
      : "no se inició"));
  results.push(check("Captura activa (indicador + /ingest)", events.capturingAt,
    `${ingest.length} conexión(es) /ingest, indicador=${events.indicatorShown ? "sí" : "no"}`));
  if (events.popupState) {
    results.push(check("El popup refleja la captura", events.popupState.state === "capturing",
      `${events.popupState.platform} · ${events.popupState.status}`, "warn"));
  }
  results.push(check("Los participantes se ven", events.allSeeEachOther, events.seeDetail || "no se comprobó", "warn"));

  const bChannels = channelsOf(report, (channel) => nameMatches(channel.displayName, NAMES.b));
  results.push(muteCheck(`Silencio de ${NAMES.b}`, bChannels, events.bMute, events.bUnmute, schedule, events.bMuteError));
  const selfChannels = channelsOf(report, (channel) => channel.isSelf || channel.channel === 1000);
  results.push(muteCheck("Silencio propio del grabador", selfChannels, events.selfMute, events.selfUnmute, schedule, events.selfMuteError));

  if (events.bLeftAt) {
    const left = participantLeft(timeline, {
      sinceMs: events.bLeftAt - 1_000,
      channels: bChannels.map((channel) => channel.channel),
      participantIds: bChannels.map((channel) => channel.participantId).filter(Boolean),
      name: NAMES.b,
    });
    results.push(check(`Salida de ${NAMES.b} en el timeline`, left,
      left ? `${left.kind} a los ${((left.tsMs - events.bLeftAt) / 1000).toFixed(1)} s` : "ni participant-left ni roster-state sin él"));
  } else {
    results.push(check(`Salida de ${NAMES.b} en el timeline`, false, events.bLeaveError || "no se ejecutó"));
  }

  const hostName = hostMode
    ? channelsOf(report, (channel) => !channel.isSelf && channel.channel !== 1000 && channel.displayName
      && !nameMatches(channel.displayName, NAMES.b) && !/sin identificar|^Sala$/.test(channel.displayName))[0]?.displayName
    : NAMES.a;
  const speakers = activeSpeakerNames(timeline);
  const sawA = speakers.some((name) => nameMatches(name, hostName));
  const sawB = speakers.some((name) => nameMatches(name, NAMES.b));
  results.push(check("Hablantes activos (A y B)", sawA && sawB,
    speakers.length ? `active-speakers con: ${speakers.join(", ")}` : "sin eventos active-speakers", "warn"));

  if (hostMode) {
    const said = (report?.utterances || []).filter((utterance) => String(utterance.text).toLowerCase().includes(PHRASES.a.term));
    const byHost = said.some((utterance) => !utterance.speakerIsSelf && !nameMatches(utterance.speakerName, NAMES.b)
      && (!hostName || nameMatches(utterance.speakerName, hostName)));
    results.push(check(`Atribución: anfitrión (${hostName || "?"}) dijo «${PHRASES.a.term}»`, byHost,
      said.length ? `lo dijo(eron): ${said.map((utterance) => utterance.speakerName || "?").join(", ")}` : "nadie dijo ese término"));
  }

  const autoStopMs = events.recorderLeftAt && events.autoStoppedAt ? events.autoStoppedAt - events.recorderLeftAt : null;
  results.push(check("Parada automática al salir", autoStopMs !== null && autoStopMs <= SCHEDULE.autoStopSeconds * 1000 && !events.forcedStop,
    autoStopMs !== null
      ? `la extensión cerró /ingest ${(autoStopMs / 1000).toFixed(1)} s después de salir (código ${events.autoStopCode ?? "?"}); websocket-1000 en la sonda=${report?.wire?.cleanClose ?? "?"}`
      : events.forcedStop ? `no se detuvo sola; detenida vía ${events.forcedStop}` : events.recorderLeaveError || "no se ejecutó"));
  results.push(check("Informe de la sonda escrito", report, report ? `outcome=${report.outcome}` : "la sonda no escribió informe"));
  return results;
}

function printTable(title, rows) {
  console.log(`\n${title}`);
  for (const row of rows) {
    const mark = row.level === "warn" ? "WARN" : row.passed ? "PASS" : "FAIL";
    console.log(`  ${mark}  ${row.name} — ${row.detail}`);
  }
}

// --------------------------------------------------------------------- main

/** When the extension stopped this /ingest session (Close frame or TCP end). */
const stopTime = (session) => (session ? session.closeFrameMs ?? session.closedMs : null);

/**
 * Applies one probe stdout line to `state`: READY, REPORT and the
 * KUALI_E2E_INGEST session updates. Returns the READY payload when it arrives.
 */
export function applyProbeLine(state, line) {
  if (line.startsWith("KUALI_E2E_READY ")) {
    state.ready = JSON.parse(line.slice("KUALI_E2E_READY ".length));
    return state.ready;
  }
  if (line.startsWith("KUALI_E2E_REPORT ")) {
    state.reportPath = line.slice("KUALI_E2E_REPORT ".length).trim();
  } else if (line.startsWith("KUALI_E2E_INGEST ")) {
    const { connection } = JSON.parse(line.slice("KUALI_E2E_INGEST ".length));
    state.ingest[connection.index] = connection;
  }
  return null;
}

function spawnProbe(options) {
  const args = [
    "run", "--release", "-p", "kuali-engine", "--example", "meet_live_e2e", "--",
    "--full",
    "--platform", "yandex_telemost",
    "--timeout-seconds", String(options.timeoutSeconds),
    "--terms", ROLES.map((role) => PHRASES[role].term).join(","),
    "--min-terms", String(ROLES.length),
    ...(options.hostCdp ? [] : ["--expect-speaker", `${NAMES.a}=${PHRASES.a.term}`]),
    "--expect-speaker", `${NAMES.b}=${PHRASES.b.term}`,
    "--expect-speaker", `@self=${PHRASES.recorder.term}`,
    "--language", "ru",
  ];
  const probe = spawn("cargo", args, { cwd: workspaceRoot, stdio: ["ignore", "pipe", "pipe"] });
  probe.stderr.pipe(process.stderr);
  const state = { ready: null, reportPath: null, ingest: [], exited: false, code: null, signal: null };
  let ready;
  state.readyPromise = new Promise((resolve, reject) => {
    ready = { resolve, reject };
  });
  state.readyPromise.catch(() => {});
  let buffer = "";
  probe.stdout.on("data", (chunk) => {
    const text = chunk.toString();
    process.stdout.write(text);
    buffer += text;
    let newline;
    while ((newline = buffer.indexOf("\n")) !== -1) {
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      if (applyProbeLine(state, line)) ready.resolve(state.ready);
    }
  });
  state.exitPromise = once(probe, "exit").then(([code, signal]) => {
    Object.assign(state, { exited: true, code, signal });
    ready.reject(new Error(`La sonda terminó antes de estar lista (código ${code ?? signal})`));
  });
  return { probe, state };
}

async function waitForProbe(probe, state, seconds) {
  const finished = await Promise.race([state.exitPromise.then(() => true), timeout(seconds * 1000).then(() => false)]);
  if (finished) return;
  log(`La sonda no terminó en ${seconds} s; la interrumpo para que escriba el informe`);
  probe.kill("SIGINT");
  const stopped = await Promise.race([state.exitPromise.then(() => true), timeout(30_000).then(() => false)]);
  if (!stopped) await terminate(probe);
}

/** Mutes `participant` so the window fully covers one of its speech slots. */
async function muteAcrossSlot(participant, schedule, minimumSeconds) {
  const speechStart = nextSpeechStart(schedule, participant.role, Date.now() + 5_000);
  await pauseUntil(speechStart - 3_000, `Esperando el turno de ${participant.name} para silenciarlo`);
  const mute = await participant.setMic(false);
  const seconds = Math.max(minimumSeconds, Math.ceil((schedule.speechMs[participant.role] + 6_000) / 1000));
  await pause(seconds, `${participant.name} silenciado`);
  const unmute = await participant.setMic(true);
  return { mute, unmute };
}

async function runScenario({ options, participants, probeState, events, schedule }) {
  const { a, b, recorder } = participants;
  const url = options.meetingUrl;

  await Promise.all([a.join(url, options.hostCdp ? null : NAMES.a), b.join(url, NAMES.b)]);
  await recorder.join(url, NAMES.recorder);
  events.recorderInCallAt = Date.now();
  for (const participant of [a, b, recorder]) {
    if ((await participant.micOn()) === false) await participant.setMic(true);
  }
  if (options.hostCdp) {
    // The host browser lacks --autoplay-policy; resume its AudioContexts.
    events.hostAudio = await a.resumeAudio();
    log(`Anfitrión: AudioContexts ${events.hostAudio.join(" / ")}`);
  }

  // Suggestion first; the popup is the fallback start path.
  try {
    const host = await waitFor(() => recorder.suggestionHost(), {
      timeoutMs: SCHEDULE.suggestionSeconds * 1000,
      intervalMs: 300,
      description: "la sugerencia de Kuali",
    });
    events.suggestionShownAt = Date.now();
    log("Sugerencia de Kuali visible; la acepto");
    try {
      await recorder.startViaSuggestion(host);
    } finally {
      await releaseObject(recorder.page, host);
    }
    events.startedVia = "suggestion";
  } catch (error) {
    events.suggestionError = error.message;
    // A half-failed suggestion may still have started the capture.
    const started = await waitFor(() => probeState.ingest.length > 0, { timeoutMs: 3_000, intervalMs: 250 })
      .then(() => true, () => false);
    if (started) {
      events.startedVia = "suggestion";
    } else {
      log(`Sugerencia: ${error.message}. Intento desde el popup`);
      await recorder.startViaPopup();
      events.startedVia = "popup";
    }
  }
  await waitFor(async () => probeState.ingest.length > 0 && await recorder.indicatorShown(), {
    timeoutMs: 30_000,
    description: "el indicador de grabación y la conexión /ingest",
  });
  events.capturingAt = Date.now();
  events.indicatorShown = true;
  log("Kuali está capturando");
  try {
    events.popupState = await recorder.popupState();
  } catch (error) {
    log(`No pude leer el popup: ${error.message}`);
  }

  const seen = [];
  for (const participant of [a, b, recorder]) {
    const names = [a, b, recorder].filter((item) => item !== participant && !item.hostTarget).map((item) => item.name);
    const ok = await waitFor(() => participant.seesNames(names), { timeoutMs: 30_000, intervalMs: 1_000 })
      .then(() => true, () => false);
    seen.push(`${participant.name}${ok ? "" : " NO"} ve a ${names.join(", ")}`);
  }
  events.allSeeEachOther = !seen.some((line) => line.includes(" NO "));
  events.seeDetail = seen.join("; ");

  const talkSeconds = options.talkSeconds ?? Math.ceil((schedule.cycleMs * 3) / 1000);
  await pause(talkSeconds, "Conversación con las tres voces");

  try {
    ({ mute: events.bMute, unmute: events.bUnmute } = await muteAcrossSlot(b, schedule, SCHEDULE.bMuteSeconds));
  } catch (error) {
    events.bMuteError = error.message;
  }
  await pause(SCHEDULE.afterUnmuteSeconds, `${NAMES.b} vuelve a hablar`);

  try {
    ({ mute: events.selfMute, unmute: events.selfUnmute } = await muteAcrossSlot(recorder, schedule, SCHEDULE.selfMuteSeconds));
  } catch (error) {
    events.selfMuteError = error.message;
  }
  // Leave room for the "after unmute" activity of both muted voices.
  await pause(Math.ceil(schedule.cycleMs / 1000) + 4, "Grabador vuelve a hablar");

  try {
    events.bLeftAt = await b.leave();
    if (!events.bLeftAt) events.bLeaveError = `${NAMES.b} ya no estaba en la llamada`;
  } catch (error) {
    events.bLeaveError = error.message;
  }
  await pause(SCHEDULE.afterBLeaveSeconds, `Esperando el evento de salida de ${NAMES.b}`);

  try {
    events.recorderLeftAt = await recorder.leave();
    if (!events.recorderLeftAt) throw new Error("el grabador ya no estaba en la llamada");
    const closed = await waitFor(() => stopTime(probeState.ingest.at(-1)), {
      timeoutMs: SCHEDULE.autoStopSeconds * 1000,
      intervalMs: 250,
    }).catch(() => null);
    if (closed) {
      events.autoStoppedAt = closed;
      events.autoStopCode = probeState.ingest.at(-1).closeCode;
      log(`Kuali se detuvo solo ${((closed - events.recorderLeftAt) / 1000).toFixed(1)} s después de salir`);
    }
  } catch (error) {
    events.recorderLeaveError = error.message;
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) return usage();
  await mkdir(targetDir, { recursive: true });
  const startedAt = new Date();
  const stamp = startedAt.toISOString().replace(/[-:]/g, "").replace(/\.\d+Z$/, "Z");

  const temporaryRoot = await mkdtemp(join(tmpdir(), "kuali-telemost-e2e-"));
  const participants = {};
  const events = {};
  let probe = null;
  let probeState = null;
  let schedule = null;
  let scenarioError = null;
  const onInterrupt = () => {
    interrupted = true;
    probe?.kill("SIGINT");
  };
  process.on("SIGINT", onInterrupt);

  try {
    const browserPromise = ensureBrowser(options.browser);
    browserPromise.catch(() => {});
    const voicesPromise = loadVoices(options.audio);
    voicesPromise.catch(() => {});
    ({ probe, state: probeState } = spawnProbe(options));
    const [ready, executable, voices] = await Promise.all([probeState.readyPromise, browserPromise, voicesPromise]);
    schedule = voiceSchedule(Object.fromEntries(ROLES.map((role) => [role, voices[role].speechMs])), Math.floor(Date.now() / 1000) * 1000);
    log(`Ciclo de voces de ${schedule.cycleMs / 1000} s (turnos de ${schedule.slotMs / 1000} s: A, B, grabador)`);
    log(`Sonda en el puerto ${ready.port} (/health → motor, /ingest observado)`);
    const extension = await patchedExtension(ready.port, temporaryRoot);

    const launches = [
      options.hostCdp
        ? Participant.fromHost({ endpoint: options.hostCdp, name: "Anfitrión (A)" })
        : Participant.launch({ role: "a", name: NAMES.a, executable, profileDir: join(temporaryRoot, "profile-a"), windowIndex: 0 }),
      Participant.launch({ role: "b", name: NAMES.b, executable, profileDir: join(temporaryRoot, "profile-b"), windowIndex: 1 }),
      Participant.launch({
        role: "recorder", name: NAMES.recorder, executable, extension,
        profileDir: options.recorderProfile || join(temporaryRoot, "profile-recorder"), windowIndex: 2,
      }),
    ];
    const settled = await Promise.allSettled(launches);
    [participants.a, participants.b, participants.recorder] = settled.map((item) => (item.status === "fulfilled" ? item.value : null));
    const failed = settled.find((item) => item.status === "rejected");
    if (failed) throw failed.reason;
    for (const role of ROLES) {
      // Host mode: a canvas camera so the host's real camera is never opened.
      await participants[role].installVoice(voices[role], schedule, { canvasVideo: participants[role].hostTarget });
    }
    await participants.recorder.setKualiPort(ready.port);

    await runScenario({ options, participants, probeState, events, schedule });
  } catch (error) {
    scenarioError = error;
    console.error(`[telemost-e2e] ERROR: ${error.stack || error.message}`);
  }

  try {
    const recorder = participants.recorder;
    const lastIngest = probeState?.ingest.at(-1);
    if (recorder && lastIngest && !stopTime(lastIngest)) {
      events.forcedStop = await recorder.forceStop().catch((error) => `falló (${error.message})`);
      log(`Captura detenida manualmente vía ${events.forcedStop}`);
    }
    if (probe && probeState && !probeState.exited) {
      await waitForProbe(probe, probeState, lastIngest ? SCHEDULE.probeFinishSeconds : 5);
    }
  } finally {
    for (const participant of [participants.a, participants.b, participants.recorder]) {
      if (!participant) continue;
      if (participant.hostTarget) {
        await participant.leave().catch(() => null);
        await participant.close();
      } else if (options.keepBrowsers) {
        participant.client.close();
        participant.child?.unref();
      } else {
        await participant.close();
      }
    }
    if (options.keepBrowsers) {
      log(`Navegadores y perfiles conservados en ${temporaryRoot}`);
    } else if (temporaryRoot.startsWith(`${tmpdir()}${sep}`)) {
      await rm(temporaryRoot, { recursive: true, force: true });
    }
    process.removeListener("SIGINT", onInterrupt);
  }

  let report = null;
  if (probeState?.reportPath) {
    report = JSON.parse(await readFile(probeState.reportPath, "utf8").catch(() => "null"));
  }
  // Telemost sends avatars only for signed-in users, so guests get none.
  const guestsOnly = !options.hostCdp && !options.recorderProfile;
  const probeAssertions = (report?.assertions || []).map((item) => (
    !item.passed && guestsOnly && item.name === "Foto de participante"
      ? { ...item, level: "warn", detail: `${item.detail} (esperado: solo invitados, Telemost no envía fotos)` }
      : { ...item, level: item.passed ? "ok" : "error" }
  ));
  const runnerAssertions = assess({
    report,
    events,
    hostMode: !!options.hostCdp,
    ingest: probeState?.ingest || [],
    schedule: schedule || { cycleMs: 30_000 },
  });
  if (scenarioError) runnerAssertions.push(check("Escenario completo", false, scenarioError.message));
  const reportDir = probeState?.reportPath ? dirname(probeState.reportPath) : join(targetDir, "reports");
  await mkdir(reportDir, { recursive: true });
  const combinedPath = join(reportDir, `telemost-e2e-${stamp}.json`);
  await writeFile(combinedPath, `${JSON.stringify({
    ...(report || {}),
    probeReportPath: probeState?.reportPath || null,
    runner: {
      meetingUrl: options.meetingUrl,
      hostMode: !!options.hostCdp,
      startedAt: startedAt.toISOString(),
      finishedAt: new Date().toISOString(),
      schedule,
      events,
      recorderConsole: participants.recorder?.consoleLog || [],
      probeWarnings: probeAssertions.filter((item) => item.level === "warn").map((item) => item.name),
    },
    runnerAssertions,
  }, null, 2)}\n`);

  printTable("Sonda (meet_live_e2e)", probeAssertions);
  printTable("Runner Telemost", runnerAssertions);
  const failures = [...probeAssertions, ...runnerAssertions].filter((item) => !item.passed && item.level !== "warn");
  console.log(`\n${failures.length ? `E2E TELEMOST FALLÓ (${failures.length})` : "E2E TELEMOST APROBADO"}`);
  console.log(`KUALI_TELEMOST_E2E_REPORT ${combinedPath}`);
  process.exitCode = failures.length || !report ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error.stack || error.message || String(error));
    process.exitCode = 1;
  });
}
