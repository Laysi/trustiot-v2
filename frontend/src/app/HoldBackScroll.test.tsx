import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, render, screen } from "@testing-library/react";
import { useLayoutEffect } from "react";
import { createMemoryRouter, Link, Outlet, useNavigationType } from "react-router";
import { RouterProvider } from "react-router/dom";

import type { HoldScrollEnvironment } from "../lib/hold-scroll.ts";
import { HoldBackScroll } from "./HoldBackScroll.tsx";

afterEach(() => {
  cleanup();
});

let scrollY = 0;
const original = Object.getOwnPropertyDescriptor(window, "scrollY");
beforeEach(() => {
  scrollY = 0;
  Object.defineProperty(window, "scrollY", { configurable: true, get: () => scrollY });
});
afterEach(() => {
  if (original) Object.defineProperty(window, "scrollY", original);
  else Reflect.deleteProperty(window, "scrollY");
});

/** The reader scrolls the page to `to`. */
function scrollTo(to: number) {
  scrollY = to;
  act(() => {
    window.dispatchEvent(new Event("scroll"));
  });
}

/** An environment that only records what is held and where; the page never grows. */
function recordingEnvironment() {
  const held: number[] = [];
  const env: HoldScrollEnvironment = {
    scrollY: () => 0,
    scrollTo: (top) => held.push(top),
    onResize: () => () => {},
    onUserInput: () => () => {},
    later: () => () => {},
  };
  return { env, held };
}

/**
 * The first page. With `clampedTo`, it behaves like a page that is still short when the router
 * restores the position: in its layout effect, on a back navigation, the browser has clamped the
 * position and says so with a scroll event, before any passive effect has run.
 */
function FirstPage({ clampedTo }: { clampedTo: number | undefined }) {
  const navigation = useNavigationType();
  useLayoutEffect(() => {
    if (clampedTo === undefined || navigation !== "POP") return;
    scrollY = clampedTo;
    window.dispatchEvent(new Event("scroll"));
  }, [clampedTo, navigation]);
  return <Link to="/next">to next</Link>;
}

function setUp(options: { clampedTo?: number } = {}) {
  const { env, held } = recordingEnvironment();
  const router = createMemoryRouter(
    [
      {
        path: "/",
        element: (
          <>
            <HoldBackScroll environment={env} />
            <Outlet />
          </>
        ),
        children: [
          { index: true, element: <FirstPage clampedTo={options.clampedTo} /> },
          { path: "next", element: <p>the next page</p> },
        ],
      },
    ],
    { initialEntries: ["/"] },
  );
  render(<RouterProvider router={router} />);
  return { router, held };
}

describe("HoldBackScroll", () => {
  beforeEach(() => {
    window.sessionStorage.clear();
  });

  test("an entry left again before its page has grown keeps the position the reader left it at", async () => {
    const { router, held } = setUp();
    await screen.findByText("to next");
    scrollTo(225);
    await act(() => router.navigate("/next"));
    await screen.findByText("the next page");
    await act(() => router.navigate(-1));
    await screen.findByText("to next");
    expect(held).toEqual([225]);

    // The short page clamps the restored position; that is not where the reader left it.
    scrollTo(100);
    await act(() => router.navigate("/next"));
    await screen.findByText("the next page");
    await act(() => router.navigate(-1));
    await screen.findByText("to next");
    expect(held).toEqual([225, 225]);
  });

  test("a clamp the browser reports before the passive effects run is neither held nor recorded", async () => {
    const { router, held } = setUp({ clampedTo: 100 });
    await screen.findByText("to next");
    scrollTo(225);
    await act(() => router.navigate("/next"));
    await screen.findByText("the next page");
    await act(() => router.navigate(-1));
    await screen.findByText("to next");
    // The hold is for where the reader left the page, not for the clamped position.
    expect(held).toEqual([225]);

    // And the entry still has the position the reader left, when it is left again before the page grew.
    await act(() => router.navigate("/next"));
    await screen.findByText("the next page");
    await act(() => router.navigate(-1));
    await screen.findByText("to next");
    expect(held).toEqual([225, 225]);
  });

  test("a reload keeps the positions: the entry is held from the tab's session storage", async () => {
    window.sessionStorage.setItem("trustiot:scroll-positions", JSON.stringify([["default", 300]]));
    const { held } = setUp();
    await screen.findByText("to next");
    expect(held).toEqual([300]);
  });

  test("positions that cannot be read are ignored", async () => {
    window.sessionStorage.setItem("trustiot:scroll-positions", "{not json");
    const { held } = setUp();
    await screen.findByText("to next");
    expect(held).toEqual([]);
  });

  test("holds the position the page had when it was left, after going back, and not for a link", async () => {
    const { router, held } = setUp();
    await screen.findByText("to next");
    scrollTo(225);

    await act(() => router.navigate("/next"));
    await screen.findByText("the next page");
    // The new page's own scroll events belong to it, not to the page that was left.
    scrollTo(0);
    expect(held).toEqual([]);

    await act(() => router.navigate(-1));
    await screen.findByText("to next");
    expect(held).toEqual([225]);
  });

  test("holds nothing for a page that was left at the top", async () => {
    const { router, held } = setUp();
    await screen.findByText("to next");
    await act(() => router.navigate("/next"));
    await screen.findByText("the next page");
    await act(() => router.navigate(-1));
    await screen.findByText("to next");
    expect(held).toEqual([]);
  });
});
