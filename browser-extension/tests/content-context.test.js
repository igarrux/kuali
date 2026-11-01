import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

test("an invalidated extension context cannot escape the content-script bridge", async () => {
  const listeners = new Map();
  const window = {
    top: null,
    addEventListener(type, listener) {
      listeners.set(type, listener);
    },
    postMessage() {},
  };
  window.top = window;

  let invalidated = false;
  const chrome = {
    runtime: {
      id: "kuali-test",
      onMessage: { addListener() {} },
      getURL(path) {
        return `chrome-extension://kuali-test/${path}`;
      },
      sendMessage() {
        if (invalidated) throw new Error("Extension context invalidated.");
        return Promise.resolve({ status: "idle" });
      },
    },
  };
  const source = readFileSync(new URL("../src/content.js", import.meta.url), "utf8");
  vm.runInNewContext(source, {
    chrome,
    console,
    document: { title: "Test meeting" },
    location: {
      hostname: "meet.google.com",
      pathname: "/abc-defg-hij",
      href: "https://meet.google.com/abc-defg-hij",
    },
    Promise,
    Set,
    URL,
    window,
  });

  invalidated = true;
  assert.doesNotThrow(() => listeners.get("message")({
    source: window,
    data: {
      protocol: "kuali.capture.v1",
      type: "audio-frame",
    },
  }));

  // Also let the swallowed Promise settle: no unhandled rejection should remain
  // after the listener returns.
  await Promise.resolve();
});

function frameReady(hostname, pathname, isTop) {
  const sent = [];
  const window = { addEventListener() {}, postMessage() {} };
  window.top = isTop ? window : {};
  const chrome = {
    runtime: {
      id: "kuali-test",
      onMessage: { addListener() {} },
      getURL: (path) => `chrome-extension://kuali-test/${path}`,
      sendMessage(message) {
        sent.push(message);
        return Promise.resolve(null);
      },
    },
  };
  const source = readFileSync(new URL("../src/content.js", import.meta.url), "utf8");
  vm.runInNewContext(source, {
    chrome,
    console,
    document: { title: "Test meeting" },
    location: { hostname, pathname, href: `https://${hostname}${pathname}` },
    Promise,
    Set,
    URL,
    window,
  });
  return sent.find((message) => message.type === "frame-ready");
}

test("the isolated content script detects platforms like capture-policy.js", async () => {
  await import("../src/capture-policy.js");
  const policy = globalThis.KualiCapturePolicy;
  const manifest = JSON.parse(readFileSync(new URL("../manifest.json", import.meta.url), "utf8"));
  const hosts = manifest.content_scripts[1].matches.map((pattern) => new URL(pattern.replace("*.", "x.")).hostname);
  for (const hostname of hosts) {
    const pathname = "/private-join/00000000000000";
    const ready = frameReady(hostname, pathname, true);
    assert.equal(ready.platform, policy.meetingPlatformForHost(hostname), hostname);
    assert.equal(ready.primary, policy.isPrimaryCaptureFrame(ready.platform, pathname, true), hostname);
  }
});

test("only the Telemost call iframe announces itself as the primary frame", () => {
  assert.equal(frameReady("telemost.yandex.ru", "/j/00000000000000", true).primary, false);
  assert.equal(frameReady("telemost.yandex.ru", "/j/00000000000000", true).meetingId, "00000000000000");
  assert.equal(frameReady("telemost.yandex.ru", "/private-join/00000000000000", false).primary, true);
  assert.equal(frameReady("meet.google.com", "/abc-defg-hij", false).primary, false);
  assert.equal(frameReady("meet.google.com", "/abc-defg-hij", true).primary, true);
});
