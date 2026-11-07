import { describe, expect, test } from "bun:test";

import {
  AT_BOTTOM_PX,
  type Follow,
  type FollowEvent,
  followBehavior,
  nextFollow,
  START,
  scrollToNewest,
} from "./follow.ts";

/** The state after `events`, from the start of a question's turn. */
function after(...events: FollowEvent[]): Follow {
  return events.reduce(nextFollow, START);
}

/** The answer grew 120 px under the band: what the page does about it in `state`. */
const GREW = 120;

describe("following the answer being written", () => {
  test("a turn that appears is followed: the page has just scrolled it into view", () => {
    expect(START.following).toBe(true);
    expect(scrollToNewest(START, GREW)).toBe(true);
  });

  test("at the bottom it follows, including within the small threshold above the composer", () => {
    expect(after({ kind: "scroll", deltaY: 0, below: 0 }).following).toBe(true);
    expect(after({ kind: "scroll", deltaY: -AT_BOTTOM_PX, below: AT_BOTTOM_PX }).following).toBe(true);
    expect(scrollToNewest(after({ kind: "scroll", deltaY: -10, below: 10 }), GREW)).toBe(true);
  });

  test("scrolled up away from the bottom it stops following, and the answer growing does not pull it back", () => {
    const up = after({ kind: "scroll", deltaY: -400, below: 400 });
    expect(up.following).toBe(false);
    expect(scrollToNewest(up, 400 + GREW)).toBe(false);
    // Scrolling down short of the bottom does not start following again.
    const partWay = nextFollow(up, { kind: "scroll", deltaY: 200, below: 200 });
    expect(partWay.following).toBe(false);
    expect(scrollToNewest(partWay, 200 + GREW)).toBe(false);
  });

  test("back at the bottom it follows again, however the reader got there", () => {
    const up = after({ kind: "scroll", deltaY: -400, below: 400 });
    const back = nextFollow(up, { kind: "scroll", deltaY: 400, below: 0 });
    expect(back.following).toBe(true);
    expect(scrollToNewest(back, GREW)).toBe(true);
    // A jump to the end (the End key, a scrollbar drag) counts the same.
    expect(nextFollow(up, { kind: "scroll", deltaY: 9000, below: -40 }).following).toBe(true);
  });

  test("the page's own smooth scroll on its way down keeps following", () => {
    const state = after({ kind: "scroll", deltaY: 30, below: 300 }, { kind: "scroll", deltaY: 60, below: 240 });
    expect(state.following).toBe(true);
    expect(scrollToNewest(state, 240)).toBe(true);
  });

  test("never scrolls while the reader is scrolling: a finger on the screen or a wheel that has not stopped", () => {
    const touching = after({ kind: "touch-start" });
    expect(touching.following).toBe(true);
    expect(scrollToNewest(touching, GREW)).toBe(false);
    expect(scrollToNewest(nextFollow(touching, { kind: "touch-end" }), GREW)).toBe(true);

    const wheeling = after({ kind: "wheel" }, { kind: "scroll", deltaY: 10, below: GREW });
    expect(scrollToNewest(wheeling, GREW)).toBe(false);
    expect(scrollToNewest(nextFollow(wheeling, { kind: "wheel-quiet" }), GREW)).toBe(true);
  });

  test("nothing to scroll when the newest part already ends above the composer", () => {
    expect(scrollToNewest(START, 0)).toBe(false);
    expect(scrollToNewest(START, -200)).toBe(false);
  });

  test("an event that changes nothing keeps the same state", () => {
    expect(nextFollow(START, { kind: "scroll", deltaY: 5, below: 0 })).toBe(START);
    expect(nextFollow(START, { kind: "touch-end" })).toBe(START);
    expect(nextFollow(START, { kind: "wheel-quiet" })).toBe(START);
  });
});

describe("how the page scrolls", () => {
  test("smoothly, unless the reader asked for less motion", () => {
    expect(followBehavior(false)).toBe("smooth");
    expect(followBehavior(true)).toBe("instant");
  });
});
