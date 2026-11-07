import { type RefObject, useEffect } from "react";

import { visibleBand } from "../../lib/sticky-bars.ts";
import {
  type Follow,
  type FollowEvent,
  followBehavior,
  nextFollow,
  REDUCED_MOTION,
  START,
  scrollToNewest,
} from "./follow.ts";

/** How long the wheel must be still before the reader counts as no longer scrolling with it. */
const WHEEL_QUIET_MS = 200;

/**
 * Keeps the question being answered in view (Laysi/trustiot-v2#1182): when it appears it is
 * scrolled between the top bar and the composer (the page scroller's scroll-padding, see
 * lib/sticky-bars.ts), and while its answer grows, or the composer changes size, its newest part
 * stays above the composer for as long as the reader stays at the bottom. The decisions are in
 * follow.ts; this is the DOM side: measurements in, scrolling out.
 */
export function useFollowNewest(
  content: RefObject<HTMLElement | null>,
  composer: RefObject<HTMLElement | null> | undefined,
): void {
  useEffect(() => {
    const element = content.current;
    if (element === null) return;
    const bar = composer?.current ?? null;
    const reducedMotion = () => typeof window.matchMedia === "function" && window.matchMedia(REDUCED_MOTION).matches;
    const below = () => element.getBoundingClientRect().bottom - visibleBand().bottom;

    let state: Follow = START;
    let lastY = window.scrollY;
    let quiet: ReturnType<typeof setTimeout> | undefined;
    const follow = () => {
      if (scrollToNewest(state, below())) {
        element.scrollIntoView?.({ block: "end", behavior: followBehavior(reducedMotion()) });
      }
    };
    const on = (event: FollowEvent) => {
      state = nextFollow(state, event);
    };

    // When it appears: into view between the bars, as little scrolling as that takes.
    element.scrollIntoView?.({ block: "nearest", behavior: followBehavior(reducedMotion()) });

    const onScroll = () => {
      const y = window.scrollY;
      on({ kind: "scroll", deltaY: y - lastY, below: below() });
      lastY = y;
    };
    const onWheel = () => {
      on({ kind: "wheel" });
      clearTimeout(quiet);
      quiet = setTimeout(() => {
        on({ kind: "wheel-quiet" });
        follow();
      }, WHEEL_QUIET_MS);
    };
    const onTouchStart = () => on({ kind: "touch-start" });
    const onTouchEnd = () => {
      on({ kind: "touch-end" });
      follow();
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("wheel", onWheel, { passive: true });
    window.addEventListener("touchstart", onTouchStart, { passive: true });
    window.addEventListener("touchend", onTouchEnd, { passive: true });
    window.addEventListener("touchcancel", onTouchEnd, { passive: true });

    // A resize observer reports every target once when it starts; only a change of size counts.
    const heights = new Map<Element, number>();
    const measured = (target: Element) => heights.set(target, target.getBoundingClientRect().height);
    const observer =
      typeof ResizeObserver === "function"
        ? new ResizeObserver((entries) => {
            let changed = false;
            for (const { target } of entries) {
              const height = target.getBoundingClientRect().height;
              if (heights.get(target) !== height) changed = true;
              heights.set(target, height);
            }
            if (changed) follow();
          })
        : null;
    for (const target of bar === null ? [element] : [element, bar]) {
      measured(target);
      observer?.observe(target);
    }

    return () => {
      observer?.disconnect();
      clearTimeout(quiet);
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("wheel", onWheel);
      window.removeEventListener("touchstart", onTouchStart);
      window.removeEventListener("touchend", onTouchEnd);
      window.removeEventListener("touchcancel", onTouchEnd);
    };
  }, [content, composer]);
}
