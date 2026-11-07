import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, render } from "@testing-library/react";
import { useRef } from "react";

import { STICKY_BOTTOM, STICKY_TOP, type StickyEdge, useStickyHeight, visibleBand } from "./sticky-bars.ts";

/** A `ResizeObserver` the test drives: `resize` reports the observed elements. */
function fakeResizeObserver() {
  const original = globalThis.ResizeObserver;
  const observers = new Set<{ callback: ResizeObserverCallback; targets: Set<Element> }>();
  class FakeObserver {
    private readonly entry: { callback: ResizeObserverCallback; targets: Set<Element> };
    constructor(callback: ResizeObserverCallback) {
      this.entry = { callback, targets: new Set() };
      observers.add(this.entry);
    }
    observe(target: Element) {
      this.entry.targets.add(target);
    }
    unobserve(target: Element) {
      this.entry.targets.delete(target);
    }
    disconnect() {
      observers.delete(this.entry);
    }
  }
  globalThis.ResizeObserver = FakeObserver as unknown as typeof ResizeObserver;
  return {
    resize() {
      act(() => {
        for (const { callback, targets } of [...observers]) {
          const entries = [...targets].map((target) => ({ target }) as ResizeObserverEntry);
          if (entries.length > 0) callback(entries, {} as ResizeObserver);
        }
      });
    },
    observing: () => [...observers].reduce((count, { targets }) => count + targets.size, 0),
    restore() {
      globalThis.ResizeObserver = original;
    },
  };
}

/** A bar of `height()` px. */
function Bar({ edge, height }: { edge: StickyEdge; height: () => number }) {
  const ref = useRef<HTMLDivElement>(null);
  useStickyHeight(ref, edge);
  return (
    <div
      ref={(element) => {
        ref.current = element;
        if (element) element.getBoundingClientRect = () => ({ height: height() }) as DOMRect;
      }}
    />
  );
}

const published = (edge: StickyEdge) => document.documentElement.style.getPropertyValue(edge);

let observer: ReturnType<typeof fakeResizeObserver> | null = null;
afterEach(() => {
  cleanup();
  observer?.restore();
  observer = null;
});

describe("the sticky bars' heights", () => {
  test("a bar publishes its height in whole pixels, follows its size, and takes it away when it leaves", () => {
    observer = fakeResizeObserver();
    let height = 129.4;
    const { unmount } = render(<Bar edge={STICKY_BOTTOM} height={() => height} />);
    expect(published(STICKY_BOTTOM)).toBe("130px");
    height = 186;
    observer.resize();
    expect(published(STICKY_BOTTOM)).toBe("186px");
    unmount();
    expect(published(STICKY_BOTTOM)).toBe("");
    expect(observer.observing()).toBe(0);
  });

  test("the top and the bottom are separate, and the bar mounted last owns its edge", () => {
    observer = fakeResizeObserver();
    const top = render(<Bar edge={STICKY_TOP} height={() => 57} />);
    const first = render(<Bar edge={STICKY_BOTTOM} height={() => 130} />);
    const second = render(<Bar edge={STICKY_BOTTOM} height={() => 90} />);
    expect(published(STICKY_TOP)).toBe("57px");
    expect(published(STICKY_BOTTOM)).toBe("90px");
    // The older bar leaving does not take away the newer one's height, nor does its resize win.
    observer.resize();
    expect(published(STICKY_BOTTOM)).toBe("90px");
    first.unmount();
    expect(published(STICKY_BOTTOM)).toBe("90px");
    second.unmount();
    expect(published(STICKY_BOTTOM)).toBe("");
    expect(published(STICKY_TOP)).toBe("57px");
    top.unmount();
    expect(published(STICKY_TOP)).toBe("");
  });

  test("without a layout the band is the whole window, not a number that is not one", () => {
    const band = visibleBand();
    expect(Number.isFinite(band.top)).toBe(true);
    expect(Number.isFinite(band.bottom)).toBe(true);
  });
});
