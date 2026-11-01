// Minimal Chrome DevTools Protocol client for the E2E runners. It uses Node's
// global WebSocket (Node >= 22) and flat sessions, so it needs no dependency.

import { readFile } from "node:fs/promises";
import { join } from "node:path";

const DEFAULT_TIMEOUT_MS = 30_000;

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** `DevToolsActivePort`: the port on line 1, the browser path on line 2. */
export function parseDevToolsActivePort(text) {
  const [portLine, path] = String(text).split(/\r?\n/);
  const port = Number(portLine);
  if (!Number.isInteger(port) || port < 1 || port > 65535 || !path?.startsWith("/devtools/")) return null;
  return { port, path };
}

/** Chrome writes it once DevTools listens (`--remote-debugging-port=0`). */
export async function readDevToolsActivePort(userDataDir, timeoutMs = 20_000) {
  const file = join(userDataDir, "DevToolsActivePort");
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const parsed = parseDevToolsActivePort(await readFile(file, "utf8"));
      if (parsed) return parsed;
    } catch {}
    await delay(100);
  }
  throw new Error(`Chrome no escribió ${file} en ${timeoutMs / 1000} s`);
}

/** Centre of a CDP quad `[x1,y1,x2,y2,x3,y3,x4,y4]`. */
export function quadCentre(quad) {
  let x = 0;
  let y = 0;
  for (let index = 0; index < 8; index += 2) {
    x += quad[index];
    y += quad[index + 1];
  }
  return { x: x / 4, y: y / 4 };
}

export class CdpClient {
  /** `endpoint`: http://127.0.0.1:<port> (via /json/version) or a ws:// URL. */
  static async connect(endpoint, { timeoutMs = 10_000 } = {}) {
    let url = endpoint;
    if (/^https?:/.test(endpoint)) {
      const response = await fetch(new URL("/json/version", endpoint), { signal: AbortSignal.timeout(timeoutMs) });
      if (!response.ok) throw new Error(`${endpoint}/json/version devolvió HTTP ${response.status}`);
      url = (await response.json()).webSocketDebuggerUrl;
      if (!url) throw new Error(`${endpoint} no publicó webSocketDebuggerUrl`);
    }
    const socket = new WebSocket(url);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`CDP no respondió en ${url}`)), timeoutMs);
      socket.addEventListener("open", () => { clearTimeout(timer); resolve(); }, { once: true });
      socket.addEventListener("error", () => { clearTimeout(timer); reject(new Error(`No pude abrir CDP en ${url}`)); }, { once: true });
    });
    return new CdpClient(socket);
  }

  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Set();
    this.closed = false;
    socket.addEventListener("message", (event) => this.#dispatch(event.data));
    socket.addEventListener("close", () => {
      this.closed = true;
      for (const { reject, timer } of this.pending.values()) {
        clearTimeout(timer);
        reject(new Error("La conexión CDP se cerró"));
      }
      this.pending.clear();
    });
  }

  #dispatch(data) {
    let message;
    try {
      message = JSON.parse(typeof data === "string" ? data : Buffer.from(data).toString("utf8"));
    } catch {
      return;
    }
    if (message.id !== undefined) {
      const entry = this.pending.get(message.id);
      if (!entry) return;
      this.pending.delete(message.id);
      clearTimeout(entry.timer);
      if (message.error) {
        entry.reject(new Error(`${entry.method}: ${message.error.message}${message.error.data ? ` (${message.error.data})` : ""}`));
      } else {
        entry.resolve(message.result ?? {});
      }
      return;
    }
    for (const listener of [...this.listeners]) {
      if (listener.method !== message.method) continue;
      if (listener.sessionId !== undefined && listener.sessionId !== (message.sessionId ?? null)) continue;
      try {
        listener.handler(message.params ?? {}, message.sessionId ?? null);
      } catch {}
    }
  }

  send(method, params = {}, sessionId = null, { timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
    if (this.closed) return Promise.reject(new Error(`CDP cerrado; no se envió ${method}`));
    const id = this.nextId++;
    const message = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${method}: sin respuesta en ${timeoutMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, method });
      this.socket.send(JSON.stringify(message));
    });
  }

  /**
   * Subscribes to an event and returns the unsubscribe function. `sessionId`
   * null means browser-level events, undefined means any session.
   */
  on(method, handler, sessionId = undefined) {
    const listener = { method, handler, sessionId };
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  waitForEvent(method, { sessionId = undefined, predicate = () => true, timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        off();
        reject(new Error(`No llegó ${method} en ${timeoutMs} ms`));
      }, timeoutMs);
      const off = this.on(method, (params) => {
        if (!predicate(params)) return;
        clearTimeout(timer);
        off();
        resolve(params);
      }, sessionId);
    });
  }

  async targets() {
    return (await this.send("Target.getTargets")).targetInfos;
  }

  async waitForTarget(predicate, { timeoutMs = DEFAULT_TIMEOUT_MS, intervalMs = 250 } = {}) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const target = (await this.targets()).find(predicate);
      if (target) return target;
      await delay(intervalMs);
    }
    throw new Error(`No apareció el target esperado en ${timeoutMs} ms`);
  }

  async attach(targetId) {
    const { sessionId } = await this.send("Target.attachToTarget", { targetId, flatten: true });
    return new CdpSession(this, sessionId, targetId);
  }

  close() {
    if (!this.closed) this.socket.close();
  }
}

export class CdpSession {
  constructor(client, sessionId, targetId) {
    this.client = client;
    this.sessionId = sessionId;
    this.targetId = targetId;
  }

  send(method, params = {}, options) {
    return this.client.send(method, params, this.sessionId, options);
  }

  on(method, handler) {
    return this.client.on(method, handler, this.sessionId);
  }

  waitForEvent(method, options = {}) {
    return this.client.waitForEvent(method, { ...options, sessionId: this.sessionId });
  }

  async detach() {
    await this.client.send("Target.detachFromTarget", { sessionId: this.sessionId }).catch(() => {});
  }
}

function exceptionText(details) {
  return details?.exception?.description || details?.exception?.value || details?.text || "excepción en la página";
}

/** Runtime.evaluate returning the value; throws on page exceptions. */
export async function evaluate(session, expression, {
  contextId,
  awaitPromise = true,
  returnByValue = true,
  userGesture = false,
  timeoutMs,
} = {}) {
  const params = { expression, awaitPromise, returnByValue, userGesture };
  if (contextId !== undefined) params.contextId = contextId;
  const { result, exceptionDetails } = await session.send("Runtime.evaluate", params, timeoutMs ? { timeoutMs } : undefined);
  if (exceptionDetails) throw new Error(exceptionText(exceptionDetails));
  return returnByValue ? result.value : result;
}

/** Like `evaluate`, but returns the objectId of an element (or null). */
export async function evaluateHandle(session, expression, options = {}) {
  const result = await evaluate(session, expression, { ...options, returnByValue: false });
  return result?.objectId && result.subtype !== "null" ? result.objectId : null;
}

export async function callFunction(session, objectId, functionDeclaration, args = [], { returnByValue = true, awaitPromise = true } = {}) {
  const { result, exceptionDetails } = await session.send("Runtime.callFunctionOn", {
    objectId,
    functionDeclaration,
    arguments: args.map((value) => ({ value })),
    returnByValue,
    awaitPromise,
  });
  if (exceptionDetails) throw new Error(exceptionText(exceptionDetails));
  if (returnByValue) return result.value;
  return result?.objectId && result.subtype !== "null" ? result.objectId : null;
}

export async function releaseObject(session, objectId) {
  if (objectId) await session.send("Runtime.releaseObject", { objectId }).catch(() => {});
}

/**
 * Tracks the default (page) execution context of every frame of a page
 * session, so code can run inside a same-process iframe as if it were the
 * top document there.
 */
export class FrameContexts {
  static async attach(session) {
    const tracker = new FrameContexts(session);
    session.on("Runtime.executionContextCreated", ({ context }) => {
      if (context.auxData?.isDefault && context.auxData.frameId) {
        tracker.byFrame.set(context.auxData.frameId, context.id);
      }
    });
    session.on("Runtime.executionContextDestroyed", ({ executionContextId }) => {
      for (const [frameId, id] of tracker.byFrame) {
        if (id === executionContextId) tracker.byFrame.delete(frameId);
      }
    });
    session.on("Runtime.executionContextsCleared", () => tracker.byFrame.clear());
    await session.send("Page.enable");
    await session.send("Runtime.enable");
    await session.send("DOM.enable");
    return tracker;
  }

  constructor(session) {
    this.session = session;
    this.byFrame = new Map();
  }

  async frames() {
    const { frameTree } = await this.session.send("Page.getFrameTree");
    const list = [];
    const walk = (node) => {
      list.push({ id: node.frame.id, url: node.frame.url, parentId: node.frame.parentId || null });
      for (const child of node.childFrames || []) walk(child);
    };
    walk(frameTree);
    return list;
  }

  /** Default context of the first frame matching `pattern` by URL, or null. */
  async contextFor(pattern) {
    for (const frame of await this.frames()) {
      if (pattern.test(frame.url) && this.byFrame.has(frame.id)) return this.byFrame.get(frame.id);
    }
    return null;
  }
}

/** Real left click (mouse move, press, release) at viewport coordinates. */
export async function mouseClick(session, x, y) {
  await session.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, button: "none", buttons: 0 });
  await session.send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", buttons: 1, clickCount: 1 });
  await session.send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", buttons: 0, clickCount: 1 });
}

/**
 * Centre of a node in top-level viewport coordinates. For nodes inside a
 * same-process iframe Chrome already reports the box in root-frame
 * coordinates (verified by the offline self-test), so no offset is added.
 */
export async function nodeCentre(session, { objectId, nodeId, backendNodeId }) {
  const ref = objectId ? { objectId } : nodeId ? { nodeId } : { backendNodeId };
  await session.send("DOM.scrollIntoViewIfNeeded", ref).catch(() => {});
  const { model } = await session.send("DOM.getBoxModel", ref);
  return quadCentre(model.border);
}

export async function clickNode(session, ref) {
  const { x, y } = await nodeCentre(session, ref);
  await mouseClick(session, x, y);
  return { x, y };
}

/**
 * Queries inside a shadow root, including `mode: "closed"` roots, which page
 * scripts cannot reach. Returns the objectId of the match, or null.
 */
export async function shadowQuery(session, hostObjectId, selector) {
  const { node } = await session.send("DOM.describeNode", { objectId: hostObjectId, depth: 1, pierce: true });
  const root = node.shadowRoots?.[0];
  if (!root) return null;
  const { object } = await session.send("DOM.resolveNode", { backendNodeId: root.backendNodeId });
  try {
    return await callFunction(session, object.objectId, "function (selector) { return this.querySelector(selector); }", [selector], { returnByValue: false });
  } finally {
    await releaseObject(session, object.objectId);
  }
}

/**
 * Page-side finder: the first visible element matching CSS `selector` and,
 * optionally, a text and/or aria-label/title regex. Serialised into
 * Runtime.evaluate so it runs in whichever frame context is chosen.
 */
function findElementInPage(spec) {
  const clean = (value) => String(value || "").replace(/\s+/g, " ").trim();
  const text = spec.text ? new RegExp(spec.text[0], spec.text[1]) : null;
  const label = spec.label ? new RegExp(spec.label[0], spec.label[1]) : null;
  const visible = (element) => {
    if (spec.includeHidden) return true;
    const rect = element.getBoundingClientRect();
    if (rect.width < 1 || rect.height < 1) return false;
    const style = getComputedStyle(element);
    return style.visibility !== "hidden" && style.display !== "none";
  };
  for (const element of document.querySelectorAll(spec.selector || "*")) {
    if (text && !text.test(clean(element.textContent))) continue;
    if (label && !label.test(clean(element.getAttribute("aria-label") || element.getAttribute("title")))) continue;
    if (visible(element)) return element;
  }
  return null;
}

const regexSpec = (value) => (value instanceof RegExp ? [value.source, value.flags] : value ? [String(value), ""] : null);

/** objectId of the first match for `{ selector, text, label }`, or null. */
export async function findElement(session, { contextId, selector, text, label, includeHidden = false } = {}) {
  const spec = { selector, text: regexSpec(text), label: regexSpec(label), includeHidden };
  return evaluateHandle(session, `(${findElementInPage})(${JSON.stringify(spec)})`, { contextId });
}

export async function waitFor(check, { timeoutMs = DEFAULT_TIMEOUT_MS, intervalMs = 500, description = "la condición" } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await delay(intervalMs);
  }
  throw new Error(`Tiempo agotado esperando ${description}${lastError ? ` (último error: ${lastError.message})` : ""}`);
}
