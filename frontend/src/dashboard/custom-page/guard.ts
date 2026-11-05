// The outer page's side of the display-time comparison (frontend/sandbox/README.md, 「顯示時比對的
// 強制」): whether the custom page's frame may be seen. It decides only from what the sandbox
// runtime reports on its port, which the page cannot reach (`PortMessage`), and from what the outer
// page itself knows (the data it sent, its own clock, the frame's loads). Window messages from the
// frame never reach it.
//
// The frame is shown only while all of these hold:
// - the latest comparison is of the data the outer page sent last, and found nothing, and no later
//   data was refused (`dataRefused`);
// - a report arrived within `staleAfterMs` (a page that blocks its thread keeps its last picture
//   on screen and stops every report), and a comparison arrived since the last such gap and since
//   the outer page was shown again after being hidden;
// - nothing closed the frame: a navigation or reload, a `failed` report, no beat within
//   `heartbeatTimeoutMs`, or the page losing its shown state more than `maxRegressions` times (a
//   comparison that found something, or a gap in the reports, while the frame was shown and on
//   screen). A closed frame stays closed; the owner can only start a new one.
// The timers count only while the outer page is shown: a hidden tab throttles both pages' timers.
import { HEARTBEAT_TIMEOUT_MS, type PortMessage, STALE_AFTER_MS } from "../../../sandbox/src/protocol.ts";

/**
 * How often the page may lose its shown state (a comparison of a generation already shown finds
 * something, or its reports stop for `STALE_AFTER_MS`) while it is shown and on screen, before the
 * frame is closed: each such time a value the data does not back may have been on screen for a
 * moment (README, 「顯示時比對的強制」), so a page that keeps doing it is removed, not hidden and
 * shown again for as long as it runs. A page that keeps to its data never loses it: the sandbox
 * plays no CSS transition or animation, so a hover style applies at once. Only a page that escapes
 * that (a transition in a style attribute's `!important`) does, once per hover (README, 「沒有動作」).
 */
export const MAX_REGRESSIONS = 3;

export type CloseReason =
  /** The frame navigated or reloaded: its document is no longer the runtime the port belongs to. */
  | "navigated"
  /** No heartbeat within the timeout. */
  | "stalled"
  /** The comparison failed (`failed` with `comparison`). */
  | "comparison"
  /** The page kept rewriting values the runtime corrected (`failed` with `corrections`). */
  | "corrections"
  /** The page changed what it shows by a way the runtime is not told of (`failed` with `unnotified`). */
  | "unnotified"
  /** The page lost its shown state more than `MAX_REGRESSIONS` times. */
  | "unstable";

export type GuardStatus =
  /** Nothing to show yet: no data, or no clean comparison of the data sent last. */
  | { kind: "loading" }
  /** The page was shown before; the comparison of the data sent since is not in yet. */
  | { kind: "checking" }
  /** The latest comparison of the current data found `count` problems. */
  | { kind: "findings"; count: number }
  /** The outer page's latest data could not be sent (the protocol refused it): nothing is shown. */
  | { kind: "refused" }
  /**
   * A page script or `render` failed, and nothing clean has been compared since. The page's own
   * message is not kept: it is the page's text, which the outer page never shows.
   */
  | { kind: "error" }
  /** No report arrived for `staleAfterMs`, or none since the outer page was shown again. */
  | { kind: "stale" }
  | { kind: "closed"; reason: CloseReason };

export interface GuardView {
  /** Whether the frame may be seen. */
  visible: boolean;
  /** Why it is not visible; `{kind: "loading"}` while it is. */
  status: GuardStatus;
  /** The page's height the runtime last reported, or `null` before the first. */
  height: number | null;
}

/** The timers the guard uses: the page's own, or a test's. */
export interface GuardClock {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

export interface GuardOptions {
  heartbeatTimeoutMs?: number;
  staleAfterMs?: number;
  maxRegressions?: number;
}

export class DisplayGuard {
  private closed: CloseReason | null = null;
  /** The generation of the data sent last; 0 before any. */
  private generation = 0;
  private compared: { generation: number; findings: number } | null = null;
  /** The highest generation shown at least once (for counting regressions). */
  private shownGeneration = 0;
  private regressions = 0;
  private error = false;
  /** The latest data could not be sent; cleared by the next `dataSent`. */
  private refused = false;
  /**
   * No report arrived for `staleAfterMs`, or the outer page was shown again: hidden until the next
   * comparison arrives.
   */
  private stale = false;
  private height: number | null = null;
  private running = false;
  private paused = false;
  /** Whether the frame is in the outer page's viewport (`setOnScreen`). */
  private onScreen = true;
  private watchdog: unknown = null;
  private staleTimer: unknown = null;
  private readonly heartbeatTimeoutMs: number;
  private readonly staleAfterMs: number;
  private readonly maxRegressions: number;
  private last: GuardView;

  constructor(
    private readonly clock: GuardClock,
    private readonly onChange: (view: GuardView) => void,
    options: GuardOptions = {},
  ) {
    this.heartbeatTimeoutMs = options.heartbeatTimeoutMs ?? HEARTBEAT_TIMEOUT_MS;
    this.staleAfterMs = options.staleAfterMs ?? STALE_AFTER_MS;
    this.maxRegressions = options.maxRegressions ?? MAX_REGRESSIONS;
    this.last = this.compute();
  }

  get view(): GuardView {
    return this.last;
  }

  /** `init` went out: the runtime must report from now on. */
  started(): void {
    if (this.closed !== null || this.running) return;
    this.running = true;
    this.arm();
  }

  /** The outer page sent the data of `generation`: only a comparison of it can show the frame. */
  dataSent(generation: number): void {
    if (this.closed !== null || generation <= this.generation) return;
    this.generation = generation;
    this.error = false;
    this.refused = false;
    this.update();
  }

  /**
   * The outer page's latest data could not be sent: whatever the frame shows is of older data, so
   * it stays hidden until data is sent again.
   */
  dataRefused(): void {
    if (this.closed !== null) return;
    this.refused = true;
    this.update();
  }

  /** A report from the runtime's port. Every report says the runtime's thread runs. */
  report(message: PortMessage): void {
    if (this.closed !== null) return;
    if (message.type === "beat") {
      this.arm();
      return;
    }
    this.armStale();
    switch (message.type) {
      case "compared": {
        // Data the outer page has not sent yet cannot have been compared: such a report says nothing.
        if (message.generation > this.generation) return;
        const wasVisible = this.last.visible;
        this.compared = { generation: message.generation, findings: message.findings };
        this.stale = false;
        if (message.findings === 0 && message.generation === this.generation) this.error = false;
        const regressed =
          wasVisible && message.findings > 0 && message.generation === this.generation && this.generation > 0;
        if (regressed && this.regressed()) return;
        this.update();
        return;
      }
      case "failed":
        this.close(message.reason);
        return;
      case "error":
        this.error = true;
        this.update();
        return;
      case "resize":
        this.height = message.height;
        this.update();
        return;
      case "source":
        // The owner handles it, and only while the frame is visible.
        return;
    }
  }

  /** The frame navigated or reloaded (`SandboxHost`'s `onNavigate`). */
  navigated(): void {
    this.close("navigated");
  }

  /** The outer page was hidden (`visibilitychange`): the timers do not count meanwhile. */
  pause(): void {
    if (this.paused) return;
    this.paused = true;
    this.disarm();
  }

  /**
   * The outer page is shown again: the runtime gets full timeouts to report, and the frame waits for
   * a comparison made now (the runtime compares when its document comes back into view).
   */
  resume(): void {
    if (!this.paused) return;
    this.paused = false;
    if (!this.running || this.closed !== null) return;
    this.stale = true;
    this.arm();
    this.update();
  }

  /**
   * Whether the frame is in the outer page's viewport. A browser may slow the timers of a frame
   * out of view, so a gap in its reports then is not counted against the page (it still hides it).
   */
  setOnScreen(onScreen: boolean): void {
    this.onScreen = onScreen;
  }

  /** Stops the timers; the guard reports nothing more. */
  dispose(): void {
    this.disarm();
    this.running = false;
  }

  /** Counts a lost shown state; closes the frame past the limit (returns whether it did). */
  private regressed(): boolean {
    if (++this.regressions > this.maxRegressions) {
      this.close("unstable");
      return true;
    }
    return false;
  }

  private arm(): void {
    if (this.watchdog !== null) this.clock.clearTimeout(this.watchdog);
    this.watchdog = null;
    this.armStale();
    if (!this.running || this.paused || this.closed !== null) return;
    this.watchdog = this.clock.setTimeout(() => {
      this.watchdog = null;
      this.close("stalled");
    }, this.heartbeatTimeoutMs);
  }

  private armStale(): void {
    if (this.staleTimer !== null) this.clock.clearTimeout(this.staleTimer);
    this.staleTimer = null;
    if (!this.running || this.paused || this.closed !== null) return;
    this.staleTimer = this.clock.setTimeout(() => {
      this.staleTimer = null;
      if (this.closed !== null) return;
      const wasVisible = this.last.visible;
      this.stale = true;
      if (wasVisible && this.onScreen && this.regressed()) return;
      this.update();
    }, this.staleAfterMs);
  }

  private disarm(): void {
    if (this.watchdog !== null) this.clock.clearTimeout(this.watchdog);
    if (this.staleTimer !== null) this.clock.clearTimeout(this.staleTimer);
    this.watchdog = null;
    this.staleTimer = null;
  }

  private close(reason: CloseReason): void {
    if (this.closed !== null) return;
    this.closed = reason;
    this.disarm();
    this.update();
  }

  private compute(): GuardView {
    const height = this.height;
    if (this.closed !== null) return { visible: false, status: { kind: "closed", reason: this.closed }, height };
    if (this.refused) return { visible: false, status: { kind: "refused" }, height };
    // An error hides the frame until a clean comparison reported after it (`report` clears it).
    if (this.error) return { visible: false, status: { kind: "error" }, height };
    const current = this.compared !== null && this.compared.generation === this.generation && this.generation > 0;
    if (current && this.compared !== null && this.compared.findings > 0) {
      return { visible: false, status: { kind: "findings", count: this.compared.findings }, height };
    }
    if (this.running && this.stale) return { visible: false, status: { kind: "stale" }, height };
    if (this.running && current && this.compared?.findings === 0) {
      return { visible: true, status: { kind: "loading" }, height };
    }
    if (this.shownGeneration > 0) return { visible: false, status: { kind: "checking" }, height };
    return { visible: false, status: { kind: "loading" }, height };
  }

  private update(): void {
    const next = this.compute();
    if (next.visible) this.shownGeneration = Math.max(this.shownGeneration, this.generation);
    const before = this.last;
    this.last = next;
    if (
      before.visible !== next.visible ||
      before.height !== next.height ||
      JSON.stringify(before.status) !== JSON.stringify(next.status)
    ) {
      this.onChange(next);
    }
  }
}
