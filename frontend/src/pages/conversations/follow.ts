/**
 * When the conversation page follows the answer being written (Laysi/trustiot-v2#1182): the
 * decisions only, from measurements the page takes (useFollowNewest.ts is the DOM side), so they
 * can be tested without a layout.
 *
 * `below` is how far the bottom of the question being answered is under the bottom of the window's
 * visible band (above the composer; negative when it ends above it).
 */

/** How far under the band's bottom the newest line may end while the reader still counts as at the bottom. */
export const AT_BOTTOM_PX = 32;

export interface Follow {
  /** The newest part is kept in view as it grows. */
  following: boolean;
  /** A finger is on the screen. */
  touching: boolean;
  /** The wheel or the touchpad moved the page and has not been quiet since. */
  wheeling: boolean;
}

export type FollowEvent =
  /** The page scrolled by `deltaY` (down is positive), by whatever means. */
  | { kind: "scroll"; deltaY: number; below: number }
  | { kind: "wheel" }
  | { kind: "wheel-quiet" }
  | { kind: "touch-start" }
  | { kind: "touch-end" };

/**
 * The state when a question's turn appears: the page has just brought it into view (sending it,
 * or opening a page where one runs), so the reader is at its bottom and it is followed.
 */
export const START: Follow = { following: true, touching: false, wheeling: false };

export function atBottom(below: number): boolean {
  return below <= AT_BOTTOM_PX;
}

/**
 * The state after `event`. At the bottom the page follows, however the reader got there; scrolling
 * up away from it stops following; scrolling down short of it (the page's own smooth scroll on its
 * way, or the reader heading back) leaves following as it was.
 */
export function nextFollow(state: Follow, event: FollowEvent): Follow {
  switch (event.kind) {
    case "scroll": {
      const following = atBottom(event.below) || (state.following && event.deltaY >= 0);
      return following === state.following ? state : { ...state, following };
    }
    case "wheel":
      return state.wheeling ? state : { ...state, wheeling: true };
    case "wheel-quiet":
      return state.wheeling ? { ...state, wheeling: false } : state;
    case "touch-start":
      return state.touching ? state : { ...state, touching: true };
    case "touch-end":
      return state.touching ? { ...state, touching: false } : state;
  }
}

/**
 * Whether to scroll the newest part into view now (it grew, the composer changed size, or the
 * reader stopped scrolling): only while following, never while the reader is scrolling, and only
 * when it ends under the band.
 */
export function scrollToNewest(state: Follow, below: number): boolean {
  return state.following && !state.touching && !state.wheeling && below > 0.5;
}

/** No smooth scrolling for a reader who asked for less motion (`prefers-reduced-motion: reduce`). */
export function followBehavior(reducedMotion: boolean): ScrollBehavior {
  return reducedMotion ? "instant" : "smooth";
}

export const REDUCED_MOTION = "(prefers-reduced-motion: reduce)";
