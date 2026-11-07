// The custom page's outer component (CustomPageFrame.tsx) with a stand-in host adapter: what it
// sends, when the frame is visible, what it shows instead, and the source panel. The real frame and
// runtime are in the browser test (frontend/e2e/custom-page.e2e.ts).
import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";

import type { DeliveredDataset, SandboxHostOptions } from "../../../sandbox/src/host.ts";
import type { PortMessage } from "../../../sandbox/src/protocol.ts";
import { DashboardEnvProvider } from "../context.tsx";
import type { DashboardData, DatasetState } from "../data.ts";
import { fixtureData } from "../fixtures.ts";
import { dashboardFormat } from "../format.ts";
import {
  type CustomPageFrameProps,
  CustomPageFrameView,
  type CustomPageHost,
  CustomPageHostContext,
  frameShown,
} from "./CustomPageFrame.tsx";

/** When the version in these tests was written. */
const VERSION_CREATED_AT = "2026-11-01T06:20:00Z";

/** `data` with one dataset's state replaced. */
function withResult(data: DashboardData, dataset: string, state: DatasetState): DashboardData {
  return { ...data, results: { ...data.results, [dataset]: state } };
}

afterEach(() => {
  cleanup();
});

interface FakeHost extends CustomPageHost {
  options: SandboxHostOptions;
  inits: unknown[];
  deliveries: DeliveredDataset[][];
  destroyed: boolean;
}

function setUp(props: Partial<CustomPageFrameProps> = {}, dataFails: () => boolean = () => false) {
  const hosts: FakeHost[] = [];
  const create = (options: SandboxHostOptions): CustomPageHost => {
    // A stand-in element: happy-dom would try to load a real iframe.
    const frame = document.createElement("div") as unknown as HTMLIFrameElement;
    options.container.appendChild(frame);
    const host: FakeHost = {
      frame,
      options,
      inits: [],
      deliveries: [],
      destroyed: false,
      init(message) {
        host.inits.push(message);
      },
      data(datasets) {
        if (dataFails()) throw new Error("refused");
        host.deliveries.push([...datasets]);
        return host.deliveries.length;
      },
      destroy() {
        host.destroyed = true;
        frame.remove();
      },
    };
    hosts.push(host);
    return host;
  };
  const all: CustomPageFrameProps = {
    page: "<p>page</p>",
    runtime: 1,
    data: fixtureData(),
    title: "Energy",
    versionCreatedAt: VERSION_CREATED_AT,
    ...props,
  };
  const element = (next: CustomPageFrameProps) => (
    <CustomPageHostContext.Provider value={create}>
      <DashboardEnvProvider locale="en" timeZone="UTC">
        <CustomPageFrameView {...next} theme="light" />
      </DashboardEnvProvider>
    </CustomPageHostContext.Provider>
  );
  const view = render(element(all));
  const host = () => {
    const last = hosts.at(-1);
    if (last === undefined) throw new Error("no host was created");
    return last;
  };
  const report = (message: PortMessage) => act(() => host().options.onReport?.(message));
  const ready = () => act(() => host().options.onMessage({ tq: 1, type: "ready", runtime: 1 }));
  const rerender = (next: Partial<CustomPageFrameProps>) => view.rerender(element({ ...all, ...next }));
  return { hosts, host, report, ready, rerender };
}

function frameVisible(host: FakeHost): boolean {
  return (
    frameShown(host.frame) &&
    host.frame.style.pointerEvents === "auto" &&
    host.frame.getAttribute("aria-hidden") === "false"
  );
}

/** A frame that got init and data, and a clean comparison of it. */
async function shown(props: Partial<CustomPageFrameProps> = {}, dataFails?: () => boolean) {
  const setup = setUp(props, dataFails);
  await setup.ready();
  await setup.report({ tq: 1, type: "compared", generation: 1, findings: 0 });
  expect(frameVisible(setup.host())).toBe(true);
  return setup;
}

describe("before a clean comparison", () => {
  test("the frame is hidden from the start and out of the accessibility tree and tab order", () => {
    const { host } = setUp();
    // Transparent and inert, never `visibility: hidden` (CustomPageFrame.tsx, applyView).
    expect(host().frame.style.opacity).toBe("0");
    expect(host().frame.style.visibility).toBe("");
    expect(host().frame.style.pointerEvents).toBe("none");
    expect(host().frame.inert).toBe(true);
    expect(host().frame.getAttribute("aria-hidden")).toBe("true");
    expect(host().frame.tabIndex).toBe(-1);
    expect(screen.getByText("Loading the custom page…")).toBeTruthy();
  });

  test("ready gets init in display mode with the public spec only, then the data", async () => {
    const { host, ready } = setUp();
    await ready();
    expect(host().inits).toHaveLength(1);
    const init = host().inits[0] as Record<string, unknown>;
    expect(init).toMatchObject({ page: "<p>page</p>", locale: "en", tz: "UTC", theme: "light", mode: "view" });
    expect(JSON.stringify(init)).not.toContain("SELECT");
    expect(host().deliveries).toHaveLength(1);
    expect(host().options).toMatchObject({ base: "/sandbox", runtime: 1, title: "Energy" });
  });

  test("a second ready is not answered", async () => {
    const { host, ready } = setUp();
    await ready();
    await ready();
    expect(host().inits).toHaveLength(1);
  });

  test("no data goes out while a dataset is loading", async () => {
    const data = withResult(fixtureData(), "daily", { status: "loading" });
    const { host, ready, rerender } = setUp({ data });
    await ready();
    expect(host().deliveries).toHaveLength(0);
    await act(() => rerender({ data: fixtureData() }));
    expect(host().deliveries).toHaveLength(1);
  });

  test("findings keep it hidden and say how many", async () => {
    const { host, ready, report } = setUp();
    await ready();
    await report({ tq: 1, type: "compared", generation: 1, findings: 2 });
    expect(frameVisible(host())).toBe(false);
    expect(screen.getByText("2 values on this page don't match the data, so it isn't shown.")).toBeTruthy();
  });

  test("data the protocol refuses is not sent, and the page is not shown", async () => {
    const { host, ready, report } = setUp({}, () => true);
    await ready();
    await report({ tq: 1, type: "compared", generation: 1, findings: 0 });
    expect(frameVisible(host())).toBe(false);
    expect(screen.getByText("The data for this page couldn't be passed to it, so it isn't shown.")).toBeTruthy();
  });

  test("a runtime this release does not have creates no frame", () => {
    const { hosts } = setUp({ runtime: 7 });
    expect(hosts).toHaveLength(0);
    expect(screen.getByText(/needs version 7 of the page runtime/)).toBeTruthy();
  });
});

describe("shown", () => {
  test("a clean comparison shows it with the reported height and no notice", async () => {
    const { host, report } = await shown();
    await report({ tq: 1, type: "resize", height: 640 });
    expect(host().frame.style.height).toBe("640px");
    expect(screen.queryByTestId("custom-page-notice")).toBeNull();
    expect(host().frame.tabIndex).toBe(0);
  });

  test("findings hide it in the report's own call, before React renders", async () => {
    const { host } = await shown();
    // Not wrapped in act: the frame must be hidden when onReport returns.
    host().options.onReport?.({ tq: 1, type: "compared", generation: 1, findings: 1 });
    expect(frameVisible(host())).toBe(false);
    await act(async () => {});
  });

  test("new data hides it until that data is compared", async () => {
    const { host, report, rerender } = await shown();
    const data = fixtureData();
    await act(() => rerender({ data }));
    expect(host().deliveries).toHaveLength(2);
    expect(frameVisible(host())).toBe(false);
    expect(screen.getByText("Checking the values on the page against the new data…")).toBeTruthy();
    await report({ tq: 1, type: "compared", generation: 1, findings: 0 });
    expect(frameVisible(host())).toBe(false);
    await report({ tq: 1, type: "compared", generation: 2, findings: 0 });
    expect(frameVisible(host())).toBe(true);
  });

  test("refused new data hides a page that was shown, and older comparisons do not show it again", async () => {
    let fail = false;
    const { host, report, rerender } = await shown({}, () => fail);
    fail = true;
    await act(() => rerender({ data: fixtureData() }));
    expect(frameVisible(host())).toBe(false);
    await report({ tq: 1, type: "compared", generation: 1, findings: 0 });
    expect(frameVisible(host())).toBe(false);
    expect(screen.getByText("The data for this page couldn't be passed to it, so it isn't shown.")).toBeTruthy();
  });

  test("a new data object with the same results sends nothing and keeps it shown", async () => {
    const data = fixtureData();
    const { host, rerender } = await shown({ data });
    await act(() => rerender({ data: { ...data, results: { ...data.results } } }));
    expect(host().deliveries).toHaveLength(1);
    expect(frameVisible(host())).toBe(true);
  });
});

describe("closed", () => {
  test("a navigation removes the frame and offers a new one", async () => {
    const { host, hosts } = await shown();
    await act(() => host().options.onNavigate?.());
    expect(host().destroyed).toBe(true);
    expect(host().frame.isConnected).toBe(false);
    expect(screen.getByText("The page tried to leave its frame, so it was closed.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load the page again" }));
    expect(hosts).toHaveLength(2);
    expect(frameVisible(host())).toBe(false);
  });

  test("a failed report removes the frame", async () => {
    const { host, report } = await shown();
    await report({ tq: 1, type: "failed", reason: "corrections" });
    expect(host().frame.isConnected).toBe(false);
    expect(
      screen.getByText("The page kept putting back values that don't match the data, so it was closed."),
    ).toBeTruthy();
  });
  test("a page that changed what it shows unnoticed is removed and says so", async () => {
    const { host, report } = await shown();
    await report({ tq: 1, type: "failed", reason: "unnotified" });
    expect(host().frame.isConnected).toBe(false);
    expect(
      screen.getByText("The page changed what it shows in a way the platform can't check, so it was closed."),
    ).toBeTruthy();
  });
});

describe("a page error", () => {
  test("hides the frame with a fixed message, never the page's own text", async () => {
    const { host, report } = await shown();
    await report({ tq: 1, type: "error", phase: "render", message: "TypeError: the total is 999.99 kW" });
    expect(frameVisible(host())).toBe(false);
    expect(screen.getByText("The custom page stopped with an error, so it isn't shown.")).toBeTruthy();
    expect(screen.queryByText(/999\.99/)).toBeNull();
  });
});

describe("the card", () => {
  test("is labelled a custom chart drawn by code the AI wrote, checked before the version was written", () => {
    const { host } = setUp();
    expect(screen.getByRole("heading", { level: 3, name: "Energy" })).toBeTruthy();
    expect(screen.getByTestId("custom-page-label").textContent).toBe("Custom chart");
    const time = dashboardFormat("en", "UTC").instant(VERSION_CREATED_AT);
    expect(screen.getByTestId("custom-page-label-description").textContent).toBe(
      `Drawn by code the AI wrote, not a platform component・This version passed the check before it was saved on ${time}`,
    );
    // The label is the outer page's: it is there before the frame shows anything.
    expect(frameShown(host().frame)).toBe(false);
  });

  test("its source button opens the only dataset's source, whether the frame is shown or not", () => {
    const data = fixtureData();
    setUp({ data: { ...data, datasets: data.datasets.slice(0, 1) } });
    fireEvent.click(screen.getByRole("button", { name: "Data sources of “Energy”" }));
    expect(screen.getByRole("region", { name: "Source" })).toBeTruthy();
    expect(screen.getByText("SELECT ts, line, kw FROM power ORDER BY ts")).toBeTruthy();
  });

  test("with several datasets its source button is a menu of them", async () => {
    setUp();
    await act(async () => {
      fireEvent.keyDown(screen.getByRole("button", { name: "Data sources of “Energy”" }), { key: "Enter" });
    });
    const items = screen.getAllByRole("menuitem").map((item) => item.textContent);
    expect(items).toEqual(["Line power", "Daily energy", "Meter readings", "Daily total"]);
    await act(async () => {
      fireEvent.click(screen.getByRole("menuitem", { name: "Daily energy" }));
    });
    expect(screen.getByText("SELECT day, site, kwh FROM daily")).toBeTruthy();
  });
});

describe("source panel", () => {
  test("a clicked cell opens its dataset's SQL and query time from the outer page's own data", async () => {
    const { report } = await shown();
    await report({ tq: 1, type: "source", reference: "c|power|kw|1" });
    const data = fixtureData();
    const power = data.datasets[0];
    const state = data.results.power;
    if (power?.source.kind !== "sql" || state?.status !== "ready") throw new Error("fixture changed");
    expect(screen.getByText(power.source.sql)).toBeTruthy();
    expect(screen.getByText("Line power")).toBeTruthy();
    // The query time the API returned, in the user's format.
    expect(screen.getByText(dashboardFormat("en", "UTC").instant(state.result.meta.queried_at))).toBeTruthy();
  });

  test("a reference that names nothing in this version opens nothing", async () => {
    const { report } = await shown();
    await report({ tq: 1, type: "source", reference: "c|nope|kw|0" });
    expect(screen.queryByText("SELECT", { exact: false })).toBeNull();
  });

  describe("closing (Laysi/trustiot-v2#1201)", () => {
    /** One dataset, so the card's source button opens its source at once. */
    function single() {
      const data = fixtureData();
      return setUp({ data: { ...data, datasets: data.datasets.slice(0, 1) } });
    }
    const SOURCE_BUTTON = { name: "Data sources of “Energy”" };

    test("Escape closes the panel and gives focus back to the card's source button", async () => {
      single();
      const button = screen.getByRole("button", SOURCE_BUTTON);
      act(() => button.focus());
      fireEvent.click(button);
      const panel = screen.getByRole("region", { name: "Source" });
      fireEvent.keyDown(within(panel).getByRole("heading", { name: "Source" }), { key: "Escape" });
      expect(screen.queryByRole("region", { name: "Source" })).toBeNull();
      expect(document.activeElement).toBe(button);
    });

    test("pressing the source button again closes the panel, and it says whether it is open", () => {
      single();
      const button = screen.getByRole("button", SOURCE_BUTTON);
      expect(button.getAttribute("aria-expanded")).toBe("false");
      act(() => button.focus());
      fireEvent.click(button);
      expect(button.getAttribute("aria-expanded")).toBe("true");
      fireEvent.click(button);
      expect(screen.queryByRole("region", { name: "Source" })).toBeNull();
      expect(button.getAttribute("aria-expanded")).toBe("false");
      expect(document.activeElement).toBe(button);
    });

    test("the same marked value reported by the page again closes the panel; another one switches it", async () => {
      const { report } = await shown();
      await report({ tq: 1, type: "source", reference: "c|power|kw|1" });
      const first = screen.getByRole("region", { name: "Source" }).textContent;
      await report({ tq: 1, type: "source", reference: "c|power|kw|2" });
      expect(screen.getByRole("region", { name: "Source" }).textContent).not.toBe(first);
      await report({ tq: 1, type: "source", reference: "c|power|kw|2" });
      expect(screen.queryByRole("region", { name: "Source" })).toBeNull();
    });

    test("a panel the page opened gives focus back to the frame when it is closed", async () => {
      const { host, report } = await shown();
      const frame = host().frame;
      frame.tabIndex = 0;
      act(() => frame.focus());
      await report({ tq: 1, type: "source", reference: "c|power|kw|1" });
      const panel = screen.getByRole("region", { name: "Source" });
      expect(document.activeElement).not.toBe(frame);
      fireEvent.click(within(panel).getByRole("button", { name: "Close the source" }));
      expect(document.activeElement).toBe(frame);
    });
  });

  test("nothing opens while the frame is hidden", async () => {
    const { report } = await shown();
    await report({ tq: 1, type: "compared", generation: 1, findings: 1 });
    await report({ tq: 1, type: "source", reference: "c|power|kw|1" });
    expect(screen.queryByText("SELECT ts, line, kw FROM power ORDER BY ts")).toBeNull();
  });
});

describe("failed datasets", () => {
  test("are listed with the outer page's message and a way to run them again", async () => {
    const data = withResult(fixtureData(), "daily", { status: "failed", message: "The query took too long." });
    let refreshed = 0;
    setUp({ data, onRefresh: () => refreshed++ });
    expect(screen.getByText("Daily energy: The query took too long.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load the data again" }));
    expect(refreshed).toBe(1);
  });
});
