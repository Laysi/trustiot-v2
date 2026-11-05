import type { Clock, StreamFetch, StreamResponse } from "../api/sse.ts";

// Test helpers for the server-sent event streams (api/sse.ts and what reads them): a clock that
// moves only when the test says so, a stream body the test writes to, and a fetch that answers
// with the responses the test lines up. No helper waits on real time.

/** Timers that move only when the test says so. */
export class ManualClock implements Clock {
  now = 0;
  private next = 1;
  private readonly timers = new Map<number, { at: number; callback: () => void }>();

  setTimeout(callback: () => void, ms: number): unknown {
    const id = this.next++;
    this.timers.set(id, { at: this.now + ms, callback });
    return id;
  }

  clearTimeout(handle: unknown): void {
    this.timers.delete(handle as number);
  }

  /** Moves time forward, firing every timer that falls due on the way, in order. */
  async advance(ms: number): Promise<void> {
    const target = this.now + ms;
    for (;;) {
      await settle();
      const due = [...this.timers.entries()]
        .filter(([, timer]) => timer.at <= target)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) break;
      this.timers.delete(due[0]);
      this.now = due[1].at;
      due[1].callback();
    }
    this.now = target;
    await settle();
  }
}

/** Lets pending promise callbacks and stream reads run; no real time passes. */
export async function settle(): Promise<void> {
  for (let turn = 0; turn < 5; turn += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
}

/** Waits, by event loop turns rather than time, until `condition` holds. */
export async function until(what: string, condition: () => boolean): Promise<void> {
  for (let turn = 0; turn < 1_000; turn += 1) {
    if (condition()) return;
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
  throw new Error(`timed out waiting for ${what}`);
}

export function fakeStream() {
  let controller: ReadableStreamDefaultController<Uint8Array> | undefined;
  let cancelled = false;
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    },
    cancel() {
      cancelled = true;
    },
  });
  const encoder = new TextEncoder();
  return {
    body,
    get cancelled() {
      return cancelled;
    },
    send(text: string) {
      controller?.enqueue(encoder.encode(text));
    },
    end() {
      controller?.close();
    },
  };
}

export function eventStream(body: ReadableStream<Uint8Array>): StreamResponse {
  return { status: 200, headers: new Headers({ "content-type": "text/event-stream; charset=utf-8" }), body };
}

export function status(code: number): StreamResponse {
  return { status: code, headers: new Headers({ "content-type": "text/plain" }), body: null };
}

/** A fetch that answers with the given responses in order, then never answers. */
export function fakeFetch(responses: Array<() => Promise<StreamResponse>>) {
  const calls: Array<{ url: string; headers: Record<string, string>; credentials: RequestCredentials | undefined }> =
    [];
  const fetch: StreamFetch = (url, init) => {
    calls.push({ url, headers: { ...(init.headers as Record<string, string>) }, credentials: init.credentials });
    const next = responses.shift();
    return next ? next() : new Promise<StreamResponse>(() => {});
  };
  return { fetch, calls };
}
