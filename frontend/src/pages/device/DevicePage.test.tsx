import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { createApiClient } from "../../api/client.ts";
import { type DeviceApi, deviceApi, type Series } from "../../api/devices.ts";
import { devicePath } from "../../app/paths.ts";
import type { Preferences } from "../../preferences/preferences.ts";
import { type FakeChart, fakeEngine } from "../../test/fake-chart.ts";
import { FakeDeviceServer, fakeError } from "../../test/fake-devices.ts";
import { type ManualClock, manualClock } from "../../test/manual-clock.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const numbers = new Intl.NumberFormat("en", { maximumFractionDigits: 3 });
const twoDecimals = new Intl.NumberFormat("en", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const minuteFormat = (zone: string) =>
  new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short", timeZone: zone });

/**
 * A meter with a voltage (float), an energy counter (int), a breaker (bool) and a status text,
 * read every minute for the last two hours, plus older readings 30 days ago.
 */
function meter(server = new FakeDeviceServer(), options: { pollIntervalMs?: number } = {}) {
  const device = server.addDevice({
    name: "line-1-meter",
    modelName: "power-meter",
    hardwareId: "tg-0042",
    pollIntervalMs: options.pollIntervalMs ?? 60_000,
    points: [
      { key: "voltage", data_type: "f32", description: "Phase A voltage" },
      { key: "energy_kwh", data_type: "u32" },
      { key: "breaker", data_type: "bool" },
      { key: "status", data_type: "string" },
    ],
  });
  const now = server.now();
  const minuteStart = Math.floor(now / MINUTE) * MINUTE;
  for (let minute = 120; minute >= 1; minute -= 1) {
    const time = minuteStart - minute * MINUTE + 7_000;
    server.addReadings(device.device_id, "voltage", [[time, { type: "float", value: 230 + (minute % 7) * 0.25 }]]);
    server.addReadings(device.device_id, "energy_kwh", [[time, { type: "int", value: 1000 + (120 - minute) }]]);
    server.addReadings(device.device_id, "breaker", [[time, { type: "bool", value: minute % 10 !== 0 }]]);
    server.addReadings(device.device_id, "status", [[time, { type: "string", value: minute % 2 ? "ok" : "check" }]]);
  }
  server.addReadings(device.device_id, "voltage", [[now - 30 * DAY, { type: "float", value: 228 }]]);
  return { server, device };
}

async function openDevice(
  server: FakeDeviceServer,
  deviceId: string,
  options: {
    query?: string;
    preferences?: Partial<Preferences>;
    /** The page's refresh timer; the real one (a minute) unless the test drives it. */
    clock?: ManualClock;
    /** The API the page calls; the server's own unless the test wraps it. */
    api?: DeviceApi;
  } = {},
) {
  const identity = signedInBrowser(options.preferences ? { preferences: options.preferences } : {}).browser;
  const chart = fakeEngine();
  const rendered = renderAt(`${devicePath(deviceId)}${options.query ?? ""}`, {
    identity,
    devices: options.api ?? server.api(),
    chartEngine: chart.engine,
    ...(options.clock ? { autoRefresh: options.clock } : {}),
  });
  await screen.findByRole("heading", { level: 1 });
  return { ...rendered, charts: chart.charts };
}

/** A screen of the given kind for one test: wide (the range as buttons) or a phone (a list). */
function onScreen<T>(kind: "wide" | "phone", run: () => Promise<T>): Promise<T> {
  const original = window.matchMedia;
  window.matchMedia = ((query: string) => ({
    matches: query === "(min-width: 640px)" && kind === "wide",
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  })) as unknown as typeof window.matchMedia;
  return run().finally(() => {
    window.matchMedia = original;
  });
}

/** A test on a phone: the range is the browser's own list there. */
function phoneTest(name: string, run: () => Promise<void>) {
  test(name, () => onScreen("phone", run));
}

const latestRegion = () => screen.getByRole("region", { name: "Current readings" });
const historyRegion = () => screen.getByRole("region", { name: /^History of / });

/** The reading list's button of a data point (named by its description, or its key). */
function readingRow(label: string): HTMLElement {
  return within(latestRegion()).getByRole("button", { name: new RegExp(label) });
}

/**
 * Waits until `find` finds nothing. Not `waitFor(() => expect(find()).toBeNull())`: while the
 * element is still there, that assertion formats the whole happy-dom node into its failure
 * message, which takes seconds per attempt and made a test wait about 4 s on a loaded runner.
 */
async function untilGone(what: string, find: () => HTMLElement | null): Promise<void> {
  await waitFor(() => {
    if (find() !== null) throw new Error(`${what} is still shown`);
  });
}

/** The chart the page made, once its option is set. */
async function theChart(charts: FakeChart[]): Promise<FakeChart> {
  await waitFor(() => expect(charts[0]?.options.length).toBeGreaterThan(0));
  return charts[0] as FakeChart;
}

type Line = { data: [number, number | null][]; markArea?: { data: unknown[][] }; step?: string; name: string };
const seriesOf = (chart: FakeChart) => chart.option.series as Line[];

/** The series the page asked for, asked again of the same API. */
async function lastSeriesAnswer(server: FakeDeviceServer): Promise<Series> {
  const url = server.seriesRequests().at(-1) as URL;
  const response = await server.fetch(new Request(url));
  return (await response.json()) as Series;
}

async function customRange(from: string, to: string) {
  await act(async () => {
    fireEvent.change(screen.getByLabelText("Time range"), { target: { value: "custom" } });
  });
  await act(async () => {
    fireEvent.change(screen.getByLabelText("From"), { target: { value: from } });
    fireEvent.change(screen.getByLabelText("To"), { target: { value: to } });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Show" }));
  });
}

describe("DevicePage", () => {
  test("shows the device, whether data is coming in, and the newest stored value of each data point", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    expect(screen.getByRole("heading", { level: 1, name: "line-1-meter" })).toBeDefined();
    expect(await screen.findByText(/^Data is coming in · /)).toBeDefined();
    const head = screen.getByRole("heading", { level: 1 }).closest("header") as HTMLElement;
    expect(within(head).getByText("power-meter")).toBeDefined();
    expect(within(head).getByText("tg-0042")).toBeDefined();
    expect(within(head).getByText("Polled every 1 minute")).toBeDefined();
    expect(screen.queryByText(/No new readings for/)).toBeNull();

    // Every stored value is the one the latest API answers, with its time.
    const latest = await server.api().getLatest(device.device_id);
    expect(latest.points.map((point) => point.point)).toEqual(["breaker", "energy_kwh", "status", "voltage"]);
    const expected: Record<string, string> = {};
    for (const point of latest.points) {
      const value = point.value;
      expected[point.point] =
        value.type === "bool"
          ? value.value
            ? "On"
            : "Off"
          : value.type === "string"
            ? value.value
            : value.type === "int"
              ? numbers.format(value.value)
              : twoDecimals.format(value.value ?? Number.NaN);
    }
    await waitFor(() =>
      expect(within(readingRow("Phase A voltage")).getByText(expected.voltage as string)).toBeDefined(),
    );
    for (const point of latest.points) {
      const row = readingRow(point.point === "voltage" ? "Phase A voltage" : point.point);
      expect(within(row).getByText(expected[point.point] as string)).toBeDefined();
      expect(row.querySelector("time")?.getAttribute("datetime")).toBe(point.time);
    }
    // The model's order; the description leads, the key follows.
    const rows = within(latestRegion()).getAllByRole("button");
    expect(rows.map((row) => row.querySelector("span span")?.textContent)).toEqual([
      "Phase A voltage",
      "energy_kwh",
      "breaker",
      "status",
    ]);
    expect(within(readingRow("Phase A voltage")).getByText("voltage")).toBeDefined();
    // The facts that matter only when something is wrong come last.
    const info = screen.getByRole("region", { name: "Device information" });
    expect(within(info).getByText("Modbus TCP, 192.0.2.10:502, unit 1")).toBeDefined();
    expect(within(info).getByText("Every 1 minute")).toBeDefined();
  });

  test("the chart plots the series the history API answers, in the user's time zone", async () => {
    const { server, device } = meter();
    const { charts } = await openDevice(server, device.device_id, { preferences: { timeZone: "Asia/Taipei" } });
    const chart = await theChart(charts);

    // The first chartable point, the last 24 hours, the user's zone.
    const request = server.seriesRequests().at(-1) as URL;
    expect(request.searchParams.get("point")).toBe("voltage");
    expect(request.searchParams.get("tz")).toBe("Asia/Taipei");
    const from = Date.parse(request.searchParams.get("from") ?? "");
    const to = Date.parse(request.searchParams.get("to") ?? "");
    expect(to - from).toBe(DAY);
    expect(Math.abs(Date.now() - to)).toBeLessThan(HOUR);
    expect(screen.getByRole("heading", { level: 2, name: "History of Phase A voltage" })).toBeDefined();
    expect(within(historyRegion()).getByText("Time zone Asia/Taipei")).toBeDefined();

    // One point per bucket of the answer, at the user's wall-clock time; the mean is the line.
    const answer = await lastSeriesAnswer(server);
    expect(answer.points.length).toBe(120);
    const [line] = seriesOf(chart);
    const drawn = (line as Line).data.filter(([, y]) => y !== null);
    expect(drawn.length).toBe(answer.points.length);
    answer.points.forEach((bucket, index) => {
      const [x, y] = drawn[index] as [number, number];
      expect(x).toBe(Date.parse(bucket.time) + 8 * HOUR);
      expect(y).toBe(bucket.avg as number);
    });
    // The band between each bucket's minimum and maximum is stacked under the same x values.
    const band = seriesOf(chart).find((item) => item.name === "band-range");
    expect(band?.data.length).toBe((line as Line).data.length);

    // The table lists exactly those buckets.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /^Show as a table/ }));
    });
    const table = within(historyRegion()).getByRole("table");
    const bodyRows = within(table).getAllByRole("row").slice(1);
    expect(bodyRows.length).toBe(answer.points.length);
    answer.points.forEach((bucket, index) => {
      const row = bodyRows[index] as HTMLElement;
      expect(row.querySelector("time")?.getAttribute("datetime")).toBe(bucket.time);
      expect(
        within(row)
          .getAllByRole("cell")
          .map((cell) => cell.textContent),
      ).toEqual([
        numbers.format(bucket.avg as number),
        numbers.format(bucket.min as number),
        numbers.format(bucket.max as number),
        numbers.format(bucket.last.value as number),
        String(bucket.count),
      ]);
    });
  });

  test("the figures over the chart are the mean, the extremes with their times, and the readings", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    await screen.findByRole("slider");
    const answer = await lastSeriesAnswer(server);
    let weighted = 0;
    let count = 0;
    let low = { value: Number.POSITIVE_INFINITY, time: "" };
    let high = { value: Number.NEGATIVE_INFINITY, time: "" };
    for (const bucket of answer.points) {
      weighted += (bucket.avg as number) * bucket.count;
      count += bucket.count;
      if ((bucket.min as number) < low.value) low = { value: bucket.min as number, time: bucket.time };
      if ((bucket.max as number) > high.value) high = { value: bucket.max as number, time: bucket.time };
    }
    const figure = (name: string) => within(historyRegion()).getByText(name).closest("div") as HTMLElement;
    expect(within(figure("Mean")).getByText(numbers.format(weighted / count))).toBeDefined();
    expect(within(figure("Min")).getByText(numbers.format(low.value))).toBeDefined();
    expect(within(figure("Min")).getByText(minuteFormat("UTC").format(Date.parse(low.time)))).toBeDefined();
    expect(within(figure("Max")).getByText(numbers.format(high.value))).toBeDefined();
    expect(within(figure("Readings")).getByText(`${numbers.format(count)} readings`)).toBeDefined();
  });

  test("the arrow keys read the buckets out through the slider's value text", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    const slider = await screen.findByRole("slider");
    const answer = await lastSeriesAnswer(server);
    await act(async () => {
      slider.focus();
    });
    expect(slider.getAttribute("aria-valuenow")).toBe(String(answer.points.length - 1));
    await act(async () => {
      fireEvent.keyDown(slider, { key: "Home" });
    });
    expect(slider.getAttribute("aria-valuenow")).toBe("0");
    const first = answer.points[0] as Series["points"][number];
    expect(slider.getAttribute("aria-valuetext")).toContain(`mean ${numbers.format(first.avg as number)}`);
    await act(async () => {
      fireEvent.keyDown(slider, { key: "ArrowRight" });
    });
    expect(slider.getAttribute("aria-valuenow")).toBe("1");
    // Enter has no source panel here: it opens the buckets as a table.
    await act(async () => {
      fireEvent.keyDown(slider, { key: "Enter" });
    });
    expect(within(historyRegion()).getByRole("table")).toBeDefined();
  });

  phoneTest("a reading row and a deep link choose the point and the range", async () => {
    const { server, device } = meter();
    const { router } = await openDevice(server, device.device_id, { query: "?point=energy_kwh&range=7d" });
    await screen.findByRole("slider");
    let request = server.seriesRequests().at(-1) as URL;
    expect(request.searchParams.get("point")).toBe("energy_kwh");
    expect(Date.parse(request.searchParams.get("to") ?? "") - Date.parse(request.searchParams.get("from") ?? "")).toBe(
      7 * DAY,
    );
    expect((screen.getByLabelText("Time range") as HTMLSelectElement).value).toBe("7d");
    expect(readingRow("energy_kwh").getAttribute("aria-pressed")).toBe("true");

    await act(async () => {
      fireEvent.click(readingRow("breaker"));
    });
    await waitFor(() => expect(server.seriesRequests().at(-1)?.searchParams.get("point")).toBe("breaker"));
    expect(router.state.location.search).toBe("?point=breaker&range=7d");
    expect(readingRow("breaker").getAttribute("aria-pressed")).toBe("true");
    expect(readingRow("energy_kwh").getAttribute("aria-pressed")).toBe("false");
    // A bool point is the share of readings that were on.
    expect(await screen.findByText(/Share of readings that were on/)).toBeDefined();
    expect(within(historyRegion()).getByText("Turned on")).toBeDefined();
    request = server.seriesRequests().at(-1) as URL;
    expect(request.searchParams.get("point")).toBe("breaker");
  });

  test("a bool point is drawn as steps between 0 and 100 %", async () => {
    const { server, device } = meter();
    const { charts } = await openDevice(server, device.device_id, { query: "?point=breaker" });
    const chart = await theChart(charts);
    const [line] = seriesOf(chart);
    expect(line?.step).toBe("end");
    const yAxis = chart.option.yAxis as { min: number; max: number };
    expect([yAxis.min, yAxis.max]).toEqual([0, 1]);
    // No band: a share has no minimum and maximum to show.
    expect(seriesOf(chart).some((item) => item.name === "band-range")).toBe(false);
    expect(within(historyRegion()).getByText("Now")).toBeDefined();
  });

  test("zooming a bool point recomputes its figures for the zoomed window", async () => {
    const { server, device } = meter();
    const { charts } = await openDevice(server, device.device_id, { query: "?range=6h&point=breaker" });
    const chart = await theChart(charts);
    const answer = await lastSeriesAnswer(server);
    const total = answer.points.reduce((n, bucket) => n + bucket.count, 0);
    const first = Date.parse(answer.points[10]?.time ?? "");
    const last = Date.parse(answer.points[19]?.time ?? "");
    await act(async () => {
      chart.window = { from: first, to: last + MINUTE };
      chart.emit("datazoom", { start: 10, end: 20 });
    });
    const zoomed = answer.points.slice(10, 20).reduce((n, bucket) => n + bucket.count, 0);
    expect(zoomed).toBeLessThan(total);
    const figures = within(historyRegion()).getByLabelText("Figures for the zoomed range");
    expect(within(figures).getByText(`${numbers.format(zoomed)} readings`)).toBeDefined();
    expect(within(figures).queryByText(`${numbers.format(total)} readings`)).toBeNull();
  });

  test("the range is buttons on a wide screen and chooses a preset", async () => {
    const { server, device } = meter();
    await onScreen("wide", async () => {
      const { router } = await openDevice(server, device.device_id);
      await screen.findByRole("slider");
      const group = screen.getByRole("group", { name: "Time range" });
      expect(within(group).getByRole("button", { name: "24 hours" }).getAttribute("aria-pressed")).toBe("true");
      await act(async () => {
        fireEvent.click(within(group).getByRole("button", { name: "6 hours" }));
      });
      expect(router.state.location.search).toBe("?range=6h");
      expect(within(group).getByRole("button", { name: "6 hours" }).getAttribute("aria-pressed")).toBe("true");
      // "Custom" opens the form for the range shown, and cancelling closes it again.
      await act(async () => {
        fireEvent.click(within(group).getByRole("button", { name: "Custom" }));
      });
      expect(screen.getByRole("form", { name: "Custom range" })).toBeDefined();
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
      });
      expect(screen.queryByRole("form", { name: "Custom range" })).toBeNull();
    });
  });

  test("a gap in the readings breaks the line and is drawn and named", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ pollIntervalMs: 60_000, points: [{ key: "voltage", data_type: "f32" }] });
    const minuteStart = Math.floor(server.now() / MINUTE) * MINUTE;
    for (const minute of [...range(1, 31), ...range(61, 91)]) {
      server.addReadings(device.device_id, "voltage", [
        [minuteStart - minute * MINUTE + 5_000, { type: "float", value: 230 }],
      ]);
    }
    const { charts } = await openDevice(server, device.device_id, { query: "?range=6h" });
    const chart = await theChart(charts);
    const [line] = seriesOf(chart);
    // The hole between 61 and 31 minutes ago has a null point in it; the hatched span is named.
    expect((line as Line).data.filter(([, y]) => y === null).length).toBeGreaterThanOrEqual(1);
    const areas = (line as Line).markArea?.data ?? [];
    expect(areas.length).toBeGreaterThanOrEqual(1);
    const label = (areas[0] as { label: { formatter(): string } }[])[0]?.label.formatter();
    expect(label).toMatch(/without readings/);
    // The figure says how much of the range had no readings.
    const readings = within(historyRegion()).getByText("Readings").closest("div") as HTMLElement;
    expect(within(readings).getByText(/without readings/)).toBeDefined();
  });

  test("zooming recomputes the figures and sets the zoomed window as the range", async () => {
    const { server, device } = meter();
    const { charts, router } = await openDevice(server, device.device_id, { query: "?range=6h" });
    const chart = await theChart(charts);
    const answer = await lastSeriesAnswer(server);
    const first = Date.parse(answer.points[10]?.time ?? "");
    const last = Date.parse(answer.points[19]?.time ?? "");
    await act(async () => {
      // UTC: the wall-clock x values are the instants themselves.
      chart.window = { from: first, to: last + MINUTE };
      chart.emit("datazoom", { start: 10, end: 20 });
    });
    expect(screen.getByText(/^Zoomed to /)).toBeDefined();
    const zoomed = answer.points.slice(10, 20);
    const mean = zoomed.reduce((sum, bucket) => sum + (bucket.avg as number), 0) / zoomed.length;
    expect(within(historyRegion()).getByText("Mean (zoomed range)")).toBeDefined();
    expect(within(historyRegion()).getByText(numbers.format(mean))).toBeDefined();
    expect(
      within(historyRegion()).getByText(
        `${numbers.format(zoomed.reduce((n, bucket) => n + bucket.count, 0))} readings`,
      ),
    ).toBeDefined();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Use as the range" }));
    });
    const params = new URLSearchParams(router.state.location.search);
    expect(Date.parse(params.get("from") ?? "")).toBe(first);
    expect(Date.parse(params.get("to") ?? "")).toBe(last + MINUTE);
    expect(params.get("range")).toBeNull();
    await untilGone("the zoomed figures", () => screen.queryByText("Mean (zoomed range)"));
  });

  phoneTest(
    "a range wholly before the queryable period explains it and offers the period, not a blank chart",
    async () => {
      const { server, device } = meter();
      server.aggregatedPeriodMs = 90 * DAY;
      await openDevice(server, device.device_id);
      await screen.findByRole("slider");
      await customRange("2025-01-01T00:00", "2025-01-02T00:00");

      expect(await screen.findByText("This range is outside the queryable period")).toBeDefined();
      expect(screen.queryByRole("slider")).toBeNull();
      const refused = await server.fetch(new Request(server.seriesRequests().at(-1) as URL));
      expect(refused.status).toBe(403);
      const earliest = Date.parse(((await refused.json()) as { details: { earliest: string } }).details.earliest);
      const minute = minuteFormat("UTC");
      expect(screen.getByText(new RegExp(`queried from ${minute.format(earliest)} on`))).toBeDefined();

      // The next step: the queryable period up to now.
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: `Show from ${minute.format(earliest)} until now` }));
      });
      await screen.findByRole("slider");
      expect(Date.parse(server.seriesRequests().at(-1)?.searchParams.get("from") ?? "")).toBe(earliest);
    },
  );

  test("a range that starts before the queryable period says where the chart starts", async () => {
    const { server, device } = meter();
    server.aggregatedPeriodMs = 60 * DAY;
    await openDevice(server, device.device_id, { query: "?range=90d" });
    expect(await screen.findByText(/The range starts before the queryable period/)).toBeDefined();
    expect(screen.getByRole("slider")).toBeDefined();
  });

  phoneTest("a custom range from the address opens its form from a button on a phone", async () => {
    const { server, device } = meter();
    const to = new Date(server.now() - HOUR).toISOString();
    const from = new Date(server.now() - 3 * HOUR).toISOString();
    await openDevice(server, device.device_id, { query: `?from=${from}&to=${to}` });
    await screen.findByRole("slider");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Change the range" }));
    });
    expect(screen.getByLabelText("From")).toBeDefined();
    expect(screen.queryByRole("button", { name: "Change the range" })).toBeNull();
  });

  phoneTest("a range without readings says so and suggests a longer one", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    await screen.findByRole("slider");
    const day = new Date(Date.now() - 10 * DAY).toISOString().slice(0, 10);
    await customRange(`${day}T00:00`, `${day}T06:00`);
    expect(await screen.findByText("No readings in this range")).toBeDefined();
    expect(screen.queryByRole("slider")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Show the last 7 days" }));
    });
    await screen.findByRole("slider");
    expect((screen.getByLabelText("Time range") as HTMLSelectElement).value).toBe("7d");
  });

  phoneTest("a custom range whose end is not after its start is refused before any request", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    await screen.findByRole("slider");
    const before = server.seriesRequests().length;
    await customRange("2026-10-02T00:00", "2026-10-01T00:00");
    expect(screen.getByText("The end must be after the start.")).toBeDefined();
    expect(screen.getByLabelText("To").getAttribute("aria-invalid")).toBe("true");
    expect(server.seriesRequests().length).toBe(before);
  });

  test("a text point lists its periods instead of a chart", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id, { query: "?point=status" });
    expect(await screen.findByText(/status holds text, so it is not drawn as a chart/)).toBeDefined();
    expect(screen.queryByRole("slider")).toBeNull();
    const table = within(historyRegion()).getByRole("table");
    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["Value", "From", "To", "Intervals"]);
    // The readings alternate every minute, so every bucket is a period of its own.
    const answer = await lastSeriesAnswer(server);
    expect(within(table).getAllByRole("row").length - 1).toBe(answer.points.length);
    expect(within(table).getAllByText("ok").length).toBeGreaterThan(0);
  });

  test("consecutive buckets of the same text are one row", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ points: [{ key: "mode", data_type: "string" }] });
    const minuteStart = Math.floor(server.now() / MINUTE) * MINUTE;
    for (let minute = 30; minute >= 1; minute -= 1) {
      server.addReadings(device.device_id, "mode", [
        [minuteStart - minute * MINUTE + 5_000, { type: "string", value: minute > 20 ? "auto" : "manual" }],
      ]);
    }
    await openDevice(server, device.device_id);
    const table = await within(historyRegion()).findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(
      rows.map((row) =>
        within(row)
          .getAllByRole("cell")
          .map((cell) => cell.textContent?.length),
      ),
    ).toHaveLength(2);
    expect(within(rows[0] as HTMLElement).getByText("manual")).toBeDefined();
    expect(within(rows[0] as HTMLElement).getByText("20")).toBeDefined();
    expect(within(rows[1] as HTMLElement).getByText("auto")).toBeDefined();
    expect(within(rows[1] as HTMLElement).getByText("10")).toBeDefined();
  });

  test("data older than three polling intervals is stalled: a tag, a banner and readings marked as old", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({
      hardwareId: "tg-0007",
      pollIntervalMs: 60_000,
      points: [{ key: "voltage", data_type: "f32" }],
    });
    const newest = Math.floor((server.now() - 3 * HOUR - 14 * MINUTE) / MINUTE) * MINUTE;
    for (let minute = 60; minute >= 0; minute -= 1) {
      server.addReadings(device.device_id, "voltage", [[newest - minute * MINUTE, { type: "float", value: 230 }]]);
    }
    const { charts } = await openDevice(server, device.device_id);
    expect(await screen.findByText(/^Data stalled · last reading 3 hours ago/)).toBeDefined();
    expect(screen.getByText(/^No new readings for 3 hours/)).toBeDefined();
    expect(screen.getByText(/Check that gateway tg-0007 has power and a network connection/)).toBeDefined();
    expect(screen.getByText(/the device at 192.0.2.10:502 is reachable/)).toBeDefined();
    // The row's time is marked, with an icon and the same words.
    expect(readingRow("voltage").querySelector("time svg")).not.toBeNull();
    // The gap runs on to the end of the range.
    const chart = await theChart(charts);
    const areas = (seriesOf(chart)[0] as Line).markArea?.data ?? [];
    expect(areas.length).toBeGreaterThanOrEqual(1);
  });

  test("a device polled every ten seconds is not stalled within two minutes", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ pollIntervalMs: 10_000, points: [{ key: "voltage", data_type: "f32" }] });
    server.addReadings(device.device_id, "voltage", [[server.now() - 100_000, { type: "float", value: 1 }]]);
    await openDevice(server, device.device_id);
    expect(await screen.findByText(/^Data is coming in · /)).toBeDefined();
    expect(screen.queryByText(/Data stalled/)).toBeNull();
  });

  test("a device without stored readings tells what to check and draws no chart", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ hardwareId: "tg-0007", points: [{ key: "voltage", data_type: "f32" }] });
    await openDevice(server, device.device_id);
    expect(await screen.findByText("No readings from this device yet")).toBeDefined();
    expect(screen.getByText("No readings yet")).toBeDefined();
    expect(screen.getByText("Gateway tg-0007 is online.")).toBeDefined();
    expect(screen.getByText("The connection settings are right: Modbus TCP, 192.0.2.10:502, unit 1.")).toBeDefined();
    expect(
      screen.getByText("The addresses of the data points in model power-meter match the device's manual."),
    ).toBeDefined();
    expect(screen.queryByRole("region", { name: /^History of / })).toBeNull();
    expect(server.seriesRequests()).toEqual([]);

    // Older readings may exist beyond what the latest values cover: the history opens on request.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Look at older history" }));
    });
    expect(await screen.findByRole("region", { name: /^History of / })).toBeDefined();
    await waitFor(() => expect(server.seriesRequests().length).toBeGreaterThan(0));

    // "Check again" asks again, and readings that arrived show up.
    server.addReadings(device.device_id, "voltage", [[server.now() - 10_000, { type: "float", value: 231.5 }]]);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    });
    expect(await screen.findByRole("region", { name: "Current readings" })).toBeDefined();
    expect(await within(readingRow("voltage")).findByText("231.50")).toBeDefined();
  });

  test("a model without data points says so, without a link or a button for the model page", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ modelName: "empty-model", points: [] });
    await openDevice(server, device.device_id);
    expect(await screen.findByText("Model empty-model has no data points yet")).toBeDefined();
    const card = screen.getByRole("region", { name: "Current readings" });
    expect(within(card).queryByRole("button")).toBeNull();
    expect(within(card).queryByRole("link")).toBeNull();
    expect(server.seriesRequests()).toEqual([]);
  });

  test("more than eight data points show eight and offer the others; a linked point past them stays visible", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({
      points: Array.from({ length: 11 }, (_, index) => ({
        key: `p${String(index).padStart(2, "0")}`,
        data_type: "u16" as const,
      })),
    });
    for (let index = 0; index < 11; index += 1) {
      server.addReadings(device.device_id, `p${String(index).padStart(2, "0")}`, [
        [server.now() - 5_000, { type: "int", value: index }],
      ]);
    }
    await openDevice(server, device.device_id);
    await screen.findByRole("button", { name: "Show 3 more data points" });
    expect(within(latestRegion()).getAllByRole("button").length).toBe(9);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Show 3 more data points" }));
    });
    expect(within(latestRegion()).getAllByRole("button").length).toBe(12);

    cleanup();
    await openDevice(server, device.device_id, { query: "?point=p10" });
    await waitFor(() => expect(readingRow("p10").getAttribute("aria-pressed")).toBe("true"));
  });

  test("a model that failed to load lists the points by key and can be retried", async () => {
    const { server, device } = meter();
    server.failNext("model", fakeError(503, "unavailable", "error.unavailable"));
    await openDevice(server, device.device_id);
    expect(
      await screen.findByText(/The device's model couldn't be loaded, so data points are listed by key only/),
    ).toBeDefined();
    expect(within(latestRegion()).getByRole("button", { name: /^voltage/ })).toBeDefined();
    await act(async () => {
      fireEvent.click(within(latestRegion()).getByRole("button", { name: "Try again" }));
    });
    await untilGone("the model failure", () => screen.queryByText(/couldn't be loaded, so data points/));
    expect(await within(latestRegion()).findByText("Phase A voltage")).toBeDefined();
  });

  test("a model that failed to load on a device without readings keeps the failure and its retry", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ hardwareId: "tg-0008", points: [{ key: "voltage", data_type: "f32" }] });
    server.failNext("model", fakeError(503, "unavailable", "error.unavailable"));
    await openDevice(server, device.device_id);
    expect(await screen.findByText(/The device's model couldn't be loaded/)).toBeDefined();
    // The checklist would name a model that is not known, and there is nothing to chart.
    expect(screen.queryByText("No readings from this device yet")).toBeNull();
    expect(screen.queryByRole("region", { name: /^History of / })).toBeNull();
    await act(async () => {
      fireEvent.click(within(latestRegion()).getByRole("button", { name: "Try again" }));
    });
    expect(await screen.findByText("No readings from this device yet")).toBeDefined();
  });

  test("an unknown or malformed device ID is not found", async () => {
    const server = new FakeDeviceServer();
    await openDevice(server, "00000000-0000-4000-8000-0000000fffff");
    expect(screen.getByRole("heading", { level: 1, name: "Device not found" })).toBeDefined();
    expect(screen.getByText(/Acme Factory has no device with this address/)).toBeDefined();
    expect(screen.getByRole("link", { name: /All devices/ }).getAttribute("href")).toBe("/devices");

    cleanup();
    const before = server.requests.length;
    await openDevice(server, "not-a-device");
    expect(screen.getByRole("heading", { level: 1, name: "Device not found" })).toBeDefined();
    expect(server.requests.length).toBe(before);
  });

  test("a device that fails to load says why, keeps the way back and can be retried", async () => {
    const { server, device } = meter();
    server.failNext("device", fakeError(503, "unavailable", "error.unavailable"));
    const identity = signedInBrowser().browser;
    renderAt(devicePath(device.device_id), { identity, devices: server.api(), chartEngine: fakeEngine().engine });
    expect(
      await screen.findByText(/The device couldn't be loaded: The service is temporarily unavailable/),
    ).toBeDefined();
    expect(screen.getByRole("link", { name: "Devices" }).getAttribute("href")).toBe("/devices");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    });
    expect(await screen.findByRole("heading", { level: 1, name: "line-1-meter" })).toBeDefined();
  });

  test("a failed request for the latest values or the history says why and can be retried", async () => {
    const { server, device } = meter();
    server.failNext("latest", fakeError(503, "unavailable", "error.unavailable"));
    server.failNext("series", fakeError(404, "not_found", "telemetry.account_not_ready"));
    await openDevice(server, device.device_id);
    expect(
      await screen.findByText(
        "The latest values couldn't be loaded: The service is temporarily unavailable. Try again later.",
      ),
    ).toBeDefined();
    expect(
      await screen.findByText(
        "The history couldn't be loaded: Telemetry history for this organization isn't ready yet. Try again in a few minutes.",
      ),
    ).toBeDefined();
    for (const button of screen.getAllByRole("button", { name: "Try again" })) {
      await act(async () => {
        fireEvent.click(button);
      });
    }
    await screen.findByRole("slider");
    await untilGone("the missing-reading mark of Phase A voltage", () =>
      within(readingRow("Phase A voltage")).queryByText("No reading"),
    );
  });

  test("the page follows the account's language", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id, { preferences: { language: "zh-TW" } });
    expect(await screen.findByRole("heading", { name: "目前讀數" })).toBeDefined();
    expect(screen.getByRole("heading", { name: "Phase A voltage 的歷史" })).toBeDefined();
    expect(screen.getByRole("heading", { name: "設備資訊" })).toBeDefined();
    expect(await screen.findByText(/^資料正常 · /)).toBeDefined();
  });
});

/** What a set of requests asked for, by endpoint, in a fixed order. */
function askedFor(server: FakeDeviceServer, since: number): string[] {
  return server.requests
    .slice(since)
    .map((url) => {
      if (url.pathname.endsWith("/series")) return "series";
      if (url.pathname.endsWith("/latest")) return "latest";
      return url.pathname.includes("/device-models/") ? "model" : "device";
    })
    .sort();
}

/** The server's API, with every answer held back from `hold()` to `release()`. */
function holdableApi(server: FakeDeviceServer) {
  let release: () => void = () => {};
  let open: Promise<void> = Promise.resolve();
  let arrived = 0;
  const api = deviceApi(
    createApiClient({
      fetch: async (request) => {
        if (new URL(request.url).pathname.endsWith("/series")) arrived += 1;
        await open;
        return server.fetch(request);
      },
    }),
  );
  return {
    api,
    hold: () => {
      open = new Promise<void>((resolve) => {
        release = resolve;
      });
    },
    release: () => release(),
    /** How many series requests have reached the API, held or not. */
    arrived: () => arrived,
  };
}

/**
 * What a screen reader would announce on its own: the live regions that are not switched off. The
 * data's state chip and the stall notice are left out: they are read from the clock and change at
 * every 30-second tick whether or not the page refreshes (a follow-up of
 * Laysi/trustiot-v2#1147), so a refresh adds nothing to them. The header's "Updated at" line is checked on its own.
 */
function announced(): string[] {
  return [...document.querySelectorAll('[role="status"], [role="alert"], [aria-live]')]
    .filter((element) => element.getAttribute("aria-live") !== "off")
    .map((element) => element.textContent ?? "")
    .filter((text) => !/^(Data is coming in|Data stalled|No readings yet|No new readings for|Updated )/.test(text));
}

function setVisibility(state: "visible" | "hidden") {
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => state });
  document.dispatchEvent(new Event("visibilitychange"));
}

describe("DevicePage automatic refresh", () => {
  let clock: ManualClock;
  beforeEach(() => {
    clock = manualClock();
  });
  afterEach(() => {
    Reflect.deleteProperty(document, "visibilityState");
    clock.restore();
  });
  const advance = (ms: number) =>
    act(async () => {
      clock.advance(ms);
    });
  /** The page, loaded: its readings, its chart and the status it shows. */
  async function opened(server: FakeDeviceServer, deviceId: string, options: Parameters<typeof openDevice>[2] = {}) {
    const rendered = await openDevice(server, deviceId, { ...options, clock });
    const chart = await theChart(rendered.charts);
    const slider = await screen.findByRole("slider");
    await screen.findByText(/^Data is coming in · /);
    return { ...rendered, chart, slider };
  }
  const lastSeriesTo = (server: FakeDeviceServer) =>
    Date.parse((server.seriesRequests().at(-1) as URL).searchParams.get("to") ?? "");

  test("an open page asks again every minute: the range's end moves and the readings that arrived show up", async () => {
    const { server, device } = meter();
    const { chart } = await opened(server, device.device_id);
    const firstEnd = lastSeriesTo(server);
    const drawn = chart.options.length;
    const requests = server.requests.length;
    // A reading that is stored after the page opened.
    server.addReadings(device.device_id, "voltage", [[Date.now() + 10_000, { type: "float", value: 250.5 }]]);
    expect(clock.pending()).toBe(1);

    await advance(59_999);
    expect(server.requests.length).toBe(requests);
    await advance(1);
    expect(await within(readingRow("Phase A voltage")).findByText("250.50")).toBeDefined();
    await waitFor(() => expect(chart.options.length).toBeGreaterThan(drawn));
    expect(lastSeriesTo(server)).toBe(firstEnd + 60_000);
    // The refresh button's rule: the device, its model, the latest values and the series, once each.
    expect(askedFor(server, requests)).toEqual(["device", "latest", "model", "series"]);
    // The next one is a minute after this one.
    expect(clock.pending()).toBe(1);
    await advance(60_000);
    await waitFor(() => expect(lastSeriesTo(server)).toBe(firstEnd + 120_000));
    expect(askedFor(server, requests)).toEqual([
      "device",
      "device",
      "latest",
      "latest",
      "model",
      "model",
      "series",
      "series",
    ]);
  });

  test("readings that stop show the stalled state and the gap by themselves, and resuming clears both", async () => {
    const { server, device } = meter();
    const { chart } = await opened(server, device.device_id);
    const gaps = () => (seriesOf(chart)[0] as Line).markArea?.data.length ?? 0;
    // The 24 hours start with a hole before the first reading; the one at the right edge is new.
    const leading = gaps();
    const stoppedAt = Date.now();

    // Four minutes without a reading: past three polling intervals, and a hole the chart marks.
    for (let minute = 0; minute < 4; minute += 1) await advance(60_000);
    expect(await screen.findByText(/^Data stalled · /)).toBeDefined();
    expect(await screen.findByText(/^No new readings for /)).toBeDefined();
    await waitFor(() => expect(gaps()).toBe(leading + 1));

    // The gateway is back and sends what it kept.
    for (let minute = 0; minute <= 4; minute += 1) {
      server.addReadings(device.device_id, "voltage", [
        [stoppedAt + minute * MINUTE + 5_000, { type: "float", value: 231 }],
      ]);
    }
    await advance(60_000);
    expect(await screen.findByText(/^Data is coming in · /)).toBeDefined();
    await untilGone("the stalled notice", () => screen.queryByText(/^No new readings for /));
    await waitFor(() => expect(gaps()).toBe(leading));
  });

  test("nothing flashes, moves or is announced while it loads, and what the reader set stays", async () => {
    const { server, device } = meter();
    const held = holdableApi(server);
    const { router, charts } = await openDevice(server, device.device_id, { clock, api: held.api });
    await screen.findByText(/^Data is coming in · /);
    // The reader's choices: another data point, the buckets as a table, a tooltip, the chart focused.
    await act(async () => {
      fireEvent.click(readingRow("energy_kwh"));
    });
    const slider = await screen.findByRole("slider");
    await waitFor(() => expect(server.seriesRequests().at(-1)?.searchParams.get("point")).toBe("energy_kwh"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /^Show as a table/ }));
    });
    // The chart of the data point now shown is the newest one the engine made.
    await waitFor(() => expect(charts.at(-1)?.options.length).toBeGreaterThan(0));
    const chart = charts.at(-1) as FakeChart;
    await act(async () => {
      chart.showTip(0, 5);
      slider.focus();
    });
    expect(screen.getByTestId("tip-time")).toBeDefined();
    const before = { announced: announced(), rows: within(historyRegion()).getAllByRole("row").length };
    const drawn = chart.options.length;

    // The series answer is held: the page is in the middle of a refresh nobody asked for.
    held.hold();
    const arrived = held.arrived();
    await advance(60_000);
    await waitFor(() => expect(held.arrived()).toBe(arrived + 1));
    const calm = () => {
      expect(screen.queryByText("Updating…")).toBeNull();
      expect(screen.queryByText("Refreshing…")).toBeNull();
      expect(screen.queryByText("Loading the history…")).toBeNull();
      expect((screen.getByRole("button", { name: "Refresh" }) as HTMLButtonElement).disabled).toBe(false);
      expect(document.querySelector('[aria-busy="true"]')).toBeNull();
      // The same chart element, with the focus where it was, and the reader's choices on it.
      expect(screen.getByRole("slider")).toBe(slider);
      expect(document.activeElement).toBe(slider);
      expect(screen.getByTestId("tip-time")).toBeDefined();
      expect(within(historyRegion()).getByRole("table")).toBeDefined();
      expect(readingRow("energy_kwh").getAttribute("aria-pressed")).toBe("true");
      expect(router.state.location.search).toBe("?point=energy_kwh");
      expect(announced()).toEqual(before.announced);
    };
    calm();
    // The timer's time is shown, but its live region is off.
    expect(screen.getByText(/^Updated /).getAttribute("aria-live")).toBe("off");

    held.release();
    await waitFor(() => expect(chart.options.length).toBeGreaterThan(drawn));
    calm();
    expect(within(historyRegion()).getAllByRole("row").length).toBe(before.rows);
    // Once it has loaded, the page is as it was: a request the reader makes later is announced and seen.
    await waitFor(() => expect(screen.getByText(/^Updated /).getAttribute("aria-live")).toBe("polite"));

    // The refresh button's own refresh is still seen: it says so and is disabled while it loads.
    held.hold();
    const asked = held.arrived();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    });
    expect(await screen.findByText("Refreshing…")).toBeDefined();
    expect((screen.getByRole("button", { name: "Refresh" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("Refreshing…").getAttribute("aria-live")).toBe("polite");
    await waitFor(() => expect(held.arrived()).toBe(asked + 1));
    held.release();
    await untilGone("the refresh in progress", () => screen.queryByText("Refreshing…"));
  });

  test("a zoomed chart is left alone, and the page catches up when it is back to the full range", async () => {
    const { server, device } = meter();
    const { chart } = await opened(server, device.device_id, { query: "?range=6h" });
    const answer = await lastSeriesAnswer(server);
    const first = Date.parse(answer.points[10]?.time ?? "");
    await act(async () => {
      chart.window = { from: first, to: first + 10 * MINUTE };
      chart.emit("datazoom", { start: 10, end: 20 });
    });
    expect(screen.getByText(/^Zoomed to /)).toBeDefined();
    const requests = server.requests.length;
    expect(clock.pending()).toBe(0);
    await advance(5 * MINUTE);
    expect(server.requests.length).toBe(requests);
    expect(screen.getByText(/^Zoomed to /)).toBeDefined();

    // Back to the full range: the minute has long passed, so it asks at once.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Show the full range" }));
      chart.window = null;
      chart.emit("datazoom", { start: 0, end: 100 });
    });
    expect(clock.pending()).toBe(1);
    await advance(0);
    await waitFor(() => expect(askedFor(server, requests)).toEqual(["device", "latest", "model", "series"]));
  });

  test("a range the reader picked by its instants is a fixed span and is not refreshed", async () => {
    const { server, device } = meter();
    const to = Date.now() - 10 * MINUTE;
    const query = `?from=${encodeURIComponent(new Date(to - HOUR).toISOString())}&to=${encodeURIComponent(new Date(to).toISOString())}`;
    await opened(server, device.device_id, { query });
    const requests = server.requests.length;
    expect(clock.pending()).toBe(0);
    await advance(10 * MINUTE);
    expect(server.requests.length).toBe(requests);
  });

  test("a hidden page does not ask; coming back asks at once when the minute has passed, else when it is up", async () => {
    const { server, device } = meter();
    await opened(server, device.device_id);
    const requests = server.requests.length;

    await advance(20_000);
    await act(async () => {
      setVisibility("hidden");
    });
    expect(clock.pending()).toBe(0);
    await advance(10 * MINUTE);
    expect(server.requests.length).toBe(requests);
    await act(async () => {
      setVisibility("visible");
    });
    await advance(0);
    await waitFor(() => expect(askedFor(server, requests)).toEqual(["device", "latest", "model", "series"]));

    // Hidden for part of a minute: the minute is counted from the last refresh, not from coming back.
    const afterRefresh = server.requests.length;
    await advance(10_000);
    await act(async () => {
      setVisibility("hidden");
    });
    await advance(20_000);
    await act(async () => {
      setVisibility("visible");
    });
    await advance(29_999);
    expect(server.requests.length).toBe(afterRefresh);
    await advance(1);
    await waitFor(() => expect(askedFor(server, afterRefresh)).toEqual(["device", "latest", "model", "series"]));
  });

  test("a refresh that fails keeps what is shown, says why, and the next one tries again", async () => {
    const { server, device } = meter();
    const { slider } = await opened(server, device.device_id);
    const shown = within(readingRow("Phase A voltage")).getByText(/^\d+\.\d\d$/).textContent;
    server.failNext("device", fakeError(503, "unavailable", "error.unavailable"));
    server.failNext("latest", fakeError(503, "unavailable", "error.unavailable"));
    server.failNext("series", fakeError(503, "unavailable", "error.unavailable"));
    await advance(60_000);

    expect(await screen.findByText(/^The device couldn't be loaded: /)).toBeDefined();
    expect(await screen.findByText(/^The latest values couldn't be loaded: /)).toBeDefined();
    expect(await screen.findByText(/^The history couldn't be loaded: /)).toBeDefined();
    // Not an error page: the head, the readings and the chart are all still there.
    expect(screen.getByRole("heading", { level: 1, name: "line-1-meter" })).toBeDefined();
    expect(within(readingRow("Phase A voltage")).getByText(shown as string)).toBeDefined();
    expect(screen.getByRole("slider")).toBe(slider);

    // The next minute asks again, and everything recovers.
    expect(clock.pending()).toBe(1);
    await advance(60_000);
    await untilGone("the device failure", () => screen.queryByText(/^The device couldn't be loaded: /));
    await untilGone("the latest values failure", () => screen.queryByText(/^The latest values couldn't be loaded: /));
    await untilGone("the history failure", () => screen.queryByText(/^The history couldn't be loaded: /));
    expect(screen.getByRole("slider")).toBe(slider);
  });
});

function range(from: number, to: number): number[] {
  return Array.from({ length: to - from }, (_, index) => from + index);
}
