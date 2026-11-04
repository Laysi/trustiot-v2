// The display guard (guard.ts): the frame is visible only while the latest comparison of the data
// sent last found nothing and the runtime keeps reporting; every other report hides it, and the
// closing ones keep it hidden.
import { describe, expect, test } from "bun:test";

import type { PortMessage } from "../../../sandbox/src/protocol.ts";
import { DisplayGuard, type GuardView, MAX_REGRESSIONS } from "./guard.ts";

/** A clock the test advances by hand. */
function fakeClock() {
  let now = 0;
  let next = 1;
  const timers = new Map<number, { at: number; callback: () => void }>();
  return {
    setTimeout(callback: () => void, ms: number) {
      const id = next++;
      timers.set(id, { at: now + ms, callback });
      return id;
    },
    clearTimeout(handle: unknown) {
      timers.delete(handle as number);
    },
    advance(ms: number) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now && timers.has(id)) {
          timers.delete(id);
          timer.callback();
        }
      }
    },
    pending: () => timers.size,
  };
}

const TIMEOUT = 10_000;
const STALE = 1_000;

function setUp(options: { maxRegressions?: number } = {}) {
  const clock = fakeClock();
  const views: GuardView[] = [];
  const guard = new DisplayGuard(clock, (view) => views.push(view), {
    heartbeatTimeoutMs: TIMEOUT,
    staleAfterMs: STALE,
    ...options,
  });
  return { clock, views, guard };
}

const beat = (seq = 1): PortMessage => ({ tq: 1, type: "beat", seq });
const compared = (generation: number, findings = 0): PortMessage => ({ tq: 1, type: "compared", generation, findings });

/** A guard showing generation 1. */
function shown(options: { maxRegressions?: number } = {}) {
  const setup = setUp(options);
  setup.guard.started();
  setup.guard.dataSent(1);
  setup.guard.report(compared(1));
  expect(setup.guard.view.visible).toBe(true);
  return setup;
}

describe("showing", () => {
  test("hidden before init, before data, and before a comparison of the data sent", () => {
    const { guard } = setUp();
    expect(guard.view).toEqual({ visible: false, status: { kind: "loading" }, height: null });
    // A clean comparison before anything was sent shows nothing.
    guard.report(compared(1));
    expect(guard.view.visible).toBe(false);
    guard.started();
    expect(guard.view.visible).toBe(false);
    guard.dataSent(1);
    expect(guard.view.visible).toBe(false);
  });

  test("a clean comparison of the data sent last shows it", () => {
    const { guard, views } = setUp();
    guard.started();
    guard.dataSent(1);
    guard.report(compared(1));
    expect(guard.view.visible).toBe(true);
    expect(views.at(-1)?.visible).toBe(true);
  });

  test("a clean comparison before init does not show it", () => {
    const { guard } = setUp();
    guard.dataSent(1);
    guard.report(compared(1));
    expect(guard.view.visible).toBe(false);
  });

  test("a clean comparison of older data does not show the new data", () => {
    const { guard } = shown();
    guard.dataSent(2);
    expect(guard.view).toMatchObject({ visible: false, status: { kind: "checking" } });
    guard.report(compared(1));
    expect(guard.view.visible).toBe(false);
    // Nor of a generation the outer page never sent.
    guard.report(compared(3));
    expect(guard.view.visible).toBe(false);
    guard.report(compared(2));
    expect(guard.view.visible).toBe(true);
  });

  test("an older generation sent again is ignored", () => {
    const { guard } = shown();
    guard.dataSent(1);
    expect(guard.view.visible).toBe(true);
  });
});

describe("hiding", () => {
  test("a comparison with findings hides it, and a clean one shows it again", () => {
    const { guard } = shown();
    guard.report(compared(1, 2));
    expect(guard.view).toMatchObject({ visible: false, status: { kind: "findings", count: 2 } });
    guard.report(compared(1));
    expect(guard.view.visible).toBe(true);
  });

  test("findings in the first comparison keep it hidden", () => {
    const { guard } = setUp();
    guard.started();
    guard.dataSent(1);
    guard.report(compared(1, 1));
    expect(guard.view).toMatchObject({ visible: false, status: { kind: "findings", count: 1 } });
  });

  test("a page error hides it until a clean comparison", () => {
    const { guard } = shown();
    guard.report({ tq: 1, type: "error", phase: "render", message: "Error: 999.99 kW" });
    // The page's message is not kept: the outer page never shows the page's text.
    expect(guard.view).toEqual({ visible: false, status: { kind: "error" }, height: null });
  });

  test("a page error before the first comparison stays shown as the reason", () => {
    const { guard } = setUp();
    guard.started();
    guard.dataSent(1);
    guard.report({ tq: 1, type: "error", phase: "page" });
    expect(guard.view.status).toEqual({ kind: "error" });
    guard.report(compared(1, 1));
    expect(guard.view.status).toEqual({ kind: "error" });
  });

  test("refused data hides it, even while older data compares clean, until data is sent again", () => {
    const { guard } = shown();
    guard.dataRefused();
    expect(guard.view).toMatchObject({ visible: false, status: { kind: "refused" } });
    guard.report(compared(1));
    expect(guard.view.visible).toBe(false);
    guard.dataSent(2);
    guard.report(compared(2));
    expect(guard.view.visible).toBe(true);
  });

  test("the size the runtime reports follows, without showing it", () => {
    const { guard } = setUp();
    guard.report({ tq: 1, type: "resize", height: 480 });
    expect(guard.view).toMatchObject({ visible: false, height: 480 });
  });
});

describe("closing", () => {
  test("a missed heartbeat closes it for good, whatever else arrives", () => {
    const { guard, clock } = shown();
    // Comparisons keep it shown, but only beats count for the heartbeat.
    for (let at = STALE / 2; at < TIMEOUT; at += STALE / 2) {
      clock.advance(STALE / 2);
      guard.report(compared(1));
    }
    expect(guard.view.visible).toBe(true);
    guard.report(beat(1));
    for (let at = 0; at < TIMEOUT - 1; at += STALE / 2) {
      clock.advance(STALE / 2);
      if (guard.view.status.kind === "closed") break;
      guard.report(compared(1));
    }
    expect(guard.view).toMatchObject({ visible: false, status: { kind: "closed", reason: "stalled" } });
    guard.report(beat(2));
    guard.report(compared(1));
    expect(guard.view.visible).toBe(false);
  });

  test("a gap in the reports hides it until the next comparison, and counts against the page on screen", () => {
    const { guard, clock } = shown({ maxRegressions: 2 });
    for (let round = 0; round < 2; round++) {
      clock.advance(STALE - 1);
      expect(guard.view.visible).toBe(true);
      clock.advance(1);
      expect(guard.view).toMatchObject({ visible: false, status: { kind: "stale" } });
      // A beat says the thread runs again, but not what the page shows: only a comparison does.
      guard.report(beat(round + 1));
      expect(guard.view.visible).toBe(false);
      guard.report(compared(1));
      expect(guard.view.visible).toBe(true);
    }
    clock.advance(STALE);
    expect(guard.view.status).toEqual({ kind: "closed", reason: "unstable" });
  });

  test("a gap while the frame is out of view hides it but is not counted", () => {
    const { guard, clock } = shown({ maxRegressions: 0 });
    guard.setOnScreen(false);
    clock.advance(STALE);
    expect(guard.view.status).toEqual({ kind: "stale" });
    guard.setOnScreen(true);
    guard.report(compared(1));
    expect(guard.view.visible).toBe(true);
  });

  test("the heartbeat is counted from init, before any comparison", () => {
    const { guard, clock } = setUp();
    expect(clock.pending()).toBe(0);
    guard.started();
    clock.advance(TIMEOUT);
    expect(guard.view.status).toEqual({ kind: "closed", reason: "stalled" });
  });

  test("while the outer page is hidden nothing is counted; shown again, it waits for a comparison and gets full timeouts", () => {
    const { guard, clock } = shown({ maxRegressions: 0 });
    guard.pause();
    clock.advance(TIMEOUT * 5);
    expect(guard.view.visible).toBe(true);
    guard.resume();
    expect(guard.view).toMatchObject({ visible: false, status: { kind: "stale" } });
    guard.report(compared(1));
    expect(guard.view.visible).toBe(true);
    for (let at = 0; at < TIMEOUT - 1; at += STALE / 2) {
      clock.advance(STALE / 2);
      if (guard.view.status.kind === "closed") break;
      guard.report(compared(1));
    }
    expect(guard.view.status).toEqual({ kind: "closed", reason: "stalled" });
  });

  test("a failed report closes it with its reason", () => {
    for (const reason of ["comparison", "corrections", "unnotified"] as const) {
      const { guard } = shown();
      guard.report({ tq: 1, type: "failed", reason });
      expect(guard.view.status).toEqual({ kind: "closed", reason });
      guard.report(compared(1));
      expect(guard.view.visible).toBe(false);
    }
  });

  test("a navigation closes it", () => {
    const { guard, clock } = shown();
    guard.navigated();
    expect(guard.view.status).toEqual({ kind: "closed", reason: "navigated" });
    expect(clock.pending()).toBe(0);
    guard.dataSent(2);
    guard.report(compared(2));
    expect(guard.view.visible).toBe(false);
  });

  test("findings after it was shown close it once they pass the limit", () => {
    const { guard } = shown({ maxRegressions: 3 });
    for (let round = 0; round < 3; round++) {
      guard.report(compared(1, 1));
      expect(guard.view.visible).toBe(false);
      guard.report(compared(1));
      expect(guard.view.visible).toBe(true);
    }
    guard.report(compared(1, 1));
    expect(guard.view.status).toEqual({ kind: "closed", reason: "unstable" });
  });

  test("the default limit is MAX_REGRESSIONS", () => {
    const { guard } = shown();
    for (let round = 0; round < MAX_REGRESSIONS; round++) {
      guard.report(compared(1, 1));
      guard.report(compared(1));
    }
    expect(guard.view.visible).toBe(true);
    guard.report(compared(1, 1));
    expect(guard.view.status).toEqual({ kind: "closed", reason: "unstable" });
  });

  test("dispose stops the heartbeat timer", () => {
    const { guard, clock } = shown();
    guard.dispose();
    expect(clock.pending()).toBe(0);
    guard.report(beat());
    expect(clock.pending()).toBe(0);
  });
});
