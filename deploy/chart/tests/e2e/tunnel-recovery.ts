// Recovery from a lost tunnel to the ingress gateway (Laysi/trustiot-v2#261). The e2e test reaches
// the gateway through `kubectl port-forward` or `ssh -L`. When that process dies while a request
// is in flight, the request fails first (empty response, reset) and the process only counts as
// exited a moment later: kubectl prints "lost connection to pod" and exits after the client
// socket was already closed. Checking the exit state right when the request fails therefore
// misses the loss. This module waits a bounded time for the exit, then reopens the tunnel and
// sends the request once more.
import { ConnectionError } from "./raw-client.ts";

// The part of a Bun Subprocess that the recovery looks at.
export type TunnelProcess = {
  readonly exited: Promise<unknown>;
  readonly exitCode: number | null;
  readonly signalCode: string | null;
};

export type RecoveryOptions = {
  // The tunnel in use now; undefined when the test talks to the gateway without one.
  tunnel: () => TunnelProcess | undefined;
  // Starts a new tunnel and points the client at it.
  reopen: () => Promise<void>;
  // How long a connection failure may wait for the tunnel to exit; default 5 s.
  graceMs?: number;
  log?: (message: string) => void;
};

export const DEFAULT_GRACE_MS = 5_000;

async function exitsWithin(tunnel: TunnelProcess, graceMs: number): Promise<boolean> {
  if (tunnel.exitCode !== null || tunnel.signalCode !== null) return true;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<false>((resolve) => {
    timer = setTimeout(() => resolve(false), graceMs);
  });
  try {
    return await Promise.race([tunnel.exited.then(() => true as const), timeout]);
  } finally {
    clearTimeout(timer);
  }
}

// Runs `attempt`. Retries it once, after reopening the tunnel, only when it failed with a
// ConnectionError and the tunnel process exits within the grace period. An HTTP response of any
// status is returned as is, any other error is rethrown, a second failure is not retried, and a
// connection failure while the tunnel stays up is rethrown too (the gateway really dropped it).
export async function withTunnelRecovery<T>(attempt: () => Promise<T>, options: RecoveryOptions): Promise<T> {
  try {
    return await attempt();
  } catch (error) {
    if (!(error instanceof ConnectionError)) throw error;
    const tunnel = options.tunnel();
    if (tunnel === undefined) throw error;
    if (!(await exitsWithin(tunnel, options.graceMs ?? DEFAULT_GRACE_MS))) throw error;
    options.log?.(
      `tunnel to the ingress gateway exited (${tunnel.exitCode ?? tunnel.signalCode}) during a request ` +
        `(${error.message}); reopening it`,
    );
    await options.reopen();
    return attempt();
  }
}
