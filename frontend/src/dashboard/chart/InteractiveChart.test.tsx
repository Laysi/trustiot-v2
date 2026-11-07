// What a chart keeps of the point a reader is on when new data arrives (Laysi/trustiot-v2#1156):
// the stop's time or category, not its index. The shared component is driven directly with a small
// data model, then through the three real users: the dashboard's time series, bar and sparkline and
// the device page's history chart. ECharts is the fake of test/fake-chart.ts: a `showTip` action
// answers with the same `showTip` event ECharts sends (the hit and a position), which is what the
// component relies on to put the tooltip back where the new data has the point; the browser check
// of the issue covers the real library.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";

import { translate } from "../../i18n/translate.ts";
import type { Datum } from "../../pages/device/chart.ts";
import type { DeviceFormat } from "../../pages/device/format.ts";
import { HistoryChart } from "../../pages/device/HistoryChart.tsx";
import { historyModel } from "../../pages/device/historyOption.ts";
import { type FakeChart, fakeEngine } from "../../test/fake-chart.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { DashboardEnvProvider } from "../context.tsx";
import { DashboardComponentView } from "../DashboardComponent.tsx";
import type { DashboardData, DatasetResult } from "../data.ts";
import { DAILY_ROWS, fixtureData, POWER_ROWS, SPECS, TREND_ROWS } from "../fixtures.ts";
import { dashboardFormat, wallMs } from "../format.ts";
import { type ChartEngine, ChartEngineContext } from "./engine.ts";
import { InteractiveChart } from "./InteractiveChart.tsx";

// The waits below are for a rendered chart and its options, which a loaded runner can delay.
allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

/** happy-dom lays nothing out: give the chart's frame a size so the tooltip's position is computed. */
const FRAME_WIDTH = 1000;
const FRAME_HEIGHT = 300;
const sizes: Array<[string, PropertyDescriptor | undefined]> = [];
beforeEach(() => {
  for (const [name, value] of [
    ["clientWidth", FRAME_WIDTH],
    ["clientHeight", FRAME_HEIGHT],
  ] as const) {
    sizes.push([name, Object.getOwnPropertyDescriptor(HTMLElement.prototype, name)]);
    Object.defineProperty(HTMLElement.prototype, name, { configurable: true, get: () => value });
  }
});
afterEach(() => {
  for (const [name, original] of sizes.splice(0)) {
    if (original === undefined) delete (HTMLElement.prototype as unknown as Record<string, unknown>)[name];
    else Object.defineProperty(HTMLElement.prototype, name, original);
  }
});

async function chartOf(charts: FakeChart[]): Promise<FakeChart> {
  await waitFor(() => {
    if (charts[0] === undefined || charts[0].options.length === 0) throw new Error("waiting for the chart's option");
  });
  return charts[0] as FakeChart;
}

/** Where the fake puts the tooltip for a data item: further right for a later one. */
function placeByIndex(chart: FakeChart): void {
  chart.tipPosition = (_series, dataIndex) => ({ x: 100 + dataIndex * 10, y: 50 });
}

function tooltip(): HTMLElement | null {
  return screen.queryByTestId("chart-tooltip");
}

function tipTime(): string | null {
  return screen.queryByTestId("tip-time")?.textContent ?? null;
}

function tipLeft(): number {
  return Number.parseFloat((tooltip() as HTMLElement).style.left);
}

describe("the shared chart component", () => {
  /** Stops named by a time, as the time series are. */
  function Harness(props: { times: readonly number[]; empty?: ReadonlySet<number>; engine: ChartEngine }) {
    const { times, empty } = props;
    return (
      <ChartEngineContext.Provider value={props.engine}>
        <DashboardEnvProvider locale="en" timeZone="UTC">
          <InteractiveChart
            label="Demo"
            size="chart"
            buildOption={() => ({ times })}
            slots={[0]}
            hiddenSlots={new Set<number>()}
            zoom={false}
            stopCount={times.length}
            stopKey={(stop) => times[stop]}
            hitOf={(stop) => (stop < times.length ? { seriesIndex: 0, dataIndex: stop } : null)}
            stopOf={(hit) => (hit.dataIndex < times.length ? hit.dataIndex : null)}
            describe={(stop) => `time ${times[stop]}`}
            renderTip={(stop) => {
              const time = times[stop];
              return time === undefined || empty?.has(time) ? null : <p data-testid="tip-time">{`time ${time}`}</p>;
            }}
            onSource={() => {}}
          />
        </DashboardEnvProvider>
      </ChartEngineContext.Provider>
    );
  }

  function show(times: readonly number[], empty?: ReadonlySet<number>) {
    const { engine, charts } = fakeEngine();
    const view = render(<Harness times={times} {...(empty ? { empty } : {})} engine={engine} />);
    const update = (next: readonly number[], nextEmpty?: ReadonlySet<number>) =>
      view.rerender(<Harness times={next} {...(nextEmpty ? { empty: nextEmpty } : {})} engine={engine} />);
    return { charts, update };
  }

  test("data that slides by one step keeps the tooltip on the same time, re-placed at its new position", async () => {
    const { charts, update } = show([10, 20, 30, 40]);
    const chart = await chartOf(charts);
    placeByIndex(chart);
    act(() => chart.showTip(0, 2, 120, 50));
    expect(tipTime()).toBe("time 30");
    expect(tipLeft()).toBe(120 + 12);
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuenow")).toBe("2");

    update([20, 30, 40, 50]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    // Time 30 is now stop 1: the tooltip, the slider and ECharts' own tooltip all follow it.
    expect(tipTime()).toBe("time 30");
    expect(slider.getAttribute("aria-valuenow")).toBe("1");
    expect(slider.getAttribute("aria-valuetext")).toBe("time 30");
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 1 });
    expect(tipLeft()).toBe(100 + 1 * 10 + 12);
  });

  test("the first render of new data already shows the same time, with no frame on another one", async () => {
    const { charts, update } = show([10, 20, 30, 40]);
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 2));
    const seen: Array<string | null> = [];
    const observer = new MutationObserver(() => seen.push(tipTime()));
    observer.observe(document.body, { subtree: true, childList: true, characterData: true });
    update([20, 30, 40, 50]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    observer.disconnect();
    expect(seen.filter((time) => time !== null && time !== "time 30")).toEqual([]);
  });

  test("a selection whose time is not in the new data closes the tooltip and the slider falls back to the last stop", async () => {
    const { charts, update } = show([10, 20, 30, 40]);
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 0));
    expect(tipTime()).toBe("time 10");

    update([20, 30, 40, 50]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tooltip()).toBeNull();
    expect(chart.actions.at(-1)).toEqual({ type: "hideTip" });
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuenow")).toBe("3");
    expect(slider.getAttribute("aria-valuetext")).toBe("time 50");
  });

  test("data stalled: one point fewer keeps the newest time selected and draws no empty box", async () => {
    const { charts, update } = show([10, 20, 30, 40]);
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 3));
    // The window moved on and nothing new came: the old index 3 is out of range now.
    update([20, 30, 40]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe("time 40");
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuenow")).toBe("2");
    expect(Number(slider.getAttribute("aria-valuenow"))).toBeLessThanOrEqual(
      Number(slider.getAttribute("aria-valuemax")),
    );
    // Then the newest time leaves too: closed, never an empty box.
    update([20, 30]);
    await waitFor(() => expect(chart.options).toHaveLength(3));
    expect(tooltip()).toBeNull();
  });

  test("a stop the tooltip has nothing to say about draws no box", async () => {
    const { charts } = show([10, 20, 30], new Set([20]));
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 1));
    expect(tooltip()).toBeNull();
    act(() => chart.showTip(0, 2));
    expect(tipTime()).toBe("time 30");
    act(() => chart.showTip(0, 1));
    expect(tooltip()).toBeNull();
  });

  test("a stop that has nothing to say after an update draws no box either", async () => {
    const { charts, update } = show([10, 20, 30]);
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 1));
    expect(tipTime()).toBe("time 20");
    update([10, 20, 30], new Set([20]));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tooltip()).toBeNull();
  });

  test("a keyboard selection stays on its time across an update and goes on from there", async () => {
    const { charts, update } = show([10, 20, 30, 40, 50]);
    const chart = await chartOf(charts);
    const slider = screen.getByRole("slider");
    act(() => slider.focus());
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(slider.getAttribute("aria-valuetext")).toBe("time 30");
    expect(slider.getAttribute("aria-valuenow")).toBe("2");

    update([20, 30, 40, 50, 60]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(slider.getAttribute("aria-valuetext")).toBe("time 30");
    expect(slider.getAttribute("aria-valuenow")).toBe("1");
    expect(tipTime()).toBe("time 30");
    // The next key press steps from the time the reader is on.
    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(slider.getAttribute("aria-valuetext")).toBe("time 40");
    expect(tipTime()).toBe("time 40");
  });

  test("a keyboard selection whose time left the data falls back the way no selection does", async () => {
    const { charts, update } = show([10, 20, 30]);
    const chart = await chartOf(charts);
    const slider = screen.getByRole("slider");
    act(() => slider.focus());
    fireEvent.keyDown(slider, { key: "Home" });
    update([20, 30, 40]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tooltip()).toBeNull();
    expect(slider.getAttribute("aria-valuenow")).toBe("2");
    // The next arrow key starts from the last stop, as it does with nothing selected.
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(slider.getAttribute("aria-valuetext")).toBe("time 30");
  });

  test("with nothing selected, an update shows no tooltip", async () => {
    const { charts, update } = show([10, 20, 30]);
    const chart = await chartOf(charts);
    update([20, 30, 40]);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(chart.actions.filter((action) => action.type === "showTip" || action.type === "hideTip")).toEqual([]);
    expect(tooltip()).toBeNull();
  });

  test("the same data again (a theme change) shows the selected time again at its position", async () => {
    const { charts, update } = show([10, 20, 30]);
    const chart = await chartOf(charts);
    placeByIndex(chart);
    act(() => chart.showTip(0, 1, 110, 50));
    expect(tipLeft()).toBe(110 + 12);
    update([10, 20, 30]);
    // A new array of the same times is a new option as far as the component can tell.
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe("time 20");
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 1 });
    expect(tipLeft()).toBe(100 + 10 + 12);
  });
});

describe("the dashboard's charts", () => {
  const format = dashboardFormat("en", "UTC");

  function inEnv(engine: ChartEngine, children: ReactNode) {
    return (
      <ChartEngineContext.Provider value={engine}>
        <DashboardEnvProvider locale="en" timeZone="UTC">
          {children}
        </DashboardEnvProvider>
      </ChartEngineContext.Provider>
    );
  }

  /** The fixture with one dataset's rows replaced. */
  function withRows(dataset: string, rows: DatasetResult["rows"]) {
    const data = fixtureData();
    const state = data.results[dataset];
    if (state?.status !== "ready") throw new Error(`no result for ${dataset}`);
    return {
      ...data,
      results: {
        ...data.results,
        [dataset]: { status: "ready" as const, result: { ...state.result, rows, row_count: rows.length } },
      },
    };
  }

  function showComponent(spec: Parameters<typeof DashboardComponentView>[0]["spec"], data: DashboardData) {
    const { engine, charts } = fakeEngine();
    const view = render(inEnv(engine, <DashboardComponentView spec={spec} data={data} />));
    const update = (next: DashboardData) =>
      view.rerender(inEnv(engine, <DashboardComponentView spec={spec} data={next} />));
    return { charts, update };
  }

  test("a time series keeps the time it is on when the window slides by a step", async () => {
    // 00:00 leaves, 01:00 arrives.
    const slid = [
      ...POWER_ROWS.slice(2),
      ["2026-11-01 01:00:00", "Line A", 3.5],
      ["2026-11-01 01:00:00", "Line B", 1260],
    ];
    const { charts, update } = showComponent(SPECS.timeSeries, fixtureData());
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 1));
    const time = format.wallTime(wallMs("2026-11-01 00:15:00") as number);
    expect(tipTime()).toBe(time);
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuenow")).toBe("1");

    update(withRows("power", slid));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe(time);
    expect(slider.getAttribute("aria-valuenow")).toBe("0");
    expect(slider.getAttribute("aria-valuetext")).toContain(time);
    // Line A has 00:15 as its first point now.
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 0 });
  });

  test("a time series closes the tooltip when its time left the data", async () => {
    const slid = [...POWER_ROWS.slice(2), ["2026-11-01 01:00:00", "Line B", 1260]];
    const { charts, update } = showComponent(SPECS.timeSeries, fixtureData());
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 0));
    expect(tooltip()).not.toBeNull();
    update(withRows("power", slid));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tooltip()).toBeNull();
    expect(chart.actions.at(-1)).toEqual({ type: "hideTip" });
  });

  test("a bar chart keeps the category it is on, and closes when the category left", async () => {
    const slid = [...DAILY_ROWS.slice(2), ["Thu", "Site 1", 7], ["Thu", "Site 2", 8]];
    const { charts, update } = showComponent(SPECS.bar, fixtureData());
    const chart = await chartOf(charts);
    const slider = screen.getByRole("slider");
    act(() => chart.showTip(0, 1));
    expect(tipTime()).toBe("Tue");
    expect(slider.getAttribute("aria-valuenow")).toBe("1");

    update(withRows("daily", slid));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe("Tue");
    expect(slider.getAttribute("aria-valuenow")).toBe("0");
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 0 });

    // Tue itself leaves in the next update.
    update(
      withRows("daily", [
        ["Wed", "Site 1", 0.5],
        ["Thu", "Site 1", 7],
      ]),
    );
    await waitFor(() => expect(chart.options).toHaveLength(3));
    expect(tooltip()).toBeNull();
    expect(slider.getAttribute("aria-valuenow")).toBe("1");
  });

  test("a sparkline keeps the point it is on when its trend slides", async () => {
    const slid = [...TREND_ROWS.slice(1), ["2026-10-31", 330]];
    const { charts, update } = showComponent(SPECS.kpi, fixtureData());
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, 2));
    const time = format.wallTime(wallMs("2026-10-28") as number, true);
    expect(tipTime()).toBe(time);
    update(withRows("trend", slid));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe(time);
    expect(screen.getByRole("slider").getAttribute("aria-valuenow")).toBe("1");
  });
});

describe("the device page's history chart", () => {
  const MINUTE = 60_000;
  const START = Date.parse("2026-11-01T00:00:00Z");
  const numbers = new Intl.NumberFormat("en", { maximumFractionDigits: 3 });
  const minutes = new Intl.DateTimeFormat("en", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" });
  const percent = new Intl.NumberFormat("en", { style: "percent", maximumFractionDigits: 1 });
  const format: DeviceFormat = {
    t: (id, values) => translate("en", id, values),
    locale: "en",
    timeZone: "UTC",
    time: (instant) => minutes.format(new Date(instant)),
    minute: (instant) => minutes.format(new Date(instant)),
    number: (value) => numbers.format(value),
    unit: null,
    measure: (value) => numbers.format(value),
    fixed: (value) => numbers.format(value),
    reading: (reading) => String(reading.value),
    share: (value) => percent.format(value),
    date: (instant) => minutes.format(new Date(instant)),
    duration: (ms) => `${ms / MINUTE} minutes`,
    ago: (ms) => `${ms / MINUTE} minutes ago`,
    elapsed: (ms) => `${ms / MINUTE} minutes`,
  };

  /** One bucket of `step` minutes per entry, each with a mean of its own. */
  function buckets(first: number, last: number, step = 1): Datum[] {
    return Array.from({ length: Math.floor((last - first) / step) + 1 }, (_, index) => {
      const at = first + index * step;
      return { time: START + at * MINUTE, value: 200 + at, min: 199 + at, max: 201 + at, count: step };
    });
  }

  function Chart(props: { data: Datum[]; engine: ChartEngine }) {
    return (
      <ChartEngineContext.Provider value={props.engine}>
        <HistoryChart
          data={props.data}
          kind="number"
          gaps={[]}
          from={START}
          to={START + 3 * 60 * MINUTE}
          label="Voltage history"
          title="Voltage"
          format={format}
          onZoom={() => {}}
          onSetRange={() => {}}
          onOpenTable={() => {}}
        />
      </ChartEngineContext.Provider>
    );
  }

  function showHistory(data: Datum[]) {
    const { engine, charts } = fakeEngine();
    const view = render(<Chart data={data} engine={engine} />);
    const update = (next: Datum[]) => view.rerender(<Chart data={next} engine={engine} />);
    const point = (data: Datum[], stop: number) => historyModel(data, [], "UTC").pointOfStop[stop] as number;
    return { charts, update, point };
  }

  const at = (minute: number) => minutes.format(new Date(START + minute * MINUTE));

  test("the once-a-minute slide keeps the hovered minute's tooltip on that minute", async () => {
    const before = buckets(0, 59);
    const { charts, update, point } = showHistory(before);
    const chart = await chartOf(charts);
    placeByIndex(chart);
    act(() => chart.showTip(0, point(before, 30), 400, 50));
    expect(tipTime()).toBe(at(30));
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuenow")).toBe("30");
    const left = tipLeft();

    const after = buckets(1, 60);
    update(after);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe(at(30));
    expect(slider.getAttribute("aria-valuenow")).toBe("29");
    expect(slider.getAttribute("aria-valuetext")).toContain(at(30));
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: point(after, 29) });
    // Re-placed at the new position of the point, not left at the old pointer's coordinates.
    expect(tipLeft()).toBe(100 + point(after, 29) * 10 + 12);
    expect(tipLeft()).not.toBe(left);
  });

  test("buckets of five minutes: a bucket stays selected until it leaves, and new readings in the newest one change its figures, not its time", async () => {
    const before = buckets(0, 55, 5);
    const { charts, update, point } = showHistory(before);
    const chart = await chartOf(charts);
    act(() => chart.showTip(0, point(before, 11)));
    expect(tipTime()).toBe(at(55));

    // A bucket boundary is crossed: one bucket arrives at the end, the first one leaves.
    const crossed = buckets(5, 60, 5);
    update(crossed);
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe(at(55));
    expect(screen.getByRole("slider").getAttribute("aria-valuenow")).toBe("10");

    // Inside the same bucket the newest one gets another reading: same time, new figures.
    const grown = crossed.map((datum, index) => (index === 10 ? { ...datum, count: 5, value: 777 } : datum));
    update(grown);
    await waitFor(() => expect(chart.options).toHaveLength(3));
    expect(tipTime()).toBe(at(55));
    expect(screen.getByTestId("chart-tooltip").textContent).toContain("777");

    // The bucket is gone once the window is past it.
    update(buckets(60, 115, 5));
    await waitFor(() => expect(chart.options).toHaveLength(4));
    expect(tooltip()).toBeNull();
  });

  test("stalled readings: the oldest minute leaves and the newest stays selected, with no empty box", async () => {
    const before = buckets(0, 59);
    const { charts, update, point } = showHistory(before);
    const chart = await chartOf(charts);
    const slider = screen.getByRole("slider");
    act(() => slider.focus());
    // Focus selects the newest stop.
    expect(slider.getAttribute("aria-valuenow")).toBe("59");
    expect(tipTime()).toBe(at(59));

    update(buckets(1, 59));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tipTime()).toBe(at(59));
    expect(slider.getAttribute("aria-valuenow")).toBe("58");
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: point(buckets(1, 59), 58) });
    expect((screen.getByTestId("chart-tooltip").textContent ?? "").trim()).not.toBe("");
  });

  test("a keyboard selection on a minute that left the data closes the tooltip", async () => {
    const { charts, update } = showHistory(buckets(0, 59));
    const chart = await chartOf(charts);
    const slider = screen.getByRole("slider");
    act(() => slider.focus());
    fireEvent.keyDown(slider, { key: "Home" });
    expect(tipTime()).toBe(at(0));
    update(buckets(1, 60));
    await waitFor(() => expect(chart.options).toHaveLength(2));
    expect(tooltip()).toBeNull();
    expect(slider.getAttribute("aria-valuenow")).toBe("59");
  });
});
