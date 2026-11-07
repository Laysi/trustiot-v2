import { type RefObject, useLayoutEffect } from "react";

/**
 * The page's sticky bars (the top bar of every signed-in page, the conversation page's composer)
 * and the part of the window left between them (Laysi/trustiot-v2#1182). Each bar publishes its
 * measured height as a CSS variable on the root element, and global.css makes the two variables
 * the page scroller's `scroll-padding-top` and `scroll-padding-bottom`. So `scrollIntoView`, the
 * scrolling that focusing an element does, and fragment navigation stop at the bars instead of
 * behind them, without the caller knowing which bars the page has.
 */
export const STICKY_TOP = "--sticky-top";
export const STICKY_BOTTOM = "--sticky-bottom";
export type StickyEdge = typeof STICKY_TOP | typeof STICKY_BOTTOM;

/** The bar that publishes each edge now: the one mounted last, so an older one leaving does not clear it. */
const owners = new Map<StickyEdge, object>();

/**
 * Publishes the height of `bar` (border box, whole pixels) as the variable of `edge`, following
 * its size (a composer that grows with its text or an alert above it), and removes it when the
 * bar unmounts. A layout effect, so the height is there before any effect of the same render
 * scrolls (a page that opens with a question running scrolls to it at once).
 */
export function useStickyHeight(bar: RefObject<HTMLElement | null>, edge: StickyEdge): void {
  useLayoutEffect(() => {
    const element = bar.current;
    if (element === null) return;
    const root = document.documentElement;
    const owner = {};
    owners.set(edge, owner);
    const publish = () => {
      if (owners.get(edge) !== owner) return;
      root.style.setProperty(edge, `${Math.ceil(element.getBoundingClientRect().height)}px`);
    };
    publish();
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(publish) : null;
    observer?.observe(element);
    return () => {
      observer?.disconnect();
      if (owners.get(edge) !== owner) return;
      owners.delete(edge);
      root.style.removeProperty(edge);
    };
  }, [bar, edge]);
}

/** The window's part between the sticky bars, in viewport coordinates: the page scroller's scroll-padding. */
export function visibleBand(): { top: number; bottom: number } {
  const root = document.documentElement;
  const style = getComputedStyle(root);
  const px = (value: string) => {
    const number = Number.parseFloat(value);
    return Number.isFinite(number) ? number : 0;
  };
  return { top: px(style.scrollPaddingTop), bottom: root.clientHeight - px(style.scrollPaddingBottom) };
}
