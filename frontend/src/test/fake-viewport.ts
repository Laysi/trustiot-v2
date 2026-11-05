import { act } from "@testing-library/react";

/**
 * An `IntersectionObserver` the test drives: nothing is near the viewport until `enter` says so
 * (happy-dom's never reports anything, and a real one reports what the layout says). `restore`
 * puts the original back; call it after each test.
 */
export interface FakeViewport {
  /** The `rootMargin` each observer was made with. */
  margins: string[];
  /** The elements being observed now. */
  observed(): Element[];
  /** Reports these elements (all that are observed when none are named) as intersecting. */
  enter(...elements: Element[]): void;
  restore(): void;
}

export function fakeViewport(): FakeViewport {
  const original = globalThis.IntersectionObserver;
  const observers = new Set<FakeObserver>();
  const margins: string[] = [];

  class FakeObserver {
    readonly root = null;
    readonly rootMargin: string;
    readonly thresholds = [0];
    readonly targets = new Set<Element>();
    constructor(
      private readonly callback: IntersectionObserverCallback,
      options?: IntersectionObserverInit,
    ) {
      this.rootMargin = options?.rootMargin ?? "0px";
      margins.push(this.rootMargin);
      observers.add(this);
    }
    observe(target: Element): void {
      this.targets.add(target);
    }
    unobserve(target: Element): void {
      this.targets.delete(target);
    }
    disconnect(): void {
      this.targets.clear();
      observers.delete(this);
    }
    takeRecords(): IntersectionObserverEntry[] {
      return [];
    }
    report(targets: readonly Element[]): void {
      const entries = targets
        .filter((target) => this.targets.has(target))
        .map((target) => ({ target, isIntersecting: true }) as IntersectionObserverEntry);
      if (entries.length > 0) this.callback(entries, this as unknown as IntersectionObserver);
    }
  }
  globalThis.IntersectionObserver = FakeObserver as unknown as typeof IntersectionObserver;

  const observed = () => [...observers].flatMap((observer) => [...observer.targets]);
  return {
    margins,
    observed,
    enter: (...elements) => {
      const targets = elements.length > 0 ? elements : observed();
      act(() => {
        for (const observer of [...observers]) observer.report(targets);
      });
    },
    restore: () => {
      globalThis.IntersectionObserver = original;
    },
  };
}
