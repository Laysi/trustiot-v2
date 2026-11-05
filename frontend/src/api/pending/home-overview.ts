// Pending Laysi/trustiot-v2#453: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, createElement, type ReactNode, useContext } from "react";

/**
 * The data of the generic home (docs/design/home-ai/design.md, 「通用首頁」): the device status
 * counts, the devices that used the most power today, the anomalies since the user's last visit
 * and the device table. Laysi/trustiot-v2#453 lists "今天的用電、設備正常／異常／離線的數量、用電
 * 最多的設備" as the home cards' data API, and Laysi/trustiot-v2#118 the current device status;
 * neither exists yet, so the shapes below are the design's, read as one request per block (the
 * design's 12-E: every block queries and fails on its own).
 *
 * Assumptions, until those issues settle the real ones:
 * - A device is `normal`, `abnormal` or `offline`, as the design's three tiles; Laysi/trustiot-v2#118
 *   also has an "unknown" state for the devices of an offline gateway, which the design does not
 *   draw, so it is not modelled here.
 * - Power is in kWh since local midnight of the user's time zone (`tz`).
 * - "Since the last visit" is the server's: `since` in the answer is the instant it used.
 */
export type DeviceStatus = "normal" | "abnormal" | "offline";

export interface DeviceRef {
  device_id: string;
  name: string;
}

export interface StatusSummary {
  /** When the figures were last updated. */
  updated_at: string;
  gateway_count: number;
  /** The devices in each state; the tiles count them and name the one of a single device. */
  normal: DeviceRef[];
  abnormal: DeviceRef[];
  offline: DeviceRef[];
}

export interface UsageRow extends DeviceRef {
  kwh: number;
}

export interface TopUsage {
  /** The start of the period the figures cover (midnight today). */
  since: string;
  /** Descending by `kwh`, at most five. */
  devices: UsageRow[];
}

export interface AnomalyRow extends DeviceRef {
  kind: "abnormal" | "offline";
  /** What happened, a short statement the service words ("排氣溫度高於門檻"). */
  summary: string;
  started_at: string;
  /** `null` while it goes on. */
  ended_at: string | null;
}

export interface NewAnomalies {
  /** The user's last visit, or the start of the period when there was none. */
  since: string;
  anomalies: AnomalyRow[];
}

export interface DeviceHealthRow extends DeviceRef {
  model_name: string;
  /** The gateway's hardware ID, as the device list shows it. */
  gateway: string;
  status: DeviceStatus;
  usage_today_kwh: number | null;
  /** The newest reading's time; `null` when the device never reported. */
  last_report_at: string | null;
}

export interface HomeOverviewApi {
  getStatusSummary(tz: string, signal?: AbortSignal): Promise<StatusSummary>;
  getTopUsage(tz: string, signal?: AbortSignal): Promise<TopUsage>;
  getNewAnomalies(signal?: AbortSignal): Promise<NewAnomalies>;
  listDeviceHealth(tz: string, signal?: AbortSignal): Promise<DeviceHealthRow[]>;
}

const HomeOverviewApiContext = createContext<HomeOverviewApi | null>(null);

/** Gives the generic home its data: a fake server in the UI preview and in tests. */
export function HomeOverviewApiProvider({ api, children }: { api: HomeOverviewApi; children: ReactNode }) {
  return createElement(HomeOverviewApiContext.Provider, { value: api }, children);
}

/** The overview API; the generic home exists only where a provider gives it. */
export function useHomeOverviewApi(): HomeOverviewApi {
  const value = useContext(HomeOverviewApiContext);
  if (value === null) {
    throw new Error("useHomeOverviewApi needs a HomeOverviewApiProvider");
  }
  return value;
}
