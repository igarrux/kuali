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
