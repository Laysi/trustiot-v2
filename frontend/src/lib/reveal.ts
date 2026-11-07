import { visibleBand } from "./sticky-bars.ts";

export const REDUCED_MOTION = "(prefers-reduced-motion: reduce)";

/** No smooth scrolling for a reader who asked for less motion (`prefers-reduced-motion: reduce`). */
export function scrollBehavior(reducedMotion: boolean): ScrollBehavior {
  return reducedMotion ? "instant" : "smooth";
}

/** Whether the reader asked for less motion now; `false` where there is no `matchMedia`. */
export function prefersReducedMotion(): boolean {
  return typeof window.matchMedia === "function" && window.matchMedia(REDUCED_MOTION).matches;
}

/**
 * Scrolls `element` into the window's band between the sticky bars (`visibleBand()`, the page
 * scroller's scroll-padding; Laysi/trustiot-v2#1184), and does nothing when all of it is already
 * there. An element that fits the band is brought in with the least scrolling (the edge that is
 * out lines up with the band's edge); one taller than the band gets its top at the band's top, so
 * the reader starts at its beginning. Smooth, unless the reader asked for less motion.
 */
export function revealInBand(element: HTMLElement): void {
  const band = visibleBand();
  const rect = element.getBoundingClientRect();
  if (rect.top >= band.top && rect.bottom <= band.bottom) return;
  const fits = rect.height <= band.bottom - band.top;
  element.scrollIntoView?.({ block: fits ? "nearest" : "start", behavior: scrollBehavior(prefersReducedMotion()) });
}
