import { useEffect, useLayoutEffect, useRef } from "react";
import { useLocation, useNavigationType } from "react-router";

import { type HoldScrollEnvironment, holdScroll, windowScrollEnvironment } from "../lib/hold-scroll.ts";

/** Where the positions are kept for a reload of the tab (this tab only, gone with it). */
const STORAGE_KEY = "trustiot:scroll-positions";

/** The most history entries whose position is kept. */
const MAX_POSITIONS = 200;

function loadPositions(): Map<string, number> {
  try {
    const stored: unknown = JSON.parse(window.sessionStorage.getItem(STORAGE_KEY) ?? "[]");
    if (!Array.isArray(stored)) return new Map();
    const entries = stored.filter(
      (entry): entry is [string, number] =>
        Array.isArray(entry) && typeof entry[0] === "string" && typeof entry[1] === "number",
    );
    return new Map(entries);
  } catch {
    return new Map();
  }
}

function savePositions(positions: ReadonlyMap<string, number>): void {
  try {
    window.sessionStorage.setItem(STORAGE_KEY, JSON.stringify([...positions].slice(-MAX_POSITIONS)));
  } catch {
    // No storage (private window, quota): the positions then do not survive a reload.
  }
}

/**
 * Next to the router's `ScrollRestoration`: when the browser's back or forward button returns to a
 * page, keeps its scroll position while the page's content arrives (lib/hold-scroll.ts).
 * `ScrollRestoration` restores once, when the page is shown, which for a page that is still
 * loading is too early: the position is clamped to the short page and then moved by what grows
 * above it (Laysi/trustiot-v2#1185).
 *
 * The router keeps its saved positions to itself, so this records the position of each history
 * entry itself, from the page's scroll events: the last one before the page is left is where the
 * reader was. Positions are written to the tab's session storage when the page is hidden, so a
 * reload keeps them. While a position is held, the scroll events are the hold's own (the short
 * page clamps it) and are not recorded: the entry keeps the position the reader left it at, also
 * when they leave again before the page has grown. A link to a page never holds anything; it
 * starts at the top (`ScrollRestoration`).
 */
export function HoldBackScroll({ environment }: { environment?: HoldScrollEnvironment }) {
  const location = useLocation();
  const navigation = useNavigationType();
  const positions = useRef<Map<string, number> | null>(null);
  positions.current ??= loadPositions();
  const key = useRef(location.key);
  const holding = useRef(false);

  // Before the effects below: a scroll event after this commit belongs to the new entry.
  useLayoutEffect(() => {
    key.current = location.key;
  }, [location.key]);

  useEffect(() => {
    const record = () => {
      if (holding.current) return;
      const known = positions.current as Map<string, number>;
      // Set again so the entry counts as the newest when only the newest are kept.
      known.delete(key.current);
      known.set(key.current, Math.round(window.scrollY));
    };
    const save = () => savePositions(positions.current as Map<string, number>);
    window.addEventListener("scroll", record, { passive: true });
    window.addEventListener("pagehide", save);
    return () => {
      window.removeEventListener("scroll", record);
      window.removeEventListener("pagehide", save);
    };
  }, []);

  // A layout effect, after the router's restore (it renders first): the hold is decided and
  // started in the commit, before the browser reports that restore's clamp in a scroll event, which
  // a passive effect does not guarantee (the router commits in a transition).
  useLayoutEffect(() => {
    if (navigation !== "POP") return;
    const saved = positions.current?.get(location.key);
    if (saved === undefined || saved <= 0) return;
    holding.current = true;
    const stop = holdScroll(saved, environment ?? windowScrollEnvironment(), () => {
      holding.current = false;
    });
    return () => {
      stop();
      holding.current = false;
    };
  }, [location.key, navigation, environment]);

  return null;
}
