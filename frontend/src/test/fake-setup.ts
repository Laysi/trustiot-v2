import { createElement, type ReactNode } from "react";
import { ApiError } from "../api/errors.ts";
import {
  SETUP_STEPS,
  type SetupApi,
  SetupApiProvider,
  type SetupState,
  type SetupStep,
  type SetupStepId,
  type SetupUseId,
} from "../api/pending/setup.ts";
import { HomeTopSlotProvider } from "../pages/home/HomeSlot.tsx";
import { SetupChecklist } from "../pages/home/SetupChecklist.tsx";

/**
 * The setup endpoints (`api/pending/setup.ts`) in memory. It keeps the rules the interface states:
 * the page can only skip a step that is still to do, take a skip back and choose the first use;
 * every other step is completed by `complete`, which stands for the service that owns it. Every
 * call is recorded; `failNext` makes the next call to an operation fail.
 */

export type SetupOperation = "get" | "skip" | "restore" | "choose" | "dismiss";

export function unavailable(): ApiError {
  return new ApiError(503, { code: "unavailable", message_key: "error.unavailable" });
}

function emptyState(): SetupState {
  return {
    steps: SETUP_STEPS.map((id): SetupStep => ({ id, status: "todo", completed_at: null })),
    uses: [
      { id: "live_values", required_plan: null },
      { id: "export_csv", required_plan: "Business" },
    ],
    last_data_check_at: null,
    remaining_member_slots: 4,
    completion_dismissed: false,
  };
}

export class FakeSetupServer {
  state: SetupState = emptyState();
  /** Every call, in order: the operation and its argument. */
  readonly calls: string[] = [];
  private readonly failures = new Map<SetupOperation, ApiError[]>();

  /** A tenant that bound a gateway and added three devices and waits for the first reading (design page 01). */
  static midway(): FakeSetupServer {
    const server = new FakeSetupServer();
    server.complete("bind_gateway", "2026-11-01T09:12:00Z");
    server.complete("add_device", "2026-11-01T09:40:00Z", 3);
    server.state.last_data_check_at = "2026-11-01T18:24:00Z";
    return server;
  }

  /** The service that owns `step` completes it. */
  complete(step: SetupStepId, at: string, deviceCount?: number): void {
    const found = this.step(step);
    found.status = "done";
    found.completed_at = at;
    if (deviceCount !== undefined) found.device_count = deviceCount;
  }

  failNext(operation: SetupOperation, error: ApiError = unavailable()): void {
    this.failures.set(operation, [...(this.failures.get(operation) ?? []), error]);
  }

  private step(id: SetupStepId): SetupStep {
    const found = this.state.steps.find((step) => step.id === id);
    if (found === undefined) throw new Error(`no step ${id}`);
    return found;
  }

  private snapshot(): SetupState {
    return structuredClone(this.state);
  }

  private async enter(operation: SetupOperation, argument: string): Promise<void> {
    this.calls.push(argument === "" ? operation : `${operation}:${argument}`);
    // An answer is never synchronous, as a request's is not.
    await Promise.resolve();
    const failure = this.failures.get(operation)?.shift();
    if (failure !== undefined) throw failure;
  }

  private conflict(): ApiError {
    return new ApiError(409, { code: "conflict", message_key: "error.conflict" });
  }

  api(): SetupApi {
    return {
      getSetup: async () => {
        await this.enter("get", "");
        return this.snapshot();
      },
      skipStep: async (id) => {
        await this.enter("skip", id);
        const step = this.step(id);
        if (step.status !== "todo") throw this.conflict();
        step.status = "skipped";
        return this.snapshot();
      },
      restoreStep: async (id) => {
        await this.enter("restore", id);
        const step = this.step(id);
        if (step.status !== "skipped") throw this.conflict();
        step.status = "todo";
        // The owning service finds out what it already knows, as it does for a step done before it was skipped.
        this.state.completion_dismissed = false;
        return this.snapshot();
      },
      chooseUse: async (use: SetupUseId) => {
        await this.enter("choose", use);
        const step = this.step("first_use");
        if (step.status === "done" || (use !== "weekly_report" && !this.state.uses.some((item) => item.id === use)))
          throw this.conflict();
        step.status = "done";
        step.completed_at = new Date().toISOString();
        return this.snapshot();
      },
      dismissCompletion: async () => {
        await this.enter("dismiss", "");
        this.state.completion_dismissed = true;
        return this.snapshot();
      },
    };
  }
}

/** For the UI preview (Laysi/trustiot-v2#1065): "normal" is a tenant midway, "empty" a new one. */
export const preview = {
  name: "Setup checklist",
  provider: (seed: "normal" | "empty") => {
    const server = seed === "empty" ? new FakeSetupServer() : FakeSetupServer.midway();
    const api = server.api();
    return function SetupPreviewProvider({ children }: { children: ReactNode }) {
      return createElement(
        SetupApiProvider,
        { api },
        createElement(HomeTopSlotProvider, { slot: createElement(SetupChecklist), children }),
      );
    };
  },
};
