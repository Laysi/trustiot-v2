import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import type { ReactNode } from "react";

import type { Locale } from "../i18n/messages.ts";
import { until } from "../test/until.ts";
import { DashboardEnvProvider } from "./context.tsx";
import type { DashboardData, Dataset } from "./data.ts";
import { type ComponentExtras, ComponentExtrasProvider } from "./extensions.tsx";
import { fixtureData, POWER_ROWS, requestDataset, result } from "./fixtures.ts";
import { dashboardFormat, wallMs } from "./format.ts";
import type { SourceRef } from "./model.ts";
import { periodText, type SemanticRequest } from "./RequestSummary.tsx";
import { SourcePanel } from "./SourcePanel.tsx";

const DEVICE = "7d1f4c52-0a39-4c8e-9a6b-2f4a1b3c5d70";
const OTHER_DEVICE = "0b8a2e91-5c46-4d17-8f03-6a9d7e1c2b48";

const originalClipboard = Object.getOwnPropertyDescriptor(navigator, "clipboard");

afterEach(() => {
  cleanup();
  if (originalClipboard) Object.defineProperty(navigator, "clipboard", originalClipboard);
  else Reflect.deleteProperty(navigator, "clipboard");
});

/** The clipboard the page gets: `writeText` records what it was given and then does `outcome`. */
function clipboard(outcome: "works" | "refused" | "missing"): string[] {
  const written: string[] = [];
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value:
      outcome === "missing"
        ? undefined
        : {
            writeText: async (text: string) => {
              written.push(text);
              if (outcome === "refused") throw new DOMException("denied", "NotAllowedError");
            },
          },
  });
  return written;
}

function request(overrides: Partial<SemanticRequest> = {}): SemanticRequest {
  return {
    series: [{ device_id: DEVICE, point: "active_power" }],
    tz: "Asia/Taipei",
    period: "last_7_days",
    interval: "1d",
    ...overrides,
  };
}

/** The fixture data with a statistics and a usage dataset (and a result for each) added. */
function dataWith(...datasets: Dataset[]): DashboardData {
  const data = fixtureData();
  const results = { ...data.results };
  for (const dataset of datasets) {
    results[dataset.name] = {
      status: "ready",
      result: result(
        dataset.name,
        [
          { name: "bucket", type: "time" },
          { name: "value", type: "number" },
        ],
        [
          ["2026-10-26 00:00:00", 1.5],
          ["2026-10-27 00:00:00", 2.5],
        ],
      ),
    };
  }
  return { ...data, datasets: [...data.datasets, ...datasets], results };
}

function show(
  source: SourceRef,
  options: {
    data?: DashboardData;
    extras?: ComponentExtras;
    locale?: Locale;
    wrap?: (node: ReactNode) => ReactNode;
  } = {},
) {
  const locale = options.locale ?? "en";
  const panel = <SourcePanel source={source} data={options.data ?? fixtureData()} onClose={() => {}} />;
  const extras = options.extras;
  return render(
    <DashboardEnvProvider locale={locale} timeZone="UTC">
      {extras ? <ComponentExtrasProvider extras={extras}>{panel}</ComponentExtrasProvider> : panel}
    </DashboardEnvProvider>,
  );
}

const format = dashboardFormat("en", "UTC");

/** The text next to the label `name` in the panel's lists. */
function factOf(panel: HTMLElement, name: string): string | undefined {
  const label = within(panel)
    .queryAllByText(name)
    .find((element) => element.tagName === "DT");
  return label?.nextElementSibling?.textContent ?? undefined;
}

async function panelOf(): Promise<HTMLElement> {
  return screen.findByRole("region", { name: "Source" });
}

/** A click whose answer comes from the clipboard's promise: what it settles is done when this returns. */
async function clickAndSettle(element: HTMLElement) {
  await act(async () => {
    fireEvent.click(element);
  });
}

describe("what the panel says about a dataset", () => {
  test("when it was queried, the stretch of time it covers and how many rows it has", async () => {
    show({ kind: "rows", dataset: "power", rows: [] });
    const panel = await panelOf();
    expect(factOf(panel, "Queried at")).toBe(format.instant("2026-11-01T02:30:00Z"));
    expect(factOf(panel, "Data range")).toBe(
      `${format.wallTime(wallMs("2026-11-01 00:00:00") as number)} to ${format.wallTime(wallMs("2026-11-01 00:45:00") as number)}`,
    );
    expect(factOf(panel, "Rows")).toBe(String(POWER_ROWS.length));
  });

  test("the row count is the whole result's, also when rows were clicked and listed", async () => {
    show({ kind: "rows", dataset: "power", rows: [2, 3] });
    const panel = await panelOf();
    expect(factOf(panel, "Rows")).toBe(String(POWER_ROWS.length));
    expect(
      within(panel)
        .getAllByRole("region")
        .map((section) => section.getAttribute("aria-label")),
    ).toEqual(["Row 3", "Row 4"]);
  });

  test("a result of dates only has a range of dates", async () => {
    show({ kind: "rows", dataset: "trend", rows: [] });
    const panel = await panelOf();
    expect(factOf(panel, "Data range")).toBe(
      `${format.wallTime(wallMs("2026-10-26") as number, true)} to ${format.wallTime(wallMs("2026-10-30") as number, true)}`,
    );
  });

  test("a result with no time column has no range, and still has its row count", async () => {
    show({ kind: "rows", dataset: "daily", rows: [] });
    const panel = await panelOf();
    expect(factOf(panel, "Data range")).toBeUndefined();
    expect(factOf(panel, "Rows")).toBe("5");
  });

  test("a result with a time column and no rows has no range and says 0 rows", async () => {
    const data = fixtureData();
    const state = data.results.power;
    if (state?.status !== "ready") throw new Error("fixture");
    show(
      { kind: "rows", dataset: "power", rows: [] },
      {
        data: {
          ...data,
          results: { ...data.results, power: { status: "ready", result: { ...state.result, rows: [] } } },
        },
      },
    );
    const panel = await panelOf();
    expect(factOf(panel, "Data range")).toBeUndefined();
    expect(factOf(panel, "Rows")).toBe("0");
  });

  test("before the dataset has run there is no range and no count, only that it has not run", async () => {
    const data = fixtureData();
    show({ kind: "rows", dataset: "power", rows: [] }, { data: { ...data, results: {} } });
    const panel = await panelOf();
    expect(factOf(panel, "Queried at")).toBe("No result for this viewing yet");
    expect(factOf(panel, "Data range")).toBeUndefined();
    expect(factOf(panel, "Rows")).toBeUndefined();
  });

  test("a computation lists each dataset it reads with the same facts", async () => {
    show({ kind: "computation", name: "total_kwh" });
    const panel = await panelOf();
    expect(factOf(panel, "Rows")).toBe("5");
    expect(within(panel).getByText("SELECT day, site, kwh FROM daily")).toBeTruthy();
  });

  test("in Traditional Chinese the labels are the design's", async () => {
    show({ kind: "rows", dataset: "power", rows: [] }, { locale: "zh-TW" });
    const panel = await screen.findByRole("region", { name: "資料來源" });
    for (const label of ["查詢時間", "資料範圍", "筆數"]) {
      expect(factOf(panel, label)).toBeDefined();
    }
    expect(within(panel).getByRole("button", { name: "複製 SQL" })).toBeTruthy();
  });
});

describe("copying the query", () => {
  test("copies the SQL as it is and says so in a status region", async () => {
    const written = clipboard("works");
    show({ kind: "rows", dataset: "power", rows: [] });
    const panel = await panelOf();
    const status = within(panel).getByRole("status");
    expect(status.textContent).toBe("");
    await clickAndSettle(within(panel).getByRole("button", { name: "Copy the SQL" }));
    await until(() => status.textContent === "Copied the SQL.", "the panel to say the SQL was copied");
    expect(written).toEqual(["SELECT ts, line, kw FROM power ORDER BY ts"]);
  });

  test("says it could not copy when the browser refuses", async () => {
    const written = clipboard("refused");
    show({ kind: "rows", dataset: "power", rows: [] });
    const panel = await panelOf();
    await clickAndSettle(within(panel).getByRole("button", { name: "Copy the SQL" }));
    const status = within(panel).getByRole("status");
    await until(
      () => status.textContent === "Couldn't copy. Select the text and copy it yourself.",
      "the panel to say the copy failed",
    );
    expect(written).toHaveLength(1);
  });

  test("says it could not copy when the browser has no clipboard", async () => {
    clipboard("missing");
    show({ kind: "rows", dataset: "power", rows: [] });
    const panel = await panelOf();
    await clickAndSettle(within(panel).getByRole("button", { name: "Copy the SQL" }));
    expect(within(panel).getByRole("status").textContent).toBe("Couldn't copy. Select the text and copy it yourself.");
  });

  test("the words in Traditional Chinese are the design's", async () => {
    clipboard("works");
    show({ kind: "rows", dataset: "power", rows: [] }, { locale: "zh-TW" });
    const panel = await screen.findByRole("region", { name: "資料來源" });
    await clickAndSettle(within(panel).getByRole("button", { name: "複製 SQL" }));
    const status = within(panel).getByRole("status");
    await until(() => status.textContent === "已複製 SQL。", "the panel to say the SQL was copied");
    clipboard("refused");
    await clickAndSettle(within(panel).getByRole("button", { name: "複製 SQL" }));
    await until(() => status.textContent === "無法複製，請手動選取。", "the panel to say the copy failed");
  });

  test("the query's block can be focused and is named", async () => {
    show({ kind: "rows", dataset: "power", rows: [] });
    const panel = await panelOf();
    const block = within(panel).getByText("SELECT ts, line, kw FROM power ORDER BY ts");
    expect(block.tagName).toBe("PRE");
    expect(block.getAttribute("tabindex")).toBe("0");
    expect(within(panel).getByRole("group", { name: "The query's SQL" })).toBeTruthy();
  });
});

describe("a statistics or usage query", () => {
  const statistics = (overrides: Partial<SemanticRequest> = {}) =>
    requestDataset("telemetry_statistics", "stats", "Power statistics", request(overrides));

  test("is written as data points, period, grouping and time zone, with the raw parameters beside it", async () => {
    show({ kind: "rows", dataset: "stats", rows: [] }, { data: dataWith(statistics()) });
    const panel = await panelOf();
    const summary = within(panel).getByTestId("request-summary");
    expect(factOf(summary, "Data points")).toBe(`${DEVICE} · , active_power`);
    expect(factOf(summary, "Period")).toBe("The last 7 days");
    expect(factOf(summary, "Grouped by")).toBe("Every day");
    expect(factOf(summary, "Time zone")).toBe("Asia/Taipei");
    expect(within(panel).getByText("Telemetry statistics query")).toBeTruthy();
    // The parameters as written stay in the panel.
    const raw = within(panel).getByRole("group", { name: "The query's raw parameters" });
    expect(JSON.parse(raw.querySelector("pre")?.textContent ?? "")).toEqual(request());
  });

  test("a usage query says so", async () => {
    show(
      { kind: "rows", dataset: "usage", rows: [] },
      { data: dataWith(requestDataset("telemetry_usage", "usage", "Energy use", request({ period: "last_month" }))) },
    );
    const panel = await panelOf();
    expect(within(panel).getByText("Usage query")).toBeTruthy();
    expect(factOf(within(panel).getByTestId("request-summary"), "Period")).toBe("Last month");
  });

  test("writes a device by its name where the page can give one, and by its ID where it cannot", async () => {
    const names: Record<string, string> = { [DEVICE]: "Boiler B1" };
    show(
      { kind: "rows", dataset: "stats", rows: [] },
      {
        data: dataWith(
          statistics({
            series: [
              { device_id: DEVICE, point: "active_power" },
              { device_id: OTHER_DEVICE, point: "voltage" },
            ],
          }),
        ),
        extras: { deviceName: (id) => names[id] ?? id },
      },
    );
    const panel = await panelOf();
    const items = within(within(panel).getByTestId("request-summary")).getAllByRole("listitem");
    // The dot is for the eyes; a screen reader gets a comma instead.
    expect(items.map((item) => item.textContent)).toEqual([
      "Boiler B1 · , active_power",
      `${OTHER_DEVICE} · , voltage`,
    ]);
  });

  test("an explicit range is written as its two instants in the viewer's time zone", async () => {
    show(
      { kind: "rows", dataset: "stats", rows: [] },
      {
        data: dataWith(
          statistics({
            period: null,
            from: "2026-10-01T00:00:00+08:00",
            to: "2026-10-08T00:00:00+08:00",
            interval: "1mo",
          }),
        ),
      },
    );
    const panel = await panelOf();
    const summary = within(panel).getByTestId("request-summary");
    expect(factOf(summary, "Period")).toBe(
      `${format.instant("2026-10-01T00:00:00+08:00")} to ${format.instant("2026-10-08T00:00:00+08:00")}`,
    );
    expect(factOf(summary, "Grouped by")).toBe("Every month");
  });

  test("without an interval the result is one bucket for the whole period", async () => {
    const withoutInterval = requestDataset("telemetry_statistics", "stats", "Power statistics", {
      series: [{ device_id: DEVICE, point: "active_power" }],
      tz: "Asia/Taipei",
      period: "today",
    });
    show({ kind: "rows", dataset: "stats", rows: [] }, { data: dataWith(withoutInterval) });
    const panel = await panelOf();
    expect(factOf(within(panel).getByTestId("request-summary"), "Grouped by")).toBe("One value for the whole period");
  });

  test("copies the raw parameters and says so", async () => {
    const written = clipboard("works");
    show({ kind: "rows", dataset: "stats", rows: [] }, { data: dataWith(statistics()) });
    const panel = await panelOf();
    await clickAndSettle(within(panel).getByRole("button", { name: "Copy the parameters" }));
    const status = within(panel).getByRole("status");
    await until(
      () => status.textContent === "Copied the query parameters.",
      "the panel to say the parameters were copied",
    );
    expect(written).toEqual([JSON.stringify(request(), null, 2)]);
  });

  test("it still has the facts of any dataset", async () => {
    show({ kind: "rows", dataset: "stats", rows: [] }, { data: dataWith(statistics()) });
    const panel = await panelOf();
    expect(factOf(panel, "Rows")).toBe("2");
    expect(factOf(panel, "Data range")).toBeDefined();
  });
});

describe("the period in words", () => {
  const env = {
    t: (id: string, values?: Record<string, string | number>) => `${id}${values ? JSON.stringify(values) : ""}`,
    format,
  };
  const text = (period: string) => periodText(request({ period }), env as never);

  test("every relative period the contract has, and the two with a number", () => {
    expect(text("today")).toBe("dashboard.source.period_today");
    expect(text("last_year")).toBe("dashboard.source.period_last_year");
    expect(text("last_36_hours")).toBe('dashboard.source.period_last_hours{"count":36}');
    expect(text("last_366_days")).toBe('dashboard.source.period_last_days{"count":366}');
  });

  test("a period this build has no words for is shown as written, never guessed", () => {
    expect(text("last_0_days")).toBe("last_0_days");
    expect(text("fortnight")).toBe("fortnight");
    expect(text("constructor")).toBe("constructor");
  });
});

describe("the page's 「請 AI 解釋」", () => {
  const explain: ComponentExtras = { explainSource: (source) => <button type="button">Explain {source.title}</button> };

  test("is in the panel's foot for a dataset, a computation and a constant, named by what the panel shows", async () => {
    for (const [source, title] of [
      [{ kind: "rows", dataset: "power", rows: [] }, "Line power"],
      [{ kind: "computation", name: "total_kwh" }, "Total energy"],
      [{ kind: "constant", name: "limit" }, "Limit"],
    ] as const) {
      const view = show(source, { extras: explain });
      const panel = await panelOf();
      expect(within(panel).getByRole("button", { name: `Explain ${title}` })).toBeTruthy();
      view.unmount();
    }
  });

  test("is not there when the page gives none", async () => {
    show({ kind: "rows", dataset: "power", rows: [] });
    const panel = await panelOf();
    expect(within(panel).queryByRole("button", { name: /^Explain/ })).toBeNull();
  });
});
