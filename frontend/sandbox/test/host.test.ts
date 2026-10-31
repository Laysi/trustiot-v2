// The host adapter (src/host.ts; README 「postMessage 協定」): it accepts only well-formed messages
// from its own frame's window, sends init once and data per generation after `ready`, and stops at
// the first sign of a navigation.
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { SandboxHost } from "../src/host.ts";
import { type CheckRequest, CheckRequestSchema, PROTOCOL, type SandboxMessage } from "../src/protocol.ts";
import { type FakeFrame, type HostWindow, openHostWindow } from "./frames.ts";

const request: CheckRequest = CheckRequestSchema.parse({
  ...JSON.parse(readFileSync(new URL("../fixtures/request.json", import.meta.url), "utf8")),
  page: "<p>page</p>",
});

const ready = { tq: PROTOCOL, type: "ready", runtime: 1 };
const returned = (generation: number) => ({ tq: PROTOCOL, type: "returned", generation });
const rendered = (generation: number) => ({ tq: PROTOCOL, type: "rendered", generation });

let open: HostWindow | null = null;

afterEach(async () => {
  await open?.close();
  open = null;
});

function setUp(base = "/sandbox") {
  open = openHostWindow();
  const received: SandboxMessage[] = [];
  let navigated = 0;
  const host = new SandboxHost({
    container: open.document.body,
    runtime: 1,
    base,
    title: "Energy",
    onMessage: (message) => received.push(message),
    onNavigate: () => navigated++,
  });
  const frame = open.attach(host.frame);
  return { host, frame, received, navigated: () => navigated };
}

const initOf = () => ({
  page: request.page,
  spec: request.spec,
  locale: request.locale,
  tz: request.tz,
  theme: "dark" as const,
  mode: "view" as const,
});

const datasets = () => request.datasets.map((result) => ({ status: "ok" as const, result }));

function generationOf(entry: FakeFrame["sent"][number] | undefined) {
  return (entry?.message as { generation?: number } | undefined)?.generation;
}

function types(frame: FakeFrame) {
  return frame.sent.map((entry) => (entry.message as { type: string }).type);
}

describe("the frame", () => {
  test("is sandboxed before it has a source, sends no referrer, and loads the runtime's document", () => {
    const { host } = setUp();
    const names = host.frame.getAttributeNames();
    expect(host.frame.getAttribute("sandbox")).toBe("allow-scripts");
    expect(host.frame.getAttribute("referrerpolicy")).toBe("no-referrer");
    expect(host.frame.getAttribute("title")).toBe("Energy");
    expect(host.frame.getAttribute("src")).toBe("/sandbox/v1/sandbox.html");
    expect(names.indexOf("sandbox")).toBeLessThan(names.indexOf("src"));
    expect(host.frame.parentNode).toBe(open?.document.body as never);
  });

  test("a runtime version the build does not have is refused", () => {
    open = openHostWindow();
    const container = open.document.body;
    expect(
      () => new SandboxHost({ container, runtime: 2 as never, base: ".", title: "x", onMessage: () => {} }),
    ).toThrow();
    expect(container.querySelector("iframe")).toBeNull();
  });
});

describe("what the host accepts", () => {
  test("only parsed protocol messages from its frame's window with an opaque origin", () => {
    const { frame, received } = setUp();
    frame.post(ready, { origin: "https://check.invalid" });
    frame.post(ready, { source: {} });
    frame.post(ready, { source: null });
    frame.post({ ...ready, extra: true });
    frame.post({ ...ready, tq: 2 });
    frame.post({ tq: PROTOCOL, type: "navigate", to: "https://dashboard.invalid/" });
    frame.post(JSON.stringify(ready));
    expect(received).toEqual([]);
    frame.post(ready);
    expect(received).toEqual([ready as SandboxMessage]);
  });

  test("nothing but ready before init went out", () => {
    const { host, frame, received } = setUp();
    frame.post(rendered(1));
    frame.post(ready);
    frame.post(returned(1));
    expect(received.map((message) => message.type)).toEqual(["ready"]);
    host.init(initOf());
    host.data(datasets());
    frame.post(returned(1));
    expect(received.map((message) => message.type)).toEqual(["ready", "returned"]);
  });

  test("returned and rendered for a generation not sent yet are dropped", () => {
    const { host, frame, received } = setUp();
    frame.post(ready);
    host.init(initOf());
    expect(host.data(datasets())).toBe(1);
    frame.post(returned(2));
    frame.post(rendered(2));
    frame.post(rendered(1));
    expect(host.data(datasets())).toBe(2);
    frame.post(rendered(2));
    expect(received.slice(1)).toEqual([rendered(1), rendered(2)] as SandboxMessage[]);
  });
});

describe("what the host sends", () => {
  test("init then the latest data, only after ready, with the target origin *", () => {
    const { host, frame } = setUp();
    host.data(datasets());
    host.init(initOf());
    expect(frame.sent).toEqual([]);
    frame.post(ready);
    expect(types(frame)).toEqual(["init", "data"]);
    expect(frame.sent.every((entry) => entry.targetOrigin === "*")).toBe(true);
    const init = frame.sent[0]?.message as Record<string, unknown>;
    expect(init).toEqual({ tq: PROTOCOL, type: "init", runtime: 1, ...initOf() });
    expect(generationOf(frame.sent[1])).toBe(1);
    host.data(datasets());
    expect(types(frame)).toEqual(["init", "data", "data"]);
    expect(generationOf(frame.sent[2])).toBe(2);
  });

  test("init is sent once, and drops what the protocol does not carry", () => {
    const { host, frame } = setUp();
    frame.post(ready);
    const spec = {
      ...request.spec,
      datasets: request.spec.datasets.map((dataset) => ({ ...dataset, sql: "SELECT 1" })),
    };
    host.init({ ...initOf(), spec });
    expect(() => host.init(initOf())).toThrow();
    expect(JSON.stringify(frame.sent[0]?.message)).not.toContain("SELECT");
    expect(types(frame)).toEqual(["init"]);
  });

  test("a malformed data message is refused before it is sent", () => {
    const { host, frame } = setUp();
    frame.post(ready);
    host.init(initOf());
    expect(() => host.data([{ status: "ok", result: { dataset: "x" } as never }])).toThrow();
    expect(types(frame)).toEqual(["init"]);
  });
});

describe("navigation and teardown", () => {
  test("a second ready means the frame reloaded: onNavigate, then nothing more", () => {
    const { host, frame, received, navigated } = setUp();
    frame.post(ready);
    host.init(initOf());
    frame.post(ready);
    expect(navigated()).toBe(1);
    host.data(datasets());
    frame.post(returned(1));
    frame.post(ready);
    expect(navigated()).toBe(1);
    expect(types(frame)).toEqual(["init"]);
    expect(received.map((message) => message.type)).toEqual(["ready"]);
  });

  test("a second load of the frame is a navigation too", () => {
    const { host, frame, received, navigated } = setUp();
    frame.load();
    frame.post(ready);
    host.init(initOf());
    frame.load();
    expect(navigated()).toBe(1);
    host.data(datasets());
    frame.post(returned(1));
    expect(types(frame)).toEqual(["init"]);
    expect(received.map((message) => message.type)).toEqual(["ready"]);
  });

  test("destroy removes the frame and stops listening", () => {
    const { host, frame, received } = setUp();
    host.destroy();
    expect(host.frame.parentNode).toBeNull();
    frame.post(ready);
    expect(received).toEqual([]);
  });
});
