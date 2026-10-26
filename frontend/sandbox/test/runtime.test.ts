// The sandbox runtime (frontend/sandbox/README.md, 「頁面」, 「頁面 API」, 「畫完」 and
// 「postMessage 協定」) in a happy-dom window of its own.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { Window } from "happy-dom";

import type { DomWindow } from "../src/dom.ts";
import type { CheckRequest, DatasetResult, SandboxMessage } from "../src/protocol.ts";
import { describeError, installSandbox, MAX_CORRECTION_ROUNDS, timeOf, trustedEvents } from "../src/runtime.ts";
import { dataMessage, type Frame, initMessage, openFrame } from "./harness.ts";

const BASE = JSON.parse(readFileSync(new URL("../fixtures/request.json", import.meta.url), "utf8")) as Omit<
  CheckRequest,
  "page"
>;

/** How long a test waits for a message the runtime sends right away; it fails naming the message. */
const WAIT_MS = 30_000;
const TEST_MS = 60_000;

function request(page: string, change: (request: CheckRequest) => void = () => {}): CheckRequest {
  const built = structuredClone({ ...BASE, page }) as CheckRequest;
  change(built);
  return built;
}

async function expectMessage(
  frame: Frame,
  what: string,
  accept: (message: SandboxMessage) => boolean,
  from = 0,
): Promise<SandboxMessage> {
  const message = await frame.next(accept, WAIT_MS, from);
  if (message === null) throw new Error(`no ${what} within ${WAIT_MS} ms; got ${JSON.stringify(frame.messages)}`);
  return message;
}

/** Waits until `condition` holds, checking every few milliseconds; fails naming what it awaited. */
async function waitFor(what: string, condition: () => boolean): Promise<void> {
  const deadline = Date.now() + WAIT_MS;
  while (!condition()) {
    if (Date.now() > deadline) throw new Error(`no ${what} within ${WAIT_MS} ms`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

const rendered = (generation: number) => (message: SandboxMessage) =>
  message.type === "rendered" && message.generation === generation;

/** Runs `page` in a fresh frame until generation 1 is drawn (or failed), then hands the frame to `use`. */
async function withPage(
  built: CheckRequest,
  use: (frame: Frame, win: DomWindow & Record<string, unknown>) => Promise<void> | void,
  mode: "check" | "view" = "check",
  results: unknown = dataMessage(built.datasets),
) {
  const frame = await openFrame();
  try {
    frame.runtime.receive(initMessage(built, mode));
    frame.runtime.receive(results);
    await use(frame, frame.window as DomWindow & Record<string, unknown>);
  } finally {
    await frame.close();
  }
}

describe("loading the page", () => {
  test(
    "runs only the classic inline scripts, in order and strict, and keeps the rest of the markup",
    async () => {
      const page = `<p id="a">文字</p>
<script>window.order = ["first"]; window.strict = this === undefined;</script>
<script type="module">window.order.push("module");</script>
<script src="other.js">window.order.push("src");</script>
<script type=" Text/JavaScript ">window.order.push("second");</script>
<script type="text/plain">window.order.push("plain");</script>
<script>tq.on("data", () => {});</script>`;
      await withPage(request(page), async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        expect(win.order).toEqual(["first", "second"]);
        expect(win.strict).toBe(true);
        expect(win.document.getElementsByTagName("script").length).toBe(0);
        expect(win.document.getElementById("a")?.parentElement?.id).toBe("tq-root");
        expect(frame.messages[0]).toEqual({ tq: 1, type: "ready", runtime: 1 });
      });
    },
    TEST_MS,
  );

  test(
    "takes one init, ignores data before it, and ignores a malformed init",
    async () => {
      const frame = await openFrame();
      try {
        const page = `<script>window.runs = (window.runs ?? 0) + 1; tq.on("data", () => {});</script>`;
        const built = request(page);
        frame.runtime.receive(dataMessage(built.datasets));
        frame.runtime.receive({ ...initMessage(built), extra: true });
        frame.runtime.receive({ ...initMessage(built), mode: "edit" });
        const win = frame.window as DomWindow & Record<string, unknown>;
        expect(win.runs).toBeUndefined();
        frame.runtime.receive(initMessage(built));
        frame.runtime.receive(initMessage(built));
        expect(win.runs).toBe(1);
        frame.runtime.receive(dataMessage(built.datasets));
        await expectMessage(frame, "rendered", rendered(1));
        expect(frame.messages.filter((message) => message.type === "ready").length).toBe(1);
      } finally {
        await frame.close();
      }
    },
    TEST_MS,
  );

  test(
    "a script that throws stops the page and reports its text without hidden characters",
    async () => {
      const page = `<script>throw new Error("bad\\u200Bthing\\u0000");</script><script>window.later = true;</script>`;
      await withPage(request(page), async (frame, win) => {
        const error = await expectMessage(frame, "error", (message) => message.type === "error");
        expect(error).toEqual({ tq: 1, type: "error", phase: "page", message: "Error: bad thing" });
        expect(win.later).toBeUndefined();
      });
    },
    TEST_MS,
  );
});

describe("describeError", () => {
  test("names the error, cuts long text and gives nothing for what has no readable text", () => {
    expect(describeError(new TypeError("x"))).toBe("TypeError: x");
    expect(describeError("y".repeat(300))).toBe("y".repeat(200));
    expect(describeError("​\u0007")).toBeUndefined();
    const hostile = {
      get message(): string {
        throw new Error("no");
      },
    };
    expect(describeError(hostile)).toBeUndefined();
  });
});

describe("rendering", () => {
  test(
    "returned comes before rendered, and each new generation draws again; older ones are ignored",
    async () => {
      const page = `<p id="n"></p><script>let count = 0; tq.on("data", () => { count++; document.getElementById("n").dataset.count = String(count); });</script>`;
      const built = request(page);
      await withPage(built, async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        const types = frame.messages.map((message) => message.type);
        expect(types.indexOf("returned")).toBeLessThan(types.indexOf("rendered"));
        frame.runtime.receive(dataMessage(built.datasets, 1));
        frame.runtime.receive(dataMessage(built.datasets, 3));
        await expectMessage(frame, "rendered 3", rendered(3));
        frame.runtime.receive(dataMessage(built.datasets, 2));
        expect(win.document.getElementById("n")?.getAttribute("data-count")).toBe("2");
        expect(frame.messages.some((message) => message.type === "returned" && message.generation === 2)).toBe(false);
      });
    },
    TEST_MS,
  );

  test(
    "a render that throws or rejects reports a render error",
    async () => {
      const throwing = request(`<script>tq.on("data", () => { throw new RangeError("no data"); });</script>`);
      await withPage(throwing, async (frame) => {
        const error = await expectMessage(frame, "error", (message) => message.type === "error");
        expect(error).toEqual({ tq: 1, type: "error", phase: "render", message: "RangeError: no data" });
      });
      const rejecting = request(`<script>tq.on("data", async () => { throw new Error("later"); });</script>`);
      await withPage(rejecting, async (frame) => {
        const error = await expectMessage(frame, "error", (message) => message.type === "error");
        expect(error).toEqual({ tq: 1, type: "error", phase: "render", message: "Error: later" });
        expect(frame.messages.some((message) => message.type === "returned")).toBe(false);
      });
    },
    TEST_MS,
  );

  test(
    "a render that returns something whose then or constructor throws reports a render error",
    async () => {
      const pages = [
        `<script>tq.on("data", () => ({ get then() { throw new Error("no then"); } }));</script>`,
        `<script>tq.on("data", () => { const p = Promise.resolve(); Object.defineProperty(p, "constructor", { get() { throw new Error("no constructor"); } }); return p; });</script>`,
        `<script>tq.on("data", () => Object.assign(() => {}, { then(_ok, fail) { fail(new Error("a function thenable")); } }));</script>`,
      ];
      const messages = ["Error: no then", "Error: no constructor", "Error: a function thenable"];
      for (const [i, page] of pages.entries()) {
        await withPage(request(page), async (frame) => {
          const error = await expectMessage(frame, "error", (message) => message.type === "error");
          expect(error).toEqual({ tq: 1, type: "error", phase: "render", message: messages[i] as string });
          expect(frame.messages.some((message) => message.type === "returned")).toBe(false);
        });
      }
    },
    TEST_MS,
  );

  test(
    "a render function registered after the data came draws that data",
    async () => {
      const page = `<script>setTimeout(() => tq.on("data", (tq) => { window.seen = tq.value("k|line_name"); }), 0);</script>`;
      await withPage(request(page), async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        expect(win.seen).toBe("Line 2");
      });
    },
    TEST_MS,
  );

  test(
    "a new render function draws the current data again; the same one registered again does not",
    async () => {
      const page = `<script>
let calls = 0;
const first = () => { calls++; window.firstCalls = calls; tq.on("data", first); };
tq.on("data", first);
window.replace = () => tq.on("data", () => { window.secondSaw = tq.value("k|line_name"); });
</script>`;
      await withPage(request(page), async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        expect(win.firstCalls).toBe(1);
        const from = frame.messages.length;
        (win.replace as () => void)();
        await expectMessage(frame, "rendered again", rendered(1), from);
        expect(win.secondSaw).toBe("Line 2");
        expect(win.firstCalls).toBe(1);
        expect(frame.messages.slice(from).filter((message) => message.type === "returned")).toEqual([
          { tq: 1, type: "returned", generation: 1 },
        ]);
      });
    },
    TEST_MS,
  );

  test(
    "a newer generation stops the older one's wait for quiet",
    async () => {
      // Generation 1 keeps changing the page; generation 2 stops that and settles.
      const page = `<p id="n"></p><script>
let ticker = null;
let ticks = 0;
tq.on("data", () => {
  if (ticker === null) ticker = setInterval(() => { document.getElementById("n").dataset.tick = String(++ticks); }, 20);
  else clearInterval(ticker);
});
</script>`;
      const live = new Set<unknown>();
      const built = request(page);
      const frame = await openFrame((win) => {
        const prototype = win.MutationObserver.prototype;
        const observe = prototype.observe;
        const disconnect = prototype.disconnect;
        Object.defineProperty(prototype, "observe", {
          value(this: MutationObserver, ...args: Parameters<MutationObserver["observe"]>) {
            live.add(this);
            return Reflect.apply(observe, this, args);
          },
          configurable: true,
          writable: true,
        });
        Object.defineProperty(prototype, "disconnect", {
          value(this: MutationObserver) {
            live.delete(this);
            return Reflect.apply(disconnect, this, []);
          },
          configurable: true,
          writable: true,
        });
      });
      try {
        frame.runtime.receive(initMessage(built));
        frame.runtime.receive(dataMessage(built.datasets, 1));
        await expectMessage(frame, "returned 1", (message) => message.type === "returned" && message.generation === 1);
        // Generation 1 is waiting for a quiet that its interval never gives.
        await waitFor("a tick", () => (frame.window.document.getElementById("n")?.dataset.tick ?? "") !== "");
        frame.runtime.receive(dataMessage(built.datasets, 2));
        await expectMessage(frame, "rendered 2", rendered(2));
        expect(frame.messages.some(rendered(1))).toBe(false);
        expect(live.size).toBe(0);
      } finally {
        await frame.close();
      }
    },
    TEST_MS,
  );

  test(
    "data that does not name every declared dataset exactly once is ignored",
    async () => {
      const built = request(`<script>tq.on("data", () => { window.drawn = true; });</script>`);
      const frame = await openFrame();
      try {
        frame.runtime.receive(initMessage(built));
        const [first, second] = built.datasets as [DatasetResult, DatasetResult, DatasetResult];
        frame.runtime.receive(dataMessage([first, second]));
        frame.runtime.receive(dataMessage([first, second, first]));
        expect((frame.window as DomWindow & Record<string, unknown>).drawn).toBeUndefined();
        frame.runtime.receive(dataMessage(built.datasets));
        await expectMessage(frame, "rendered", rendered(1));
      } finally {
        await frame.close();
      }
    },
    TEST_MS,
  );
});

describe("the page API", () => {
  test(
    "datasets, values, formats and the init settings",
    async () => {
      const page = `<script>
tq.on("data", (tq) => {
  const daily = tq.dataset("daily_energy");
  const failed = tq.dataset("hourly_power");
  const errors = {};
  for (const [name, run] of Object.entries({
    reference: () => tq.value("not a reference"),
    dataset: () => tq.dataset("nope"),
    format: () => tq.format(1, "9>20,.1f"),
    value: () => tq.format({}),
    event: () => tq.on("click", () => {}),
  })) {
    try { run(); } catch (error) { errors[name] = error.name; }
  }
  window.result = {
    daily: {
      name: daily.name, title: daily.title, source: daily.source, status: daily.status, rowCount: daily.rowCount,
      columns: daily.columns, firstDay: daily.rows[0].day.toISOString(), row: daily.rows[2].$row, empty: daily.rows[3].kwh,
      queried: daily.meta.queried_at.toISOString(), metaRows: daily.meta.row_count, range: daily.meta.range_from,
    },
    failed: { status: failed.status, messageKey: failed.messageKey, rows: failed.rows.length, queried: failed.meta.queried_at },
    usageRange: tq.dataset("meter_usage").meta.range_from.toISOString(),
    values: [tq.value("f|week_kwh"), tq.value("c|daily_energy|kwh|99"), tq.value("k|line_name"), tq.value("f|first_day_share")],
    formats: [tq.format(1234.5), tq.format(1234.5, ",.0f"), tq.format(null), tq.format(true), tq.format(new Date(Date.UTC(2026, 9, 20))), tq.format(0.25, ".0%")],
    settings: [tq.locale, tq.tz, tq.theme, tq.mode === undefined],
    frozen: Object.isFrozen(tq),
    errors,
  };
});
</script>`;
      const built = request(page);
      const results = dataMessage(built.datasets);
      results.datasets[1] = { status: "error", dataset: "hourly_power", message_key: "assistant.dataset.timeout" };
      await withPage(
        built,
        async (frame, win) => {
          await expectMessage(frame, "rendered", rendered(1));
          expect(win.result).toEqual({
            daily: {
              name: "daily_energy",
              title: "每日用電",
              source: "sql",
              status: "ok",
              rowCount: 7,
              columns: [
                { name: "day", type: "time" },
                { name: "kwh", type: "number" },
              ],
              firstDay: "2026-10-20T00:00:00.000Z",
              row: 2,
              empty: null,
              queried: "2026-10-26T09:02:03.456Z",
              metaRows: 7,
              range: null,
            },
            failed: { status: "error", messageKey: "assistant.dataset.timeout", rows: 0, queried: null },
            usageRange: "2026-10-19T00:00:00.000Z",
            values: [690, null, "Line 2", 120.5 / 690],
            formats: ["1,234.5", "1,235", "—", "是", "2026-10-20", "25%"],
            settings: ["zh-TW", "Asia/Taipei", "light", true],
            frozen: true,
            errors: {
              reference: "TypeError",
              dataset: "Error",
              format: "FormatError",
              value: "TypeError",
              event: "TypeError",
            },
          });
        },
        "check",
        results,
      );
    },
    TEST_MS,
  );

  test(
    "text, mark, decor and axis write their attributes; text is never parsed as markup",
    async () => {
      const page = `<p id="html"></p><p id="week"></p><svg><g id="bars"><rect id="bar"></rect><rect id="bar2"></rect></g><line id="grid"></line><g id="band"></g><g id="linear"></g><g id="time"></g></svg>
<script>
tq.on("data", (tq) => {
  tq.text(document.getElementById("html"), "k|markup");
  tq.text(document.getElementById("week"), "f|week_kwh", ",.1f");
  tq.mark(d3.selectAll("#bar, #bar2").data([0, 1]), (row) => "c|daily_energy|kwh|" + row, { chart: "daily", channel: "height" });
  tq.decor(document.getElementById("grid"));
  const days = tq.dataset("daily_energy").rows.map((row) => row.day);
  const band = d3.scaleBand([...days.slice(0, 2), new Date(Date.UTC(2030, 0, 1))], [0, 300]);
  tq.axis(document.getElementById("band"), d3.axisBottom(band), { dataset: "daily_energy", column: "day", format: "%m/%d" });
  tq.axis(document.getElementById("linear"), d3.axisLeft(d3.scaleLinear([0, 150], [200, 0])).ticks(3), { dataset: "daily_energy", column: "kwh" });
  const hours = tq.dataset("hourly_power").rows.map((row) => row.hour);
  tq.axis(document.getElementById("time"), d3.axisBottom(d3.scaleUtc([hours[0], hours[hours.length - 1]], [0, 300])).ticks(2), { dataset: "hourly_power", column: "hour", format: "%H:%M" });
  const errors = [];
  for (const run of [
    () => tq.mark(document.getElementById("bar"), "c|daily_energy|kwh|0", { chart: "Daily", channel: "height" }),
    () => tq.mark(document.getElementById("bar"), "c|daily_energy|kwh|0", { chart: "daily", channel: "depth" }),
    () => tq.mark(document.getElementById("bar"), (row) => "c|daily_energy|kwh|0", { chart: "daily", channel: "height" }),
    () => tq.text({}, "k|line_name"),
    () => tq.axis(document.getElementById("band"), {}, { dataset: "daily_energy", column: "day" }),
    () => tq.axis(document.getElementById("band"), d3.axisBottom(band), { dataset: "daily_energy", column: "nope" }),
  ]) {
    try { run(); errors.push("none"); } catch (error) { errors.push(error.name); }
  }
  window.errors = errors;
});
</script>`;
      const built = request(page, (changed) => {
        changed.spec.constants.push({ name: "markup", title: "Markup", value: "<img src=x onerror=alert(1)>" });
      });
      await withPage(built, async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        const doc = win.document;
        const html = doc.getElementById("html");
        expect(html?.textContent).toBe("<img src=x onerror=alert(1)>");
        expect(html?.children.length).toBe(0);
        expect(html?.getAttribute("data-tq")).toBe("k|markup");
        const week = doc.getElementById("week");
        expect([week?.textContent, week?.getAttribute("data-tq"), week?.getAttribute("data-tq-format")]).toEqual([
          "690.0",
          "f|week_kwh",
          ",.1f",
        ]);
        const bar = doc.getElementById("bar2");
        expect([
          bar?.getAttribute("data-tq"),
          bar?.getAttribute("data-tq-chart"),
          bar?.getAttribute("data-tq-channel"),
        ]).toEqual(["c|daily_energy|kwh|1", "daily", "height"]);
        expect(doc.getElementById("grid")?.hasAttribute("data-tq-decor")).toBe(true);
        const ticks = (id: string) =>
          [...(doc.getElementById(id)?.querySelectorAll(".tick text") ?? [])].map((text) => [
            text.textContent,
            text.getAttribute("data-tq"),
          ]);
        expect(doc.getElementById("band")?.getAttribute("data-tq-axis")).toBe("daily_energy|day");
        // A category that is in no row gets no reference: its text would show an unmarked number.
        expect(ticks("band")).toEqual([
          ["10/20", "c|daily_energy|day|0"],
          ["10/21", "c|daily_energy|day|1"],
          ["01/01", null],
        ]);
        expect(ticks("linear")).toEqual([
          ["0", "t|daily_energy|kwh|0"],
          ["50", "t|daily_energy|kwh|50"],
          ["100", "t|daily_energy|kwh|100"],
          ["150", "t|daily_energy|kwh|150"],
        ]);
        expect(ticks("time")[0]).toEqual(["00:00", "t|hourly_power|hour|2026-10-26T00:00:00.000"]);
        expect(win.errors).toEqual(["TypeError", "TypeError", "TypeError", "TypeError", "TypeError", "Error"]);
      });
    },
    TEST_MS,
  );

  test(
    "window.tq cannot be assigned, and a page that redefines it still gets the runtime's API",
    async () => {
      const page = `<script>
try { window.tq = null; } catch (error) { window.assigned = error.name; }
Object.defineProperty(window, "tq", { value: { on() {} } });
</script>
<script>
tq.on("data", (api) => { window.same = api === tq && Object.isFrozen(api) && api !== window.tq; });
</script>`;
      await withPage(request(page), async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        expect(win.assigned).toBe("TypeError");
        expect(win.same).toBe(true);
      });
    },
    TEST_MS,
  );

  test(
    "a column named __proto__ is a column of the row",
    async () => {
      const page = `<script>tq.on("data", (tq) => { const row = tq.dataset("daily_energy").rows[0]; window.row = [Object.keys(row), row.__proto__, Object.getPrototypeOf(row) === Object.prototype]; });</script>`;
      const built = request(page, (changed) => {
        const daily = changed.datasets[0] as DatasetResult;
        daily.columns = [
          { name: "day", type: "time" },
          { name: "__proto__", type: "number" },
        ];
      });
      await withPage(built, async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        const [keys, value] = win.row as [string[], unknown, boolean];
        expect(keys).toEqual(["day", "__proto__", "$row"]);
        expect(value).toBe(120.5);
      });
    },
    TEST_MS,
  );

  test("timeOf reads a Date from any realm and nothing else", () => {
    expect(timeOf(new Date(5))).toBe(5);
    expect(timeOf(Object.create(Date.prototype))).toBeUndefined();
    expect(timeOf({ getTime: () => 1 })).toBeUndefined();
    expect(timeOf(5)).toBeUndefined();
  });
});

describe("view mode", () => {
  const page = `<p><b id="week"></b> <b id="missing"></b></p><p id="typed">上週 4,321 kWh</p>
<script>
let round = 0;
tq.on("data", (tq) => {
  round++;
  const week = document.getElementById("week");
  tq.text(week, "f|week_kwh", ",.1f");
  if (round === 1) week.textContent = "1.0";
  tq.text(document.getElementById("missing"), "c|daily_energy|kwh|99");
});
</script>`;

  test(
    "shows 「—」 for a value that does not match, and drops the flag when a later render is right",
    async () => {
      const built = request(page);
      await withPage(
        built,
        async (frame, win) => {
          await expectMessage(frame, "rendered", rendered(1));
          const week = win.document.getElementById("week");
          expect(week?.textContent).toBe("—");
          expect(week?.hasAttribute("data-tq-flag")).toBe(true);
          const typed = win.document.getElementById("typed");
          expect(typed?.textContent).toBe("上週 — kWh");
          expect(typed?.hasAttribute("data-tq-flag")).toBe(true);
          frame.runtime.receive(dataMessage(built.datasets, 2));
          await expectMessage(frame, "rendered 2", rendered(2));
          expect(week?.textContent).toBe("690.0");
          expect(week?.hasAttribute("data-tq-flag")).toBe(false);
        },
        "view",
      );
    },
    TEST_MS,
  );

  test(
    "a click on a value that resolves asks the host to show its source; on one that does not, nothing",
    async () => {
      await withPage(
        request(page),
        async (frame, win) => {
          await expectMessage(frame, "rendered", rendered(1));
          const click = (id: string) =>
            win.document.getElementById(id)?.dispatchEvent(new win.MouseEvent("click", { bubbles: true }));
          const before = frame.messages.length;
          click("missing");
          click("week");
          const sources = frame.messages.slice(before).filter((message) => message.type === "source");
          expect(sources).toEqual([{ tq: 1, type: "source", reference: "f|week_kwh" }]);
        },
        "view",
      );
    },
    TEST_MS,
  );

  test(
    "when the comparison fails, nothing of the page is shown",
    async () => {
      // In a browser the page can make the comparison throw by replacing built-ins of its realm;
      // here the runtime runs in the test's realm, so a DOM function the runtime captured fails
      // instead: reading a computed `content`, which only the comparison does (happy-dom shares the
      // prototype between windows).
      let owner: object | null = new Window().CSSStyleDeclaration.prototype as object;
      while (owner !== null && Object.getOwnPropertyDescriptor(owner, "getPropertyValue") === undefined) {
        owner = Object.getPrototypeOf(owner) as object | null;
      }
      const original = owner === null ? undefined : Object.getOwnPropertyDescriptor(owner, "getPropertyValue");
      if (owner === null || original === undefined || typeof original.value !== "function") {
        throw new Error("happy-dom has no getPropertyValue to wrap");
      }
      const getPropertyValue = original.value as (this: CSSStyleDeclaration, name: string) => string;
      let broken = false;
      Object.defineProperty(owner, "getPropertyValue", {
        ...original,
        value(this: CSSStyleDeclaration, name: string) {
          if (broken && name === "content") throw new Error("no comparison");
          return getPropertyValue.call(this, name);
        },
      });
      try {
        const built = request(
          `<p id="week"></p><script>tq.on("data", (tq) => tq.text(document.getElementById("week"), "f|week_kwh"));</script>`,
        );
        await withPage(
          built,
          async (frame, win) => {
            await expectMessage(frame, "rendered", rendered(1));
            expect(win.document.documentElement.hasAttribute("data-tq-flag-hide")).toBe(false);
            broken = true;
            frame.runtime.receive(dataMessage(built.datasets, 2));
            await expectMessage(frame, "rendered 2", rendered(2));
            broken = false;
            expect(win.document.documentElement.hasAttribute("data-tq-flag-hide")).toBe(true);
            frame.runtime.receive(dataMessage(built.datasets, 3));
            await expectMessage(frame, "rendered 3", rendered(3));
            expect(win.document.documentElement.hasAttribute("data-tq-flag-hide")).toBe(true);
          },
          "view",
        );
      } finally {
        Object.defineProperty(owner, "getPropertyValue", original);
      }
    },
    TEST_MS,
  );

  test(
    "check mode leaves the page as it drew it",
    async () => {
      await withPage(request(page), async (frame, win) => {
        await expectMessage(frame, "rendered", rendered(1));
        expect(win.document.getElementById("week")?.textContent).toBe("1.0");
        expect(win.document.querySelector("[data-tq-flag]")).toBeNull();
      });
    },
    TEST_MS,
  );

  // A page that keeps writing a wrong value back after the runtime corrects it: every recheck has
  // to rewrite the same value, so after MAX_CORRECTION_ROUNDS rounds the whole page is hidden for
  // good. The interval is longer than the quiet period, so each round is driven by one rewrite.
  const fightingPage = `<p><b id="week"></b></p>
<script>
tq.on("data", (tq) => {
  const week = document.getElementById("week");
  tq.text(week, "f|week_kwh", ",.1f");
  week.textContent = "999,999.0";
  if (!window.started) {
    window.started = true;
    setInterval(() => { week.textContent = "999,999.0"; }, 300);
  }
});
</script>`;

  test(
    "hides the page for good when it keeps rewriting a value the runtime corrected",
    async () => {
      await withPage(
        request(fightingPage),
        async (frame, win) => {
          await expectMessage(frame, "rendered", rendered(1));
          // The wrong value is corrected, not yet hidden.
          expect(win.document.getElementById("week")?.textContent).toBe("—");
          expect(win.document.documentElement.hasAttribute("data-tq-flag-hide")).toBe(false);
          // It keeps coming back; within the correction-round bound the page is hidden for good.
          await waitFor("the page hidden", () => win.document.documentElement.hasAttribute("data-tq-flag-hide"));
          // Once hidden it stays hidden, whatever the page does next.
          win.document.getElementById("week")?.setAttribute("data-x", "1");
          await new Promise((resolve) => setTimeout(resolve, 3 * 300));
          expect(win.document.documentElement.hasAttribute("data-tq-flag-hide")).toBe(true);
        },
        "view",
      );
    },
    TEST_MS,
  );

  // A page that keeps redrawing the correct value: every recheck finds nothing to rewrite, so the
  // correction streak never grows and the page keeps showing, past the hide threshold.
  const correctLoop = `<p><b id="week"></b></p>
<script>
tq.on("data", (tq) => {
  const week = document.getElementById("week");
  const draw = () => { tq.text(week, "f|week_kwh", ",.1f"); window.renders = (window.renders || 0) + 1; };
  draw();
  if (!window.started) { window.started = true; setInterval(draw, 120); }
});
</script>`;

  test(
    "keeps showing a page that keeps updating with correct values",
    async () => {
      await withPage(
        request(correctLoop),
        async (frame, win) => {
          await expectMessage(frame, "rendered", rendered(1));
          // Redraws correctly more often than the hide threshold, and is never hidden.
          await waitFor(
            "enough correct redraws",
            () => ((win.renders as number | undefined) ?? 0) > MAX_CORRECTION_ROUNDS + 3,
          );
          expect(win.document.documentElement.hasAttribute("data-tq-flag-hide")).toBe(false);
          expect(win.document.getElementById("week")?.textContent).toBe("690.0");
          expect(win.document.getElementById("week")?.hasAttribute("data-tq-flag")).toBe(false);
        },
        "view",
      );
    },
    TEST_MS,
  );
});

describe("trustedEvents", () => {
  /** An Event stand-in whose trust only its creator knows, as a browser's. */
  function fakeWindow() {
    const trusted = new WeakSet<object>();
    class FakeEvent {
      get isTrusted() {
        return trusted.has(this);
      }
    }
    const win = { Event: FakeEvent } as unknown as DomWindow;
    const make = (isTrusted: boolean) => {
      const event = new FakeEvent();
      if (isTrusted) trusted.add(event);
      return event as unknown as Event;
    };
    return { win, FakeEvent, make };
  }

  test("reads the trust the DOM gave the event, whatever the page changes later", () => {
    const { win, FakeEvent, make } = fakeWindow();
    const isTrusted = trustedEvents(win);
    if (isTrusted === null) throw new Error("no getter captured");
    const forged = make(false);
    Object.defineProperty(forged, "isTrusted", { value: true });
    Object.defineProperty(FakeEvent.prototype, "isTrusted", { get: () => true, configurable: true });
    expect(isTrusted(forged)).toBe(false);
    expect(isTrusted(make(false))).toBe(false);
    expect(isTrusted(make(true))).toBe(true);
    expect(isTrusted({} as Event)).toBe(false);
  });

  test("a DOM without isTrusted gives no reader, so the runtime does not start", () => {
    expect(trustedEvents({ Event: class {} } as unknown as DomWindow)).toBeNull();
  });
});

describe("installSandbox", () => {
  test("stays inert outside an opaque-origin frame", async () => {
    const frame = await openFrame();
    try {
      // The test window is top level with an https origin, like the files opened directly.
      expect(installSandbox(frame.window)).toBeNull();
    } finally {
      await frame.close();
    }
  });
});
