// The units of a model's data points on the device page (Laysi/trustiot-v2#982): the readings
// list, the figures over the chart, the chart's value axis, tooltip and slider text carry the
// model's unit; a point without one looks as it did before units existed.
import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, screen, waitFor, within } from "@testing-library/react";

import { devicePath } from "../../app/paths.ts";
import { type FakeChart, fakeEngine } from "../../test/fake-chart.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

const MINUTE = 60_000;
const numbers = new Intl.NumberFormat("en", { maximumFractionDigits: 3 });

/**
 * The demo's temperature and humidity transmitter, read every minute for the last hour: the
 * temperature has a unit, the humidity has `humidity` (or none), a counter has none, and a
 * coil's unit is never written (a bool reads on or off).
 */
function transmitter(humidity: string | undefined) {
  const server = new FakeDeviceServer();
  const device = server.addDevice({
    name: "room-sensor",
    modelName: "ebyte-eid041-temp-humidity",
    points: [
      { key: "temperature", data_type: "i16", description: "Air temperature", unit: "°C" },
      { key: "humidity", data_type: "u16", description: "Relative humidity", unit: humidity },
      { key: "uptime", data_type: "u32" },
      { key: "alarm", data_type: "bool", unit: "state" },
    ],
  });
  const minuteStart = Math.floor(server.now() / MINUTE) * MINUTE;
  for (let minute = 60; minute >= 1; minute -= 1) {
    const time = minuteStart - minute * MINUTE + 7_000;
    server.addReadings(device.device_id, "temperature", [[time, { type: "float", value: 21 + (minute % 4) * 0.25 }]]);
    server.addReadings(device.device_id, "humidity", [[time, { type: "float", value: 55 + (minute % 3) }]]);
    server.addReadings(device.device_id, "uptime", [[time, { type: "int", value: 1000 + minute }]]);
    server.addReadings(device.device_id, "alarm", [[time, { type: "bool", value: false }]]);
  }
  return { server, device };
}

async function openDevice(server: FakeDeviceServer, deviceId: string, query = "") {
  const identity = signedInBrowser().browser;
  const chart = fakeEngine();
  renderAt(`${devicePath(deviceId)}${query}`, { identity, devices: server.api(), chartEngine: chart.engine });
  await screen.findByRole("heading", { level: 1 });
  await screen.findByRole("slider");
  await waitFor(() => expect(chart.charts[0]?.options.length).toBeGreaterThan(0));
  return chart.charts[0] as FakeChart;
}

const latestRegion = () => screen.getByRole("region", { name: "Current readings" });
const historyRegion = () => screen.getByRole("region", { name: /^History of / });
const readingRow = (label: string) => within(latestRegion()).getByRole("button", { name: new RegExp(label) });
const yAxisOf = (chart: FakeChart) => chart.option.yAxis as Record<string, unknown>;
const figure = (name: string) => within(historyRegion()).getByText(name).closest("div") as HTMLElement;
const tooltipValues = () =>
  within(screen.getByTestId("chart-tooltip"))
    .queryAllByTestId("tip-value")
    .map((element) => element.textContent ?? "");

describe("units on the device page", () => {
  test("readings, figures, the value axis, the tooltip and the slider carry the point's unit", async () => {
    const { server, device } = transmitter("%RH");
    const chart = await openDevice(server, device.device_id, "?point=temperature");

    // The readings list: each number followed by its unit; none for a point without one or a bool.
    expect(readingRow("Air temperature").textContent).toMatch(/\d\.\d\d°C/);
    expect(within(readingRow("Air temperature")).getByText("°C")).toBeDefined();
    expect(within(readingRow("Relative humidity")).getByText("%RH")).toBeDefined();
    expect(within(readingRow("uptime")).queryByText(/°C|%RH/)).toBeNull();
    expect(within(readingRow("alarm")).queryByText("state")).toBeNull();

    // The figures over the chart and the chart itself.
    expect(within(figure("Mean")).getByText(/ °C$/)).toBeDefined();
    expect(within(figure("Min")).getByText(`${numbers.format(21)} °C`)).toBeDefined();
    expect(within(figure("Max")).getByText(`${numbers.format(21.75)} °C`)).toBeDefined();
    expect(yAxisOf(chart).name).toBe("°C");
    act(() => chart.showTip(0, 0));
    const tip = tooltipValues();
    expect(tip.slice(0, 3).every((value) => value.endsWith(" °C"))).toBe(true);
    // The readings count is a count, not a measurement.
    expect(tip[3]?.endsWith("°C")).toBe(false);
    const slider = screen.getByRole("slider");
    await act(async () => {
      slider.focus();
    });
    expect(slider.getAttribute("aria-valuetext")).toMatch(/mean [\d.,]+ °C, min [\d.,]+ °C/);
  });

  test("a point without a unit looks as it did before units existed", async () => {
    const { server, device } = transmitter(undefined);
    const chart = await openDevice(server, device.device_id, "?point=humidity");

    const value = within(readingRow("Relative humidity")).getByText(/^\d+\.\d\d$/);
    expect(value.children.length).toBe(0);
    expect(yAxisOf(chart).name).toBeUndefined();
    expect(within(figure("Min")).getByText(numbers.format(55))).toBeDefined();
    act(() => chart.showTip(0, 0));
    for (const text of tooltipValues()) expect(text).toMatch(/^[\d.,]+$/);
  });

  test("a unit is the tenant's text: shown as written, never as markup", async () => {
    const { server, device } = transmitter("<b>%</b>");
    const chart = await openDevice(server, device.device_id, "?point=humidity");
    const row = readingRow("Relative humidity");
    expect(within(row).getByText("<b>%</b>")).toBeDefined();
    expect(row.querySelector("b")).toBeNull();
    expect(yAxisOf(chart).name).toBe("<b>%</b>");
    act(() => chart.showTip(0, 0));
    expect(tooltipValues()[0]?.endsWith(" <b>%</b>")).toBe(true);
    expect(screen.getByTestId("chart-tooltip").querySelector("b")).toBeNull();
  });
});
