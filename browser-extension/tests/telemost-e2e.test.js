import assert from "node:assert/strict";
import test from "node:test";
import { parseDevToolsActivePort, quadCentre } from "../e2e/cdp.mjs";
import {
  PHRASES,
  activeSpeakerNames,
  applyProbeLine,
  muteWindow,
  nextSpeechStart,
  participantLeft,
  pcm16Wav,
  readPcm16Wav,
  trimSilence,
  voiceSchedule,
} from "../e2e/run-telemost-e2e.mjs";

test("voices take disjoint slots of one wall-clock cycle", () => {
  const schedule = voiceSchedule({ a: 4_062, b: 3_511, recorder: 3_463 }, 1_000_000);
  assert.equal(schedule.slotMs, 7_600);
  assert.equal(schedule.cycleMs, 22_800);
  assert.deepEqual(schedule.offsets, { a: 0, b: 7_600, recorder: 15_200 });
  for (const role of ["a", "b"]) {
    assert(schedule.offsets[role] + schedule.speechMs[role] + 3_500 <= schedule.offsets[role] + schedule.slotMs);
  }
  assert.equal(nextSpeechStart(schedule, "b", 1_000_000), 1_007_600);
  assert.equal(nextSpeechStart(schedule, "b", 1_007_601), 1_030_400);
  assert.equal(nextSpeechStart(schedule, "a", 900_000), 1_000_000);
});

test("expected terms never open their phrase", () => {
  for (const { text, term } of Object.values(PHRASES)) {
    const position = text.toLowerCase().indexOf(term);
    assert(position > 0, `${term} must appear after the first word of «${text}»`);
  }
});

test("trimmed speech keeps its sample rate and loses the silence", () => {
  const sampleRate = 1_000;
  const pcm = Buffer.alloc(3_000 * 2);
  for (let index = 1_000; index < 1_500; index += 1) pcm.writeInt16LE(index % 2 ? 8_000 : -8_000, index * 2);
  const wav = readPcm16Wav(pcm16Wav(pcm, sampleRate));
  assert.equal(wav.sampleRate, sampleRate);
  const speech = trimSilence(wav.pcm, sampleRate, { padMs: 10 });
  assert.equal(speech.length / 2, 500 + 2 * 10);
  assert.throws(() => trimSilence(Buffer.alloc(100), sampleRate), /no contiene voz/);
});

test("mute windows tolerate slack and need speech before and after", () => {
  const seconds = [100, 101, 103, 104, 125, 140, 141];
  const window = muteWindow(seconds, 105_000, 130_000, { slackMs: 2_000, contextMs: 20_000 });
  assert.deepEqual(window.inside, [125]);
  assert.equal(window.before, 4);
  assert.equal(window.after, 2);
  assert.deepEqual(muteWindow([106, 128], 105_000, 130_000).inside, []);
});

test("a departure is found by channel, id, name or an updated roster", () => {
  const timeline = [
    { tsMs: 1_000, kind: "participant-left", channel: 4 },
    { tsMs: 5_000, kind: "participant-upsert", channel: 4, displayName: "Борис Тестовый" },
    { tsMs: 6_000, kind: "roster-state", participants: [{ displayName: "Анна Тестовая" }] },
    { tsMs: 7_000, kind: "participant-left", participantId: "p-b" },
  ];
  const search = { channels: [4], participantIds: ["p-b"], name: "Борис Тестовый" };
  assert.equal(participantLeft(timeline, { ...search, sinceMs: 2_000 }).kind, "roster-state");
  assert.equal(participantLeft(timeline, { ...search, sinceMs: 6_500 }).participantId, "p-b");
  assert.equal(participantLeft(timeline, { ...search, sinceMs: 8_000 }), null);
  assert.deepEqual(activeSpeakerNames([
    { kind: "active-speakers", participants: [{ displayName: "Анна Тестовая" }] },
    { kind: "active-speakers", participants: [] },
  ]), ["Анна Тестовая"]);
});

test("probe lines give readiness, the report and each ingest session", () => {
  const state = { ready: null, reportPath: null, ingest: [] };
  assert.deepEqual(applyProbeLine(state, 'KUALI_E2E_READY {"port":19099,"mode":"full"}'), { port: 19099, mode: "full" });
  assert.equal(applyProbeLine(state, "[motor] Listening"), null);
  const session = { index: 0, path: "/ingest?platform=yandex_telemost", openedMs: 10, closeCode: null };
  applyProbeLine(state, `KUALI_E2E_INGEST ${JSON.stringify({ event: "open", connection: session })}`);
  applyProbeLine(state, `KUALI_E2E_INGEST ${JSON.stringify({
    event: "close-frame",
    connection: { ...session, closeCode: 1000, closeFrameMs: 25 },
  })}`);
  applyProbeLine(state, "KUALI_E2E_REPORT /tmp/yandex_telemost-1.json\n");
  assert.equal(state.ingest.length, 1);
  assert.equal(state.ingest[0].closeCode, 1000);
  assert.equal(state.ingest[0].closeFrameMs, 25);
  assert.equal(state.reportPath, "/tmp/yandex_telemost-1.json");
});

test("CDP helpers parse DevToolsActivePort and quads", () => {
  assert.deepEqual(parseDevToolsActivePort("54975\n/devtools/browser/abc\n"), { port: 54975, path: "/devtools/browser/abc" });
  assert.equal(parseDevToolsActivePort("0\n/devtools/browser/abc"), null);
  assert.equal(parseDevToolsActivePort("9222\n"), null);
  assert.deepEqual(quadCentre([10, 20, 30, 20, 30, 40, 10, 40]), { x: 20, y: 30 });
});
