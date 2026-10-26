// The checker's host page (src/check-host.ts; README 「檢查服務的流程」 step 4): what the
// checking service calls and what it gets back through the report binding.
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { type CheckReport, createCheckHost, FRAME_HEIGHT, FRAME_WIDTH, REPORT_BINDING } from "../src/check-host.ts";
import { MAX_FRAME_HEIGHT, PROTOCOL } from "../src/protocol.ts";
import { type FakeFrame, type HostWindow, openHostWindow } from "./frames.ts";

const request = {
  ...JSON.parse(readFileSync(new URL("../fixtures/request.json", import.meta.url), "utf8")),
  page: "<p>page</p>",
};

let open: HostWindow | null = null;

afterEach(async () => {
  await open?.close();
  open = null;
});

function setUp(withBinding = true) {
  open = openHostWindow();
  const win = open.window as unknown as Window;
  const reports: CheckReport[] = [];
  if (withBinding) {
    Object.defineProperty(win, REPORT_BINDING, {
      value: (payload: string) => reports.push(JSON.parse(payload) as CheckReport),
      configurable: true,
    });
  }
  const host = createCheckHost(win);
  const frameOf = (): { element: HTMLIFrameElement; fake: FakeFrame } => {
    const element = open?.document.querySelector("iframe");
    if (element === null || element === undefined || open === null) throw new Error("no frame");
    return { element, fake: open.attach(element) };
  };
  return { host, reports, frameOf };
}

const message = (type: string, fields: Record<string, unknown> = {}) => ({ tq: PROTOCOL, type, ...fields });

describe("start", () => {
  test("refuses a malformed request and creates no frame", () => {
    const { host } = setUp();
    expect(() => host.start({ ...request, page: undefined })).toThrow();
    expect(() => host.start({ ...request, runtime: 2 })).toThrow();
    expect(() => host.start({ ...request, locale: "fr" })).toThrow();
    expect(() => host.start(JSON.stringify(request))).toThrow();
    expect(open?.document.querySelector("iframe")).toBeNull();
  });

  test("runs once per page load", () => {
    const { host } = setUp();
    host.start(request);
    expect(() => host.start(request)).toThrow();
    expect(open?.document.querySelectorAll("iframe").length).toBe(1);
  });

  test("the host object cannot be changed", () => {
    const { host } = setUp();
    expect(Object.isFrozen(host)).toBe(true);
  });

  test("creates the sandbox frame next to it, at the check's size", () => {
    const { host, frameOf } = setUp();
    host.start(request);
    const { element } = frameOf();
    expect(element.getAttribute("src")).toBe("./v1/sandbox.html");
    expect(element.getAttribute("sandbox")).toBe("allow-scripts");
    expect(element.width).toBe(String(FRAME_WIDTH));
    expect(element.height).toBe(String(FRAME_HEIGHT));
  });
});

describe("the conversation with the sandbox", () => {
  test("after ready: init in check mode and the light theme, then generation 1 with every dataset", () => {
    const { host, frameOf, reports } = setUp();
    host.start(request);
    const { fake } = frameOf();
    fake.post(message("ready", { runtime: 1 }));
    expect(reports).toEqual([{ type: "ready" }]);
    expect(fake.sent.map((entry) => entry.message)).toEqual([
      {
        tq: PROTOCOL,
        type: "init",
        runtime: 1,
        page: request.page,
        spec: request.spec,
        locale: request.locale,
        tz: request.tz,
        theme: "light",
        mode: "check",
      },
      {
        tq: PROTOCOL,
        type: "data",
        generation: 1,
        datasets: request.datasets.map((result: unknown) => ({ status: "ok", result })),
      },
    ]);
  });

  test("reports returned, rendered and the first error, each once and in order", () => {
    const { host, frameOf, reports } = setUp();
    host.start(request);
    const { fake } = frameOf();
    fake.post(message("ready", { runtime: 1 }));
    fake.post(message("returned", { generation: 1 }));
    fake.post(message("returned", { generation: 1 }));
    fake.post(message("rendered", { generation: 1 }));
    fake.post(message("rendered", { generation: 1 }));
    fake.post(message("error", { phase: "render", message: "Error: first" }));
    fake.post(message("error", { phase: "render", message: "Error: second" }));
    fake.post(message("source", { reference: "f|week_kwh" }));
    expect(reports).toEqual([
      { type: "ready" },
      { type: "returned" },
      { type: "rendered" },
      { type: "error", message: "Error: first" },
    ]);
  });

  test("an error message loses hidden characters, and an empty one is left out", () => {
    const first = setUp();
    first.host.start(request);
    const one = first.frameOf().fake;
    one.post(message("ready", { runtime: 1 }));
    one.post(message("error", { phase: "page", message: "Error:\u0007 bad\u202Eone " }));
    expect(first.reports.at(-1)).toEqual({ type: "error", message: "Error:  bad one" });
  });

  test("an error without visible text is reported without a message", async () => {
    const { host, frameOf, reports } = setUp();
    host.start(request);
    const { fake } = frameOf();
    fake.post(message("ready", { runtime: 1 }));
    fake.post(message("error", { phase: "page", message: "\u200B\u0000 " }));
    expect(reports.at(-1)).toEqual({ type: "error" });
  });

  test("the frame grows with the page between its own height and the limit", () => {
    const { host, frameOf } = setUp();
    host.start(request);
    const { element, fake } = frameOf();
    fake.post(message("ready", { runtime: 1 }));
    fake.post(message("resize", { height: 12 }));
    expect(element.height).toBe(String(FRAME_HEIGHT));
    fake.post(message("resize", { height: 4321 }));
    expect(element.height).toBe("4321");
    fake.post(message("resize", { height: MAX_FRAME_HEIGHT }));
    expect(element.height).toBe(String(MAX_FRAME_HEIGHT));
    fake.post(message("resize", { height: MAX_FRAME_HEIGHT + 1 }));
    expect(element.height).toBe(String(MAX_FRAME_HEIGHT));
  });

  test("without the binding nothing throws", () => {
    const { host, frameOf } = setUp(false);
    host.start(request);
    const { fake } = frameOf();
    expect(() => fake.post(message("ready", { runtime: 1 }))).not.toThrow();
    expect(fake.sent.length).toBe(2);
  });
});
