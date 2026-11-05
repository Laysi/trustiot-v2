import { useQueries, useQueryClient } from "@tanstack/react-query";
import { createContext, useCallback, useContext, useMemo, useRef, useState } from "react";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { DatasetState } from "../../dashboard/data.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { runsKey } from "./queries.ts";

/**
 * How many datasets of one page run at once: the assistant runs at most 2 per tenant at a time
 * and makes the others wait (services/assistant/README.md 「資料集的執行」), so asking for more only
 * fills its wait queue, and a third run waits there while the page could have started it later.
 */
export const RUNS_AT_ONCE = 2;

/**
 * Runs tasks at most `limit` at a time, in the order they were asked for. A task whose signal
 * aborts while it waits is dropped without running.
 */
export class RunLimiter {
  private active = 0;
  private readonly waiting: { start(): void; signal: AbortSignal }[] = [];

  constructor(private readonly limit: number) {}

  run<T>(task: () => Promise<T>, signal: AbortSignal): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const start = () => {
        this.active += 1;
        // A task that throws synchronously becomes a rejection, so it still gives its slot back.
        let running: Promise<T>;
        try {
          running = task();
        } catch (error) {
          running = Promise.reject(error);
        }
        running.then(resolve, reject).finally(() => {
          this.active -= 1;
          this.next();
        });
      };
      if (signal.aborted) {
        reject(signal.reason);
        return;
      }
      if (this.active < this.limit) {
        start();
        return;
      }
      const entry = { start, signal };
      this.waiting.push(entry);
      signal.addEventListener(
        "abort",
        () => {
          const index = this.waiting.indexOf(entry);
          if (index >= 0) {
            this.waiting.splice(index, 1);
            reject(signal.reason);
          }
        },
        { once: true },
      );
    });
  }

  private next(): void {
    while (this.active < this.limit) {
      const entry = this.waiting.shift();
      if (entry === undefined) return;
      if (!entry.signal.aborted) entry.start();
    }
  }
}

/**
 * A limiter shared by every dataset run under it. The assistant's limit of 2 at once is the
 * tenant's, not one dashboard's, so a page that shows several dashboards at once (a conversation
 * with several answers and its draft panel) provides one; without it each `useDatasetRuns` has
 * its own.
 */
export const RunLimiterContext = createContext<RunLimiter | null>(null);

function sameState(a: DatasetState, b: DatasetState): boolean {
  if (a.status === "ready" && b.status === "ready") return a.result === b.result;
  if (a.status === "failed" && b.status === "failed") return a.message === b.message;
  return a.status === b.status;
}

function sameStates(a: readonly [string, DatasetState][], b: readonly [string, DatasetState][]): boolean {
  return (
    a.length === b.length &&
    a.every(([name, state], index) => {
      const other = b[index];
      return other !== undefined && other[0] === name && sameState(state, other[1]);
    })
  );
}

export interface DatasetRuns {
  /** Each dataset's state for this viewing, by name (own properties only: see `stateOf`). */
  results: Readonly<Record<string, DatasetState>>;
  /** Whether any dataset is running or waiting to. */
  running: boolean;
  /**
   * Whether the datasets are being run again after a result was shown: the page keeps the earlier
   * content, dimmed, until the new results replace it (docs/design/chat-dashboards/design.md,
   * 「更新資料」). The first run of a dataset is not a refresh: nothing is shown yet.
   */
  refreshing: boolean;
  /** Runs every dataset again, whatever the earlier results were; they stay shown until replaced. */
  refresh(): void;
  /** Runs one dataset again. */
  retry(dataset: string): void;
}

/**
 * Runs each dataset of one version as the viewer (`POST …/dataset-runs`), `RUNS_AT_ONCE` at a
 * time in the order of `datasets`, and gives their states for the components. Nothing is reused:
 * a result lives as long as the page shows this version (decision 33, 「每次打開與重新整理都重新
 * 查詢」), and `refresh` runs every dataset again, the request going out whatever is held. The
 * results of the earlier run stay shown while it runs, and a result that fails replaces the
 * earlier one with the failure. A failed run is not retried by itself, a 429 included: the card
 * says why and offers to retry.
 */
export function useDatasetRuns(
  dashboardId: string,
  version: number,
  datasets: readonly string[],
  /** Whether the datasets run at all; while not, they show as loading (or as the results another view of them holds). */
  enabled = true,
): DatasetRuns {
  const api = useDashboardApi();
  const { errorMessage } = useI18n();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [ownLimiter] = useState(() => new RunLimiter(RUNS_AT_ONCE));
  const limiter = useContext(RunLimiterContext) ?? ownLimiter;
  const key = useMemo(() => runsKey(tenant, dashboardId, version), [tenant, dashboardId, version]);

  const queries = useQueries({
    queries: datasets.map((dataset) => ({
      queryKey: [...key, dataset] as const,
      queryFn: ({ signal }: { signal: AbortSignal }) =>
        limiter.run(() => api.runDataset(dashboardId, { version, dataset }, signal), signal),
      enabled,
      retry: false,
      staleTime: Number.POSITIVE_INFINITY,
      gcTime: 0,
      refetchOnReconnect: false,
    })),
  });

  const states = datasets.map((dataset, index): [string, DatasetState] => {
    const query = queries[index];
    if (query === undefined || query.isPending || (query.isFetching && query.isError)) {
      return [dataset, { status: "loading" }];
    }
    if (query.isError) return [dataset, { status: "failed", message: errorMessage(query.error) }];
    return [dataset, { status: "ready", result: query.data }];
  });
  // The same object while no state changed, so the components (which memoize on their data)
  // draw again only then; a result fetched again is a new object.
  const previous = useRef<{ states: [string, DatasetState][]; results: Record<string, DatasetState> } | null>(null);
  if (previous.current === null || !sameStates(previous.current.states, states)) {
    previous.current = { states, results: Object.fromEntries(states) };
  }
  const results = previous.current.results;

  const refresh = useCallback(() => {
    // A refetch keeps the result shown until the new one arrives; a run still in flight is cancelled
    // and started again, so the new results are never an older request's.
    void queryClient.refetchQueries({ queryKey: key });
  }, [queryClient, key]);
  const retry = useCallback(
    (dataset: string) => {
      void queryClient.resetQueries({ queryKey: [...key, dataset], exact: true });
    },
    [queryClient, key],
  );

  const refreshing = queries.some((query) => query.isFetching && query.data !== undefined);
  return { results, running: queries.some((query) => query.isFetching), refreshing, refresh, retry };
}
