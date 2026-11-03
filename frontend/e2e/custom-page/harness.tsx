// The outer page of the custom-page browser test (custom-page.e2e.ts): the real component
// (`CustomPageFrameView`), its guard and the real host adapter (`SandboxHost`) in a page that the
// test serves under the SPA's real headers, with the sandbox's files from the real `services/web`.
// `build.ts` bundles it; the test fetches the case it shows from `/__e2e/custom-page/case.json`.
//
// It records what the test checks in `window.__customPage`: every report the runtime sent on its
// port with whether the frame was visible right after the component handled it, every change of
// the frame's visibility, and navigations, each with `Date.now()` (the frame's pages record theirs
// on the same clock).
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { SandboxHost, type SandboxHostOptions } from "../../sandbox/src/host.ts";
import type { PortMessage } from "../../sandbox/src/protocol.ts";
import { DashboardEnvProvider } from "../../src/dashboard/context.tsx";
import {
  CustomPageFrameView,
  type CustomPageHost,
  CustomPageHostContext,
  frameShown,
} from "../../src/dashboard/custom-page/CustomPageFrame.tsx";
import type { DashboardData, DatasetResult } from "../../src/dashboard/data.ts";
import { fixtureData } from "../../src/dashboard/fixtures.ts";

/** What the test asks for. */
interface Case {
  page: string;
  /** Which data: the component tests' fixture, or that fixture with markup in a text column. */
  data?: "fixture" | "markup";
  /** Replaces the datasets' results with fresh copies after this many milliseconds (a refresh). */
  refreshAfterMs?: number;
}

interface Report {
  at: number;
  message: PortMessage;
  /** Whether the frame was visible when the component had handled the report. */
  visibleAfter: boolean;
}

interface Record {
  reports: Report[];
  visibility: { at: number; visible: boolean }[];
  navigations: number[];
  ready: boolean;
}

declare global {
  interface Window {
    __customPage: Record;
  }
}

/** Text that a page writing it as markup would turn into elements and a script. */
export const MARKUP = `<img src="/x" onerror="parent.postMessage('owned','*')"><script>window.owned=1</script>`;

function dataOf(which: Case["data"]): DashboardData {
  const data = fixtureData();
  if (which !== "markup") return data;
  const power = data.results.power;
  if (power?.status !== "ready") throw new Error("the fixture has no power result");
  const result: DatasetResult = {
    ...power.result,
    rows: power.result.rows.map((row, index) => (index === 0 ? [row[0] ?? null, MARKUP, row[2] ?? null] : row)),
  };
  return { ...data, results: { ...data.results, power: { status: "ready", result } } };
}

function frameVisible(frame: HTMLIFrameElement): boolean {
  return frameShown(frame);
}

const record: Record = { reports: [], visibility: [], navigations: [], ready: false };
window.__customPage = record;

/** The real host adapter, with every report and navigation recorded. */
function recordingHost(options: SandboxHostOptions): CustomPageHost {
  let host: SandboxHost | null = null;
  host = new SandboxHost({
    ...options,
    onReport: (message) => {
      options.onReport?.(message);
      const frame = host?.frame;
      record.reports.push({ at: Date.now(), message, visibleAfter: frame !== undefined && frameVisible(frame) });
    },
    onNavigate: () => {
      record.navigations.push(Date.now());
      options.onNavigate?.();
    },
  });
  const frame = host.frame;
  let last = frameVisible(frame);
  record.visibility.push({ at: Date.now(), visible: last });
  const observer = new MutationObserver(() => {
    const now = frameVisible(frame);
    if (now !== last) {
      last = now;
      record.visibility.push({ at: Date.now(), visible: now });
    }
  });
  observer.observe(frame, { attributes: true, attributeFilter: ["style", "inert"] });
  // A removed frame is not visible: the observer of its parent notices the removal.
  const parent = frame.parentNode;
  if (parent !== null) {
    new MutationObserver(() => {
      if (!frame.isConnected && last) {
        last = false;
        record.visibility.push({ at: Date.now(), visible: false });
      }
    }).observe(parent, { childList: true });
  }
  return host;
}

function App(props: { test: Case; data: DashboardData }) {
  return (
    <CustomPageHostContext.Provider value={recordingHost}>
      <DashboardEnvProvider locale="en" timeZone="UTC">
        <CustomPageFrameView
          page={props.test.page}
          runtime={1}
          data={props.data}
          title="E2E custom page"
          versionCreatedAt="2026-11-01T06:20:00Z"
          theme="light"
        />
      </DashboardEnvProvider>
    </CustomPageHostContext.Provider>
  );
}

async function main(): Promise<void> {
  const response = await fetch("/__e2e/custom-page/case.json");
  const test = (await response.json()) as Case;
  const element = document.getElementById("root");
  if (element === null) throw new Error("no #root");
  const root = createRoot(element);
  let data = dataOf(test.data);
  const show = () =>
    root.render(
      <StrictMode>
        <App test={test} data={data} />
      </StrictMode>,
    );
  show();
  if (test.refreshAfterMs !== undefined) {
    setTimeout(() => {
      // New result objects: the component sends them as a new generation.
      data = { ...data, results: structuredClone(data.results) };
      show();
    }, test.refreshAfterMs);
  }
  record.ready = true;
}

void main();
