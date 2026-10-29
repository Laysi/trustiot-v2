import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import type { Series } from "../../api/devices.ts";
import { devicePath } from "../../app/paths.ts";
import type { Preferences } from "../../preferences/preferences.ts";
import { FakeDeviceServer, fakeError } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const numbers = new Intl.NumberFormat("en", { maximumFractionDigits: 3 });

/**
 * A meter with a voltage (float), an energy counter (int), a breaker (bool) and a status text,
 * read every minute for the last two hours, plus older readings 30 days ago.
 */
function meter(server = new FakeDeviceServer()) {
  const device = server.addDevice({
    name: "line-1-meter",
    modelName: "power-meter",
    hardwareId: "tg-0042",
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
  options: { query?: string; preferences?: Partial<Preferences> } = {},
) {
  const identity = signedInBrowser(options.preferences ? { preferences: options.preferences } : {}).browser;
  const rendered = renderAt(`${devicePath(deviceId)}${options.query ?? ""}`, {
    identity,
    devices: server.api(),
  });
  await screen.findByRole("heading", { level: 1 });
  return rendered;
}

/** The latest values table's row of a data point. */
function latestRow(point: string): HTMLElement {
  const table = within(screen.getByRole("region", { name: "Latest stored values" })).getByRole("table");
  const header = within(table).getByRole("rowheader", { name: new RegExp(`^${point}`) });
  return header.closest("tr") as HTMLElement;
}

async function showTable() {
  const button = await screen.findByRole("button", { name: /^Show as a table/ });
  await act(async () => {
    fireEvent.click(button);
  });
  const history = screen.getByRole("region", { name: "History" });
  return within(history).getByRole("table");
}

/** The series the page asked for, asked again of the same API. */
async function lastSeriesAnswer(server: FakeDeviceServer): Promise<Series> {
  const url = server.seriesRequests().at(-1) as URL;
  const response = await server.fetch(new Request(url));
  return (await response.json()) as Series;
}

async function chooseRange(value: string) {
  await act(async () => {
    fireEvent.change(screen.getByLabelText("Time range"), { target: { value } });
  });
}

async function customRange(from: string, to: string) {
  await chooseRange("custom");
  await act(async () => {
    fireEvent.change(screen.getByLabelText("From"), { target: { value: from } });
    fireEvent.change(screen.getByLabelText("To"), { target: { value: to } });
  });
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Show" }));
  });
}

describe("DevicePage", () => {
  test("shows the device, its model and the newest stored value of each data point", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    expect(screen.getByRole("heading", { level: 1, name: "line-1-meter" })).toBeDefined();
    await screen.findByText("power-meter");
    expect(screen.getByText("tg-0042")).toBeDefined();
    expect(screen.getByText("Modbus TCP, 192.0.2.10:502, unit 1")).toBeDefined();
    expect(screen.getByText("Every 1 minute")).toBeDefined();

    // Every stored value is the one the latest API answers, with its time.
    const latest = await server.api().getLatest(device.device_id);
    expect(latest.points.map((point) => point.point)).toEqual(["breaker", "energy_kwh", "status", "voltage"]);
    await waitFor(() => expect(within(latestRow("voltage")).queryByText("…")).toBeNull());
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
            : numbers.format(value.value ?? Number.NaN);
      const row = latestRow(point.point);
      expect(within(row).getByText(expected[point.point] as string)).toBeDefined();
      expect(row.querySelector("time")?.getAttribute("datetime")).toBe(point.time);
    }
    // The model's order, with the point's description.
    const rows = within(screen.getByRole("region", { name: "Latest stored values" })).getAllByRole("rowheader");
    expect(rows.map((row) => row.textContent)).toEqual(["voltagePhase A voltage", "energy_kwh", "breaker", "status"]);
  });

  test("the chart shows the series the history API answers, in the user's time zone", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id, { preferences: { timeZone: "Asia/Taipei" } });
    const history = screen.getByRole("region", { name: "History" });
    await within(history).findByTestId("history-chart");

    // The first chartable point, the last 24 hours, the user's zone.
    const request = server.seriesRequests().at(-1) as URL;
    expect(request.searchParams.get("point")).toBe("voltage");
    expect(request.searchParams.get("tz")).toBe("Asia/Taipei");
    const from = Date.parse(request.searchParams.get("from") ?? "");
    const to = Date.parse(request.searchParams.get("to") ?? "");
    expect(to - from).toBe(DAY);
    expect(Math.abs(Date.now() - to)).toBeLessThan(HOUR);
    expect(screen.getByText("Stored readings of one data point, in your time zone (Asia/Taipei).")).toBeDefined();

    // The line has one point per bucket of the answer, and the table lists exactly those buckets.
    const answer = await lastSeriesAnswer(server);
    expect(answer.points.length).toBe(120);
    const lines = within(history).getAllByTestId("history-line");
    const commands = lines.flatMap((line) => (line.getAttribute("d") ?? "").split(" "));
    expect(commands.length).toBe(answer.points.length);
    const table = await showTable();
    const bodyRows = within(table).getAllByRole("row").slice(1);
    expect(bodyRows.length).toBe(answer.points.length);
    answer.points.forEach((bucket, index) => {
      const row = bodyRows[index] as HTMLElement;
      expect(row.querySelector("time")?.getAttribute("datetime")).toBe(bucket.time);
      const cells = within(row)
        .getAllByRole("cell")
        .map((cell) => cell.textContent);
      expect(cells).toEqual([
        numbers.format(bucket.avg as number),
        numbers.format(bucket.min as number),
        numbers.format(bucket.max as number),
        numbers.format(bucket.last.value as number),
        String(bucket.count),
      ]);
    });
    expect(screen.getByText(/Each point on the line is the mean over 1 minute/)).toBeDefined();
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
  });

  test("a row's chart button and a deep link choose the point and the range", async () => {
    const { server, device } = meter();
    const { router } = await openDevice(server, device.device_id, { query: "?point=energy_kwh&range=7d" });
    await screen.findByTestId("history-chart");
    let request = server.seriesRequests().at(-1) as URL;
    expect(request.searchParams.get("point")).toBe("energy_kwh");
    expect(Date.parse(request.searchParams.get("to") ?? "") - Date.parse(request.searchParams.get("from") ?? "")).toBe(
      7 * DAY,
    );
    expect((screen.getByLabelText("Time range") as HTMLSelectElement).value).toBe("7d");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Show breaker on the chart" }));
    });
    await waitFor(() => expect(server.seriesRequests().at(-1)?.searchParams.get("point")).toBe("breaker"));
    expect(router.state.location.search).toBe("?point=breaker&range=7d");
    expect(screen.getByRole("button", { name: "Show breaker on the chart" }).getAttribute("aria-pressed")).toBe("true");
    // A bool point is the share of readings that were on.
    expect(await screen.findByText(/share of readings that were on/)).toBeDefined();
    request = server.seriesRequests().at(-1) as URL;
    expect(request.searchParams.get("point")).toBe("breaker");
  });

  test("a range wholly before the queryable period explains it and offers the period, not a blank chart", async () => {
    const { server, device } = meter();
    server.aggregatedPeriodMs = 90 * DAY;
    await openDevice(server, device.device_id);
    await screen.findByTestId("history-chart");
    await customRange("2025-01-01T00:00", "2025-01-02T00:00");

    expect(await screen.findByText("This range is outside the queryable period")).toBeDefined();
    expect(screen.queryByTestId("history-chart")).toBeNull();
    const refused = await server.fetch(new Request(server.seriesRequests().at(-1) as URL));
    expect(refused.status).toBe(403);
    const earliest = Date.parse(((await refused.json()) as { details: { earliest: string } }).details.earliest);
    const minute = new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" });
    expect(screen.getByText(new RegExp(`queried from ${minute.format(earliest)} on`))).toBeDefined();

    // The next step: the queryable period up to now.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: `Show from ${minute.format(earliest)} until now` }));
    });
    await screen.findByTestId("history-chart");
    expect(Date.parse(server.seriesRequests().at(-1)?.searchParams.get("from") ?? "")).toBe(earliest);
  });

  test("a range that starts before the queryable period says where the chart starts", async () => {
    const { server, device } = meter();
    server.aggregatedPeriodMs = 60 * DAY;
    await openDevice(server, device.device_id, { query: "?range=90d" });
    expect(await screen.findByText(/The range starts before the queryable period/)).toBeDefined();
    expect(screen.getByTestId("history-chart")).toBeDefined();
  });

  test("a range without readings says so and suggests a longer one", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    await screen.findByTestId("history-chart");
    const day = new Date(Date.now() - 10 * DAY).toISOString().slice(0, 10);
    await customRange(`${day}T00:00`, `${day}T06:00`);
    expect(await screen.findByText("No readings in this range")).toBeDefined();
    expect(screen.queryByTestId("history-chart")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Show the last 7 days" }));
    });
    await screen.findByTestId("history-chart");
    expect((screen.getByLabelText("Time range") as HTMLSelectElement).value).toBe("7d");
  });

  test("a custom range whose end is not after its start is refused before any request", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id);
    await screen.findByTestId("history-chart");
    const before = server.seriesRequests().length;
    await customRange("2026-10-02T00:00", "2026-10-01T00:00");
    expect(screen.getByText("The end must be after the start.")).toBeDefined();
    expect(screen.getByLabelText("To").getAttribute("aria-invalid")).toBe("true");
    expect(server.seriesRequests().length).toBe(before);
  });

  test("a text point lists its readings instead of a chart", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id, { query: "?point=status" });
    expect(await screen.findByText(/status holds text, which can't be drawn as a chart/)).toBeDefined();
    expect(screen.queryByTestId("history-chart")).toBeNull();
    const table = within(screen.getByRole("region", { name: "History" })).getByRole("table");
    expect(within(table).getAllByText("ok").length).toBeGreaterThan(0);
    // Text has no mean, minimum or maximum, so the table has no such columns.
    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["From", "Last reading", "Readings"]);
  });

  test("a device without stored readings tells what to check", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ hardwareId: "tg-0007", points: [{ key: "voltage", data_type: "f32" }] });
    await openDevice(server, device.device_id);
    expect(await screen.findByText("No stored readings yet")).toBeDefined();
    expect(screen.getByText(/Check that gateway tg-0007 is online/)).toBeDefined();
    expect(within(latestRow("voltage")).getByText("No reading")).toBeDefined();
    expect(await screen.findByText("No readings in this range")).toBeDefined();
  });

  test("a model without data points says how to add them", async () => {
    const server = new FakeDeviceServer();
    const device = server.addDevice({ modelName: "empty-model", points: [] });
    await openDevice(server, device.device_id);
    expect(await screen.findByText("This device has no data points")).toBeDefined();
    expect(screen.getByText(/the device's model \(empty-model\)/)).toBeDefined();
    expect(screen.getByText("Nothing to chart yet")).toBeDefined();
    expect(server.seriesRequests()).toEqual([]);
  });

  test("an unknown or malformed device ID is not found", async () => {
    const server = new FakeDeviceServer();
    await openDevice(server, "00000000-0000-4000-8000-0000000fffff");
    expect(screen.getByRole("heading", { level: 1, name: "Device not found" })).toBeDefined();
    expect(screen.getByText(/Acme Factory has no device with this address/)).toBeDefined();

    cleanup();
    const before = server.requests.length;
    await openDevice(server, "not-a-device");
    expect(screen.getByRole("heading", { level: 1, name: "Device not found" })).toBeDefined();
    expect(server.requests.length).toBe(before);
  });

  test("a failed request says why and can be retried", async () => {
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
    await screen.findByTestId("history-chart");
    await waitFor(() => expect(within(latestRow("voltage")).queryByText("No reading")).toBeNull());
  });

  test("the page follows the account's language", async () => {
    const { server, device } = meter();
    await openDevice(server, device.device_id, { preferences: { language: "zh-TW" } });
    expect(await screen.findByRole("heading", { name: "最新已存值" })).toBeDefined();
    expect(screen.getByRole("heading", { name: "歷史" })).toBeDefined();
  });
});
