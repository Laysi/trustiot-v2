// Pending Laysi/trustiot-v2#129: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, createElement, type ReactNode, useContext } from "react";

/**
 * The new tenant's setup checklist (docs/design/home-ai/design.md, 「設定清單」). No endpoint
 * serves it yet; this is the interface the checklist is built against, from the issue's 範圍 and
 * the 2026-11-01 decisions:
 *
 * - The steps and their order are fixed. A step is completed by the service that owns it, never
 *   by the page (the assets service for the first two, the ingest service for the first reading,
 *   the identity service for the invitation); the page can only skip a step or take the skip back.
 * - Skipping and completing are the tenant's, not the user's (decision 2); only a tenant admin
 *   reads or writes the checklist (decision 1).
 * - Choosing the first use completes that step; the choices are the features that are live
 *   (the issue's 範圍), each with the plan it needs.
 * - Skipping and taking a skip back are events too (the design's 實作上的提醒), so the funnel can
 *   tell "stuck" from "skipped"; the server writes them, the page sends nothing extra.
 */
export const SETUP_STEPS = ["bind_gateway", "add_device", "first_data", "first_use", "invite_members"] as const;
export type SetupStepId = (typeof SETUP_STEPS)[number];

/** The first uses MS2 offers: the features that are live (MS3 and MS4 add the AI and the reports). */
export const SETUP_USES = ["live_values", "export_csv"] as const;
export type SetupUseId = (typeof SETUP_USES)[number];

export interface SetupStep {
  id: SetupStepId;
  status: "todo" | "done" | "skipped";
  /** When the owning service completed it; `null` unless `done`. */
  completed_at: string | null;
  /** The devices of the tenant, on the `add_device` step only. */
  device_count?: number;
}

export interface SetupUse {
  id: SetupUseId;
  /** The plan that includes the feature; `null` when every plan does. */
  required_plan: string | null;
}

export interface SetupState {
  /** Always the five steps, in `SETUP_STEPS` order. */
  steps: SetupStep[];
  /** The choices of the `first_use` step. */
  uses: SetupUse[];
  /** When the service last looked for the first reading; `null` before it did. */
  last_data_check_at: string | null;
  /** How many more members the plan allows; `null` when it has no limit. */
  remaining_member_slots: number | null;
  /** The one-time 「設定完成」 notice was closed (decision 3). */
  completion_dismissed: boolean;
}

/** Every write answers with the state after it. */
export interface SetupApi {
  getSetup(signal?: AbortSignal): Promise<SetupState>;
  skipStep(step: SetupStepId, signal?: AbortSignal): Promise<SetupState>;
  restoreStep(step: SetupStepId, signal?: AbortSignal): Promise<SetupState>;
  /** Completes the `first_use` step with this choice. */
  chooseUse(use: SetupUseId, signal?: AbortSignal): Promise<SetupState>;
  dismissCompletion(signal?: AbortSignal): Promise<SetupState>;
}

const SetupApiContext = createContext<SetupApi | null>(null);

/** Gives the home page the setup endpoints: a fake server in the UI preview and in tests. */
export function SetupApiProvider({ api, children }: { api: SetupApi; children: ReactNode }) {
  return createElement(SetupApiContext.Provider, { value: api }, children);
}

/**
 * The setup API, or `null` where none is provided: the production build has no such endpoint,
 * so the checklist is not drawn there. When the endpoints exist, this becomes `useSessionBound`
 * over a module on the generated client, like `useDeviceApi`.
 */
export function useSetupApi(): SetupApi | null {
  return useContext(SetupApiContext);
}
