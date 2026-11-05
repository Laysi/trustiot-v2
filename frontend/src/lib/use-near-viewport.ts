import { useCallback, useEffect, useRef, useState } from "react";

/**
 * How far outside the window an element still counts as near it: about a screen, so what is
 * scrolled to has started loading before it arrives.
 */
export const NEAR_VIEWPORT_MARGIN = "600px 0px";

/**
 * Whether an element is, or has ever been, near the viewport. It stays `true` once it was: work
 * started for the element (a query) is not undone by scrolling away. Where the browser has no
 * `IntersectionObserver` the element counts as near at once, so nothing waits for an event that
 * never comes. Attach `ref` to the element.
 */
export function useNearViewport(): { ref: (element: Element | null) => void; near: boolean } {
  const [near, setNear] = useState(() => typeof IntersectionObserver !== "function");
  const observer = useRef<IntersectionObserver | null>(null);
  const ref = useCallback((element: Element | null) => {
    observer.current?.disconnect();
    observer.current = null;
    if (element === null || typeof IntersectionObserver !== "function") return;
    const watching = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          setNear(true);
          watching.disconnect();
        }
      },
      { rootMargin: NEAR_VIEWPORT_MARGIN },
    );
    watching.observe(element);
    observer.current = watching;
  }, []);
  useEffect(() => () => observer.current?.disconnect(), []);
  return { ref, near };
}
