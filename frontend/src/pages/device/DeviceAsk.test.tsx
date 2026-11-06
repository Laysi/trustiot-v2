import { afterEach, beforeEach, describe, expect, setSystemTime, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { devicePath } from "../../app/paths.ts";
import { type FakeChart, fakeEngine } from "../../test/fake-chart.ts";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

// 2026-11-04 is a Wednesday: this week so far is 11-02 to 11-04, last week 10-26 to 11-01.
const NOW = Date.UTC(2026, 10, 4, 10, 0, 0);
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

beforeEach(() => {
  setSystemTime(new Date(NOW));
});

afterEach(() => {
  cleanup();
  setSystemTime();
});

/** A device with one data point, whose newest reading is `ageMs` old (or that never stored one). */
function setUp(options: { ageMs?: number | null; role?: "viewer" | "editor" | "admin" } = {}) {
  const devices = new FakeDeviceServer();
  const device = devices.addDevice({
    name: "Compressor A2",
    modelName: "screw-compressor",
    hardwareId: "tg-0042",
    pollIntervalMs: 60_000,
    points: [{ key: "exhaust_temp", data_type: "f32", description: "Exhaust temperature" }],
  });
  const age = options.ageMs === undefined ? MINUTE : options.ageMs;
  if (age !== null) {
    // Hourly readings for a day back from the newest one.
    for (let hour = 0; hour < 24; hour += 1) {
      devices.addReadings(device.device_id, "exhaust_temp", [
        [NOW - age - hour * HOUR, { type: "float", value: 180 + (hour % 5) }],
      ]);
    }
  }
  const conversations = new FakeQuestionServer();
  const chart = fakeEngine();
  const rendered = renderAt(devicePath(device.device_id), {
    devices: devices.api(),
    conversations: conversations.api(),
    chartEngine: chart.engine,
    identity: signedInBrowser({ role: options.role ?? "admin" }).browser,
  });
  return { devices, device, conversations, chart, ...rendered };
}

const group = () => screen.findByRole("group", { name: "Ask AI about “Compressor A2”" });
const questions = (server: FakeQuestionServer) =>
  server.requests.filter((request) => request.method === "POST" && request.url.pathname.endsWith("/questions"));

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("the device page's 「Ask AI」 buttons", () => {
  test("a device with data offers 「Compare with last week」, and not 「Why is it offline?」", async () => {
    setUp();
    const buttons = within(await group()).getAllByRole("button");
    expect(buttons.map((button) => button.textContent)).toEqual(["Compare with last week"]);
  });

  test("pressing it opens a conversation with the device and both weeks in the question, and a way back", async () => {
    const { conversations, device, router } = setUp();
    click(within(await group()).getByRole("button", { name: "Compare with last week" }));

    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    const text =
      "Compare with last week: how do Compressor A2's energy use, running time and anomalies this week (2026-11-02 – 2026-11-04) differ from last week (2026-10-26 – 2026-11-01)? Dates are in UTC.";
    expect(questions(conversations).map((request) => request.body)).toEqual([{ question: text }]);
    const pending = await screen.findByRole("list", { name: "Question being answered" });
    expect(within(pending).getByText(text)).toBeTruthy();

    // The conversation leads back to the device page, which opens as it was.
    const back = await screen.findByRole("link", { name: "Back to Compressor A2" });
    expect(back.getAttribute("href")).toBe(devicePath(device.device_id));
    click(back);
    await waitFor(() => expect(router.state.location.pathname).toBe(devicePath(device.device_id)));
    await screen.findByRole("heading", { level: 1, name: "Compressor A2" });
  });

  test("the week is the one in the user's time zone", async () => {
    // 2026-11-01 17:00 UTC is already Monday 11-02 in Taipei.
    setSystemTime(new Date(Date.UTC(2026, 10, 1, 17, 0, 0)));
    const devices = new FakeDeviceServer();
    const device = devices.addDevice({
      name: "Compressor A2",
      points: [{ key: "exhaust_temp", data_type: "f32" }],
    });
    devices.addReadings(device.device_id, "exhaust_temp", [
      [Date.UTC(2026, 10, 1, 16, 59, 0), { type: "float", value: 181 }],
    ]);
    const conversations = new FakeQuestionServer();
    const identity = signedInBrowser({ preferences: { timeZone: "Asia/Taipei" } }).browser;
    renderAt(devicePath(device.device_id), {
      devices: devices.api(),
      conversations: conversations.api(),
      chartEngine: fakeEngine().engine,
      identity,
    });
    click(within(await group()).getByRole("button", { name: "Compare with last week" }));
    await waitFor(() => expect(questions(conversations)).toHaveLength(1));
    const body = questions(conversations)[0]?.body as { question: string } | undefined;
    expect(body?.question).toContain(
      "this week (2026-11-02 – 2026-11-02) differ from last week (2026-10-26 – 2026-11-01)? Dates are in Asia/Taipei.",
    );
  });

  test("while the data is stalled it also offers 「Why is it offline?」, with when the last reading was", async () => {
    const { conversations, router } = setUp({ ageMs: 3 * HOUR });
    const buttons = within(await group()).getAllByRole("button");
    expect(buttons.map((button) => button.textContent)).toEqual(["Why is it offline?", "Compare with last week"]);
    click(buttons[0] as HTMLElement);
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    // The newest reading was 3 hours before 2026-11-04 10:00 UTC.
    expect(questions(conversations).map((request) => request.body)).toEqual([
      {
        question:
          "Why is it offline: Compressor A2 has had no new readings since 2026-11-04 07:00 (UTC). What could be the cause?",
      },
    ]);
    expect(await screen.findByRole("link", { name: "Back to Compressor A2" })).toBeTruthy();
  });

  test("a device that never stored a reading offers only 「Why is there no data?」", async () => {
    const { conversations, router } = setUp({ ageMs: null });
    const buttons = within(await group()).getAllByRole("button");
    expect(buttons.map((button) => button.textContent)).toEqual(["Why is there no data?"]);
    click(buttons[0] as HTMLElement);
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    expect(questions(conversations).map((request) => request.body)).toEqual([
      { question: "Why is there no data: Compressor A2 has no readings yet. What could be the cause?" },
    ]);
  });

  test("a viewer asks too: asking is read only", async () => {
    setUp({ role: "viewer" });
    expect(within(await group()).getByRole("button", { name: "Compare with last week" })).toBeTruthy();
  });

  test("a question that cannot be sent is explained under the button, and the page stays", async () => {
    const { conversations, device, router } = setUp();
    conversations.failNextOn("ask", fakeError(403, "quota_exceeded", "error.quota_exceeded"));
    const button = within(await group()).getByRole("button", { name: "Compare with last week" });
    click(button);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toBe("The question wasn't sent: Your plan's limit has been reached.");
    expect(button.getAttribute("aria-describedby")).toBe(alert.id);
    expect(router.state.location.pathname).toBe(devicePath(device.device_id));
    expect(screen.queryByRole("link", { name: "Back to Compressor A2" })).toBeNull();
  });
});

describe("「Analyze this stretch」 on the device's chart", () => {
  async function zoom(chart: FakeChart, from: number, to: number) {
    await act(async () => {
      chart.window = { from, to };
      chart.emit("datazoom", { start: 10, end: 20 });
    });
  }

  test("it shows only while the chart is zoomed, and asks about the device, data point and stretch", async () => {
    const { conversations, chart, router } = setUp();
    const charts = chart.charts;
    await screen.findByRole("slider");
    await waitFor(() => expect(charts[0]?.options.length).toBeGreaterThan(0));
    expect(screen.queryByRole("button", { name: "Analyze this stretch" })).toBeNull();

    // UTC: the wall-clock x values are the instants themselves.
    await zoom(charts[0] as FakeChart, Date.UTC(2026, 10, 4, 3, 0), Date.UTC(2026, 10, 4, 6, 30));
    click(await screen.findByRole("button", { name: "Analyze this stretch" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    expect(questions(conversations).map((request) => request.body)).toEqual([
      {
        question:
          "Analyze this stretch: what happened to Compressor A2's “Exhaust temperature” from 2026-11-04 03:00 to 2026-11-04 06:30 (UTC)? Was there an anomaly or anything else worth noting, and what caused it?",
      },
    ]);
    expect(await screen.findByRole("link", { name: "Back to Compressor A2" })).toBeTruthy();
  });

  test("resetting the zoom takes it away", async () => {
    const { chart } = setUp();
    await screen.findByRole("slider");
    await waitFor(() => expect(chart.charts[0]?.options.length).toBeGreaterThan(0));
    await zoom(chart.charts[0] as FakeChart, Date.UTC(2026, 10, 4, 3, 0), Date.UTC(2026, 10, 4, 6, 30));
    await screen.findByRole("button", { name: "Analyze this stretch" });
    await act(async () => {
      chart.charts[0]?.emit("datazoom", { start: 0, end: 100 });
    });
    await waitFor(() => expect(screen.queryByRole("button", { name: "Analyze this stretch" })).toBeNull());
  });
});
