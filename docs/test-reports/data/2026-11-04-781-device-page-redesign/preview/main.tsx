import "./preview.css";

import { createRoot } from "react-dom/client";
import { createBrowserRouter } from "react-router";
import { App } from "../src/app/App.tsx";
import { createQueryClient } from "../src/app/query-client.ts";
import { ROUTES } from "../src/app/routes.tsx";
import { applyToDocument, resolveLocale, resolveTheme } from "../src/preferences/preferences.ts";
import { FakeConversationServer } from "../src/test/fake-conversations.ts";
import { FakeDashboardServer } from "../src/test/fake-dashboards.ts";
import { FakeDeviceServer } from "../src/test/fake-devices.ts";
import { fakeWithUser, memoryPreferences } from "../src/test/render.tsx";

interface Config {
  theme?: "light" | "dark";
  language?: "zh-TW" | "en";
}
declare global {
  interface Window {
    __PREVIEW?: Config;
    __fake?: { server: FakeDeviceServer; ids: Record<string, string>; holds: Set<string> };
  }
}

const config = window.__PREVIEW ?? {};
const MIN = 60_000;
const HOUR = 3_600_000;
const DAY = 24 * HOUR;
const now = Date.now();

function rng(seed: number) {
  let s = seed;
  return () => {
    s = (s * 1664525 + 1013904223) % 4294967296;
    return s / 4294967296;
  };
}

const server = new FakeDeviceServer();
server.aggregatedPeriodMs = 90 * DAY;
const holds = new Set<string>();
const ids: Record<string, string> = {};

// Hold an endpoint's answers until the script releases it (the loading states).
const original = server.fetch.bind(server);
server.fetch = async (request) => {
  const path = new URL(request.url).pathname;
  const endpoint = path.endsWith("/series")
    ? "series"
    : path.endsWith("/latest")
      ? "latest"
      : path.includes("/device-models/")
        ? "model"
        : path.includes("/devices/")
          ? "device"
          : "other";
  while (holds.has(endpoint)) await new Promise((resolve) => setTimeout(resolve, 50));
  return original(request);
};

const POINTS = [
  { key: "voltage_a", data_type: "f32" as const, description: "A 相電壓" },
  { key: "current_a", data_type: "f32" as const, description: "A 相電流" },
  { key: "power_kw", data_type: "f32" as const, description: "有效功率" },
  { key: "energy_kwh", data_type: "u32" as const, description: "累積用電" },
  { key: "breaker", data_type: "bool" as const, description: "斷路器" },
  { key: "status", data_type: "string" as const },
];

function fill(
  deviceId: string,
  options: {
    endAgo: number;
    hours: number;
    gap?: [number, number];
    keys?: readonly string[];
    texts?: Record<string, string>;
  },
) {
  const random = rng(7);
  const keys = options.keys ?? POINTS.map((point) => point.key);
  const end = now - options.endAgo;
  const minutes = Math.floor((options.hours * HOUR) / MIN);
  for (let k = minutes; k >= 0; k -= 1) {
    const time = end - k * MIN;
    const ago = now - time;
    if (options.gap && ago >= options.gap[0] && ago < options.gap[1]) continue;
    const x = 1 - (ago - options.endAgo) / (options.hours * HOUR);
    const hour = ((time + 8 * HOUR) % DAY) / HOUR;
    for (const key of keys) {
      if (key === "voltage_a") {
        for (const offset of [0, 20, 40]) {
          const base = 229.9 + 3.3 * Math.sin(x * Math.PI * 2 + 0.6) + 0.5 * Math.sin(x * 37) + (random() - 0.5) * 0.6;
          const spike = x > 0.58 && x < 0.6 ? (random() - 0.3) * 4 : 0;
          server.addReadings(deviceId, key, [[time - offset * 1000, { type: "float", value: base + spike }]]);
        }
      } else if (key === "breaker") {
        server.addReadings(deviceId, key, [[time, { type: "bool", value: hour > 7 && hour < 21 }]]);
      } else if (key === "status") {
        const value = k % 211 < 3 ? "over-current-trip" : k % 503 < 5 ? "maintenance" : "normal";
        server.addReadings(deviceId, key, [[time, { type: "string", value: options.texts?.[key] ?? value }]]);
      } else if (key === "energy_kwh") {
        server.addReadings(deviceId, key, [[time, { type: "int", value: 99_000 + (minutes - k) }]]);
      } else {
        const level = key === "power_kw" ? 27.8 : key === "current_a" ? 41.3 : 1;
        server.addReadings(deviceId, key, [
          [time, { type: "float", value: level + 4 * Math.sin(x * 20) + (random() - 0.5) }],
        ]);
      }
    }
  }
}

const normal = server.addDevice({
  name: "line-1-meter",
  modelName: "power-meter",
  hardwareId: "tg-0042",
  pollIntervalMs: 10_000,
  points: POINTS,
});
ids.normal = normal.device_id;
fill(normal.device_id, { endAgo: 12_000, hours: 24, gap: [8.2 * HOUR, 9 * HOUR] });

const stale = server.addDevice({
  name: "line-2-meter",
  modelId: normal.model_id,
  hardwareId: "tg-0042",
  pollIntervalMs: 10_000,
});
ids.stale = stale.device_id;
fill(stale.device_id, { endAgo: 3 * HOUR + 14 * MIN, hours: 24 });

ids.never = server.addDevice({
  name: "line-3-meter",
  modelId: normal.model_id,
  hardwareId: "tg-0042",
  pollIntervalMs: 10_000,
}).device_id;

const truncated = server.addDevice({
  name: "line-4-meter",
  modelId: normal.model_id,
  hardwareId: "tg-0042",
  pollIntervalMs: 10_000,
});
ids.truncated = truncated.device_id;
fill(truncated.device_id, { endAgo: 12_000, hours: 15 });

ids.nopoints = server.addDevice({
  name: "bare-meter",
  modelName: "empty-model",
  points: [],
  hardwareId: "tg-0042",
}).device_id;

const LONG_POINTS = [
  ...POINTS,
  { key: "voltage_b", data_type: "f32" as const, description: "B 相電壓" },
  { key: "voltage_c", data_type: "f32" as const, description: "C 相電壓" },
  { key: "current_b", data_type: "f32" as const, description: "B 相電流" },
  { key: "current_c", data_type: "f32" as const, description: "C 相電流" },
  { key: "power_factor", data_type: "f32" as const, description: "功率因數" },
  { key: "frequency_hz", data_type: "f32" as const, description: "頻率" },
  {
    key: "thd_voltage_percent_phase_a_long_key_name",
    data_type: "f32" as const,
    description: "A 相電壓總諧波失真（長說明文字示範：超過一行時截斷並在提示顯示全文）",
  },
  { key: "firmware", data_type: "string" as const, description: "韌體版本" },
];
const long = server.addDevice({
  name: "assembly-line-3-north-wing-main-incomer-power-quality-analyser-02",
  modelName: "pq-analyser-advanced-firmware-v4",
  hardwareId: "tg-0042",
  pollIntervalMs: 10_000,
  points: LONG_POINTS,
});
ids.long = long.device_id;
fill(long.device_id, {
  endAgo: 12_000,
  hours: 6,
  keys: LONG_POINTS.map((point) => point.key).filter((key) => key !== "firmware"),
  texts: { status: "over-current-trip-latched-waiting-manual-reset" },
});
server.addReadings(long.device_id, "firmware", [
  [now - 26 * HOUR, { type: "string", value: "PQ-4.18.2-build.20261014-release-candidate" }],
]);
window.__fake = { server, ids, holds };

const language = config.language ?? "zh-TW";
const theme = config.theme ?? "light";
const {
  server: identity,
  user,
  tenants,
} = fakeWithUser({
  tenants: ["示範組織"],
  role: "viewer",
  preferences: { language, timeZone: "Asia/Taipei", theme },
});
const browser = identity.browser(identity.createSession(user.email, tenants[0] ?? null));
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
      devices: server.api(),
      dashboards: new FakeDashboardServer().api(),
      conversations: new FakeConversationServer().api(),
    }}
  />,
);
