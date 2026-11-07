// Which source a card's panel shows, and how it opens and closes (docs/design/chat-dashboards/
// design.md, 「元件與互動」 of the source panel): the dashboard components' cards and the custom
// page's card share it, so a panel opens, toggles, closes and gives focus back the same way
// everywhere (Laysi/trustiot-v2#1201).
import {
  createContext,
  type ReactNode,
  type RefObject,
  useCallback,
  useContext,
  useLayoutEffect,
  useRef,
  useState,
} from "react";

import { type SourceRef, sameSource } from "./model.ts";

const SourceStateContext = createContext<SourceRef | null>(null);

/** Tells the triggers inside a card which source its panel shows, for their `aria-expanded`. */
export function SourceStateProvider(props: { source: SourceRef | null; children: ReactNode }) {
  return <SourceStateContext.Provider value={props.source}>{props.children}</SourceStateContext.Provider>;
}

/**
 * Whether the panel is open on `source`: what a trigger for `source` puts in `aria-expanded`. A
 * trigger outside a card with a panel (none is open there) gets `false`.
 */
export function useSourceExpanded(): (source: SourceRef) => boolean {
  const shown = useContext(SourceStateContext);
  return (source) => shown !== null && sameSource(shown, source);
}

export interface SourcePanelState {
  /** What the panel shows, `null` when it is closed. */
  source: SourceRef | null;
  /**
   * Shows `next` in the panel. Pressing a trigger for the source the panel already shows closes
   * it instead (the same value or button again); a trigger for another source switches it.
   */
  open(next: SourceRef): void;
  /** Closes the panel and gives focus back to the value or button that opened it. */
  close(): void;
  /** Closes the panel without moving focus: what the card shows has been replaced. */
  reset(): void;
}

/**
 * The state of one card's source panel. `container` is the card's element: only an element inside
 * it, outside the panel, counts as what opened the panel.
 */
export function useSourcePanel(container: RefObject<HTMLElement | null>): SourcePanelState {
  const [source, setSource] = useState<SourceRef | null>(null);
  const shown = useRef(source);
  shown.current = source;
  // The value or button that opened the panel (it has focus when the keyboard pressed it, and when
  // the browser focuses a clicked button), and where focus goes back once the panel is closed.
  const opener = useRef<HTMLElement | null>(null);
  const returnTo = useRef<HTMLElement | null>(null);

  const open = useCallback(
    (next: SourceRef) => {
      const active = document.activeElement;
      // Only an element of this card outside the panel pressed the trigger. A click that does not
      // focus its button (Safari's) leaves focus elsewhere: on the page, the panel's heading or
      // another part of the page; then the earlier opener, if any, stays.
      const trigger =
        active instanceof HTMLElement &&
        container.current?.contains(active) === true &&
        active.closest("[data-source-panel]") === null
          ? active
          : null;
      const current = shown.current;
      if (current !== null && sameSource(current, next)) {
        // Pressed again: focus stays on the pressed trigger, or goes back to the opener when the
        // browser did not focus it.
        returnTo.current = trigger ?? opener.current;
        opener.current = null;
        setSource(null);
        return;
      }
      if (trigger !== null) opener.current = trigger;
      setSource(next);
    },
    [container],
  );
  const close = useCallback(() => {
    returnTo.current = opener.current;
    opener.current = null;
    setSource(null);
  }, []);
  const reset = useCallback(() => {
    opener.current = null;
    returnTo.current = null;
    setSource(null);
  }, []);
  // Runs once the panel is gone and the layout is back (a KPI's card gives back the whole row it
  // took for the panel, see `VersionItems`), so focusing scrolls to where the opener is now.
  useLayoutEffect(() => {
    if (source !== null) return;
    const target = returnTo.current;
    returnTo.current = null;
    if (target?.isConnected) target.focus();
  }, [source]);
  return { source, open, close, reset };
}
