import "./preview.css";

import { createRoot } from "react-dom/client";
import { createBrowserRouter } from "react-router";
import { createApiClient } from "../src/api/client.ts";
import { dashboardApi } from "../src/api/dashboards.ts";
import { App } from "../src/app/App.tsx";
import { createQueryClient } from "../src/app/query-client.ts";
import { ROUTES } from "../src/app/routes.tsx";
import { SPECS } from "../src/dashboard/fixtures.ts";
import { applyToDocument, resolveLocale, resolveTheme } from "../src/preferences/preferences.ts";
import { FakeDashboardServer } from "../src/test/fake-dashboards.ts";
import { FakeDeviceServer } from "../src/test/fake-devices.ts";
import { fakeWithUser, memoryPreferences } from "../src/test/render.tsx";

interface Config {
  role?: "viewer" | "editor" | "admin";
  theme?: "light" | "dark";
  language?: "zh-TW" | "en";
  tenants?: string[];
  empty?: boolean;
  /** Talk to a real dashboard API through the proxy (the end-to-end run). */
  real?: boolean;
}
declare global {
  interface Window {
    __PREVIEW?: Config;
    __fake?: { server: FakeDashboardServer; ids: Record<string, string> };
  }
}

const config = window.__PREVIEW ?? {};
const server = new FakeDashboardServer();
server.role = config.role ?? "editor";
const ids: Record<string, string> = {};
if (!config.empty && !config.real) {
  const add = (key: string, title: string, versions: unknown[][], updatedAt: string, archivedAt?: string) => {
    const dashboard = server.addDashboard({ title, versions, updatedAt, ...(archivedAt ? { archivedAt } : {}) });
    ids[key] = dashboard.id;
    for (const version of dashboard.versions) version.title = title;
    return dashboard;
  };
  const full = [SPECS.kpi, SPECS.status, SPECS.timeSeries, SPECS.bar, SPECS.table];
  const cooling = add(
    "cooling",
    "冷卻水塔監控",
    [full.slice(0, 3), full.slice(0, 4), full, full],
    "2026-10-30T08:05:00Z",
  );
  cooling.versions[1] && (cooling.versions[1].origin = "publish");
  cooling.versions[2] && (cooling.versions[2].origin = "restore");
  cooling.versions[2] && (cooling.versions[2].restored_from = 1);
  cooling.versions[3] && (cooling.versions[3].origin = "publish");
  add("weekly", "冷卻水塔週報", [[SPECS.kpi, SPECS.bar]], "2026-11-01T07:12:00Z");
  add(
    "air",
    "空壓機運轉時數",
    [[SPECS.table], [SPECS.table, SPECS.kpi], [SPECS.kpi, SPECS.table]],
    "2026-10-27T01:40:00Z",
  );
  add("storage", "倉儲區溫濕度", [[SPECS.timeSeries]], "2026-10-21T03:18:00Z");
  add("summer", "夏季用電試算", [[SPECS.bar]], "2026-09-02T02:30:00Z", "2026-10-01T00:00:00Z");
  add("old", "冷卻水塔（舊版）", [[SPECS.table]], "2026-08-19T06:47:00Z", "2026-09-20T00:00:00Z");
  add("custom", "含自訂圖表的儀表板", [[SPECS.kpi, { kind: "custom_page" }]], "2026-10-25T00:00:00Z");
}
window.__fake = { server, ids };

const {
  server: identity,
  user,
  tenants,
} = fakeWithUser({
  tenants: config.tenants ?? ["東區示範廠", "南區示範廠"],
  role: config.role ?? "editor",
  preferences: { language: config.language ?? "zh-TW", timeZone: "Asia/Taipei", theme: config.theme ?? "light" },
});
const browser = identity.browser(identity.createSession(user.email, tenants[0] ?? null));
const language = config.language ?? "zh-TW";
const theme = config.theme ?? "light";
applyToDocument(
  document.documentElement,
  resolveLocale(language, navigator.languages),
  resolveTheme(theme, window.matchMedia("(prefers-color-scheme: dark)").matches),
);

const router = createBrowserRouter(ROUTES);
const queryClient = createQueryClient({ onUnauthenticated: () => {} });
const root = document.getElementById("root");
if (root === null) throw new Error("no root");
createRoot(root).render(
  <App
    router={router}
    deps={{
      queryClient,
      preferences: memoryPreferences({ language, theme, timeZone: "Asia/Taipei" }),
      identity: browser,
      devices: new FakeDeviceServer().api(),
      dashboards: config.real ? dashboardApi(createApiClient()) : server.api(),
    }}
  />,
);
