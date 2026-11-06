import { createElement } from "react";

import { ApiError } from "../api/errors.ts";
import { CompletionApiProvider } from "../api/pending/CompletionApiProvider.tsx";
import type { CompleteRequest, CompleteResponse, CompletionApi } from "../api/pending/completion.ts";
import type { ProviderPreviewDeclaration } from "../ui-preview/contract.ts";

/**
 * The grey-text completion endpoint, in memory (api/pending/completion.ts: the endpoint does
 * not exist yet). It answers with demo continuations, and has switches for what the composer has
 * to handle:
 *
 * - `hold()` makes answers wait until `release()`, so a test can type again while one is in
 *   flight; `honorAbort = false` makes it ignore the signal, like a service that does not stop,
 *   so the composer's own check for a late answer is tested;
 * - `failNext()` and `failing` answer 500 `internal`;
 * - `limit` is the per-user limit (J11): past `requests` answers in `windowSecs`, 429
 *   `rate_limited` with `details.retry_after_secs` until the oldest answer leaves the window;
 *   `limitFor(seconds)` limits for a fixed time from now;
 * - in the UI preview, text with `[slow]` is answered after 3 s, `[fail]` fails and `[limit]` is
 *   limited for 60 s, so each state can be clicked without a switch.
 *
 * Every request is recorded in `requests`, with the signal it was sent with.
 */

export interface CompletionRequestRecord {
  text: string;
  signal: AbortSignal;
}

export interface CompletionLimit {
  requests: number;
  windowSecs: number;
}

/** The M1 error body of a limited request (crates/contract/src/error.rs). */
export function rateLimited(retryAfterSecs: number): ApiError {
  return new ApiError(429, {
    code: "rate_limited",
    message_key: "error.rate_limited",
    details: { retry_after_secs: retryAfterSecs },
  });
}

function internalError(): ApiError {
  return new ApiError(500, { code: "internal", message_key: "error.internal" });
}

const PREVIEW_SLOW_MS = 3000;
const PREVIEW_LIMIT_SECS = 60;

/**
 * A continuation of `text` for the demo, or an empty string when there is nothing to add (a
 * finished sentence). A real model writes it from the tenant's device and data point names.
 */
export function demoContinuation(text: string): string {
  const typed = text.trimEnd();
  if (/[?？。.!]$/.test(typed)) return "";
  const spaced = (words: string) => (/\s$/.test(text) ? words.trimStart() : words);
  if (/這週的用電$/.test(typed)) return "比上週多了多少？主要差在哪幾天";
  if (/用電$/.test(typed)) return "跟上週比有什麼不同？";
  if (/離線/.test(typed)) return "的原因是什麼？什麼時候開始的";
  if (/異常/.test(typed)) return "發生在什麼時候？持續多久";
  if (/offline/i.test(typed)) return spaced(" and since when?");
  if (/anomal/i.test(typed)) return spaced(" and how long did it last?");
  if (/energy/i.test(typed)) return spaced(" this week compared with last week?");
  if (/[㐀-鿿]/.test(typed)) return "，跟上週比呢？";
  return spaced(" compared with last week?");
}

export class FakeCompletionServer implements CompletionApi {
  readonly requests: CompletionRequestRecord[] = [];
  /** Answers wait for `release()` while this is set. */
  held = false;
  /** Whether a request stops when its signal is aborted; a service that ignores it answers anyway. */
  honorAbort = true;
  /** Every request fails with 500 `internal` while this is set. */
  failing = false;
  limit: CompletionLimit | null = null;
  /** The clock the limit counts with; a test passes its own. */
  now: () => number = () => Date.now();

  private failures = 0;
  private limitedUntil = 0;
  private answered: number[] = [];
  private waiting: (() => void)[] = [];

  /** The next request fails with 500 `internal`. */
  failNext(): void {
    this.failures += 1;
  }

  /** Limits every request for `seconds` from now, with 429 `rate_limited`. */
  limitFor(seconds: number): void {
    this.limitedUntil = this.now() + seconds * 1000;
  }

  hold(): void {
    this.held = true;
  }

  /** Lets the held requests answer, and the ones sent from now on. */
  release(): void {
    this.held = false;
    for (const wake of this.waiting.splice(0)) wake();
  }

  private retryAfter(): number | null {
    const now = this.now();
    if (now < this.limitedUntil) return Math.ceil((this.limitedUntil - now) / 1000);
    if (this.limit === null) return null;
    const windowMs = this.limit.windowSecs * 1000;
    this.answered = this.answered.filter((at) => now - at < windowMs);
    const oldest = this.answered[0];
    if (this.answered.length < this.limit.requests || oldest === undefined) return null;
    return Math.ceil((oldest + windowMs - now) / 1000);
  }

  private pause(ms: number | null, signal: AbortSignal): Promise<void> {
    return new Promise((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const done = () => {
        if (timer !== undefined) clearTimeout(timer);
        signal.removeEventListener("abort", aborted);
        resolve();
      };
      const aborted = () => {
        if (timer !== undefined) clearTimeout(timer);
        const index = this.waiting.indexOf(done);
        if (index >= 0) this.waiting.splice(index, 1);
        reject(signal.reason ?? new DOMException("aborted", "AbortError"));
      };
      if (this.honorAbort) signal.addEventListener("abort", aborted);
      if (ms === null) this.waiting.push(done);
      else timer = setTimeout(done, ms);
    });
  }

  async complete(request: CompleteRequest, options: { signal: AbortSignal }): Promise<CompleteResponse> {
    const { signal } = options;
    this.requests.push({ text: request.text, signal });
    if (signal.aborted && this.honorAbort) throw signal.reason ?? new DOMException("aborted", "AbortError");
    const retry = request.text.includes("[limit]") ? PREVIEW_LIMIT_SECS : this.retryAfter();
    if (retry !== null) throw rateLimited(retry);
    this.answered.push(this.now());
    if (this.held) await this.pause(null, signal);
    else if (request.text.includes("[slow]")) await this.pause(PREVIEW_SLOW_MS, signal);
    if (this.failing || this.failures > 0 || request.text.includes("[fail]")) {
      this.failures = Math.max(0, this.failures - 1);
      throw internalError();
    }
    return { completion: demoContinuation(request.text) };
  }
}

export const preview: ProviderPreviewDeclaration = {
  name: "completion",
  provider: () => {
    const api = new FakeCompletionServer();
    return ({ children }) => createElement(CompletionApiProvider, { api, children });
  },
};
