import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, render, screen } from "@testing-library/react";

import { fakeViewport } from "../test/fake-viewport.ts";
import { NEAR_VIEWPORT_MARGIN, useNearViewport } from "./use-near-viewport.ts";

afterEach(() => {
  cleanup();
});

function Probe() {
  const { ref, near } = useNearViewport();
  return (
    <p ref={ref} data-testid="probe">
      {near ? "near" : "far"}
    </p>
  );
}

describe("useNearViewport", () => {
  test("is far until the browser reports the element, then stays near", () => {
    const viewport = fakeViewport();
    try {
      render(<Probe />);
      expect(screen.getByTestId("probe").textContent).toBe("far");
      expect(viewport.margins).toEqual([NEAR_VIEWPORT_MARGIN]);
      viewport.enter();
      expect(screen.getByTestId("probe").textContent).toBe("near");
      // It stopped watching: scrolling away does not undo it.
      expect(viewport.observed()).toEqual([]);
    } finally {
      viewport.restore();
    }
  });

  test("is near at once where the browser has no IntersectionObserver", () => {
    const original = globalThis.IntersectionObserver;
    // @ts-expect-error simulating a browser without the API
    globalThis.IntersectionObserver = undefined;
    try {
      render(<Probe />);
      expect(screen.getByTestId("probe").textContent).toBe("near");
    } finally {
      globalThis.IntersectionObserver = original;
    }
  });
});
