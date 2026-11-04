import type { PreviewFetch } from "./contract.ts";

/**
 * The toolbar's switches that act on the next requests without reloading: make the next request
 * fail (or every request, until switched off), and make every request slow. The preview's fetch
 * (`routeRequests`) reads them for each request; the toolbar subscribes to show their state (a
 * failure for the next request is used up by one request). The query client retries a 503 and a
 * network error (`ApiError.retryable`), so one such failure is hidden by the retry: `keepFailing`
 * shows their error state.
 */

export type FailureKind = "unavailable" | "internal" | "permission_denied" | "network";
export const FAILURE_KINDS: readonly FailureKind[] = ["unavailable", "internal", "permission_denied", "network"];

const FAILURE_STATUS: Record<Exclude<FailureKind, "network">, number> = {
  unavailable: 503,
  internal: 500,
  permission_denied: 403,
};

export const SLOW_DELAYS_MS: readonly number[] = [0, 2_000, 10_000];

export interface ControlsState {
  failNext: FailureKind | null;
  /** The failure is not used up: every request fails until `failNext` is cleared. */
  keepFailing: boolean;
  slowMs: number;
}

export class PreviewControls {
  private state: ControlsState;
  private readonly listeners = new Set<() => void>();

  constructor(initial: Partial<ControlsState> = {}) {
    this.state = {
      failNext: initial.failNext ?? null,
      keepFailing: initial.keepFailing ?? false,
      slowMs: initial.slowMs ?? 0,
    };
  }

  get = (): ControlsState => this.state;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  set(change: Partial<ControlsState>): void {
    this.state = { ...this.state, ...change };
    for (const listener of this.listeners) listener();
  }

  /** The failure armed for this request, if any; it is used up unless `keepFailing`. */
  takeFailure(): FailureKind | null {
    const failure = this.state.failNext;
    if (failure !== null && !this.state.keepFailing) this.set({ failNext: null });
    return failure;
  }
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

function wait(ms: number, signal: AbortSignal): Promise<void> {
  if (ms <= 0) return Promise.resolve();
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(signal.reason);
      return;
    }
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        reject(signal.reason);
      },
      { once: true },
    );
  });
}

export interface PreviewRoute {
  prefix: string;
  fetch: PreviewFetch;
}

/** Whether `prefix` covers `pathname`: the same path, or a path below it. */
function covers(prefix: string, pathname: string): boolean {
  if (pathname === prefix) return true;
  if (!pathname.startsWith(prefix)) return false;
  return prefix.endsWith("/") || pathname.charAt(prefix.length) === "/";
}

/**
 * The preview's fetch: each request goes to the fake whose prefix is the longest one covering its
 * path, after the slow delay and unless a failure is armed. A path no fake answers is 404
 * `not_found` (and a warning in the console naming it); nothing goes to the network.
 */
export function routeRequests(routes: readonly PreviewRoute[], controls: PreviewControls): PreviewFetch {
  const ordered = [...routes].sort((a, b) => b.prefix.length - a.prefix.length);
  return async (request) => {
    await wait(controls.get().slowMs, request.signal);
    const failure = controls.takeFailure();
    if (failure === "network") throw new TypeError("Failed to fetch (UI preview: the next request fails)");
    if (failure !== null) return json(FAILURE_STATUS[failure], { code: failure, message_key: `error.${failure}` });
    const { pathname } = new URL(request.url);
    const route = ordered.find((candidate) => covers(candidate.prefix, pathname));
    if (route === undefined) {
      console.warn(`UI preview: no fake answers ${request.method} ${pathname}`);
      return json(404, { code: "not_found", message_key: "error.not_found" });
    }
    return route.fetch(request);
  };
}
