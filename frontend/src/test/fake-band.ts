/**
 * A window for the tests that scroll something into view (Laysi/trustiot-v2#1184): happy-dom has no
 * layout, so this gives the page a band between its sticky bars (the root element's
 * `scroll-padding`, which `visibleBand()` reads), gives every source panel a box, and records
 * `scrollIntoView` and `focus` calls instead of doing them.
 */
export interface FakeBand {
  /** Every `scrollIntoView` since the last `clear`, with the element it was called on. */
  scrolls: { element: Element; options: ScrollIntoViewOptions | boolean | undefined }[];
  /** Every `focus` since the last `clear`, with the element and the options it was called with. */
  focuses: { element: Element; options: FocusOptions | undefined }[];
  /** Moves the panels: their boxes, in viewport coordinates. */
  placePanel(box: { top: number; height: number }): void;
  clear(): void;
  restore(): void;
}

export function fakeBand(options: {
  /** The window's height and the heights of the top bar and of the composer (0 on the dashboard page). */
  viewport?: number;
  top?: number;
  bottom?: number;
  reducedMotion?: boolean;
}): FakeBand {
  const viewport = options.viewport ?? 720;
  const root = document.documentElement;
  const original = {
    scroll: Element.prototype.scrollIntoView,
    focus: HTMLElement.prototype.focus,
    rect: Element.prototype.getBoundingClientRect,
    media: window.matchMedia,
    clientHeight: Object.getOwnPropertyDescriptor(root, "clientHeight"),
    paddingTop: root.style.getPropertyValue("scroll-padding-top"),
    paddingBottom: root.style.getPropertyValue("scroll-padding-bottom"),
  };
  const scrolls: FakeBand["scrolls"] = [];
  const focuses: FakeBand["focuses"] = [];
  let panelBox = { top: 0, height: 0 };

  Element.prototype.scrollIntoView = function (this: Element, scrollOptions?: ScrollIntoViewOptions | boolean) {
    scrolls.push({ element: this, options: scrollOptions });
  };
  HTMLElement.prototype.focus = function (this: HTMLElement, focusOptions?: FocusOptions) {
    focuses.push({ element: this, options: focusOptions });
    original.focus.call(this, focusOptions);
  };
  Element.prototype.getBoundingClientRect = function (this: Element) {
    if (this.hasAttribute("data-source-panel")) {
      const { top, height } = panelBox;
      return { top, bottom: top + height, height, left: 0, right: 0, width: 0, x: 0, y: top, toJSON: () => ({}) };
    }
    return original.rect.call(this);
  };
  window.matchMedia = ((query: string) => ({
    matches: (options.reducedMotion ?? false) && query === "(prefers-reduced-motion: reduce)",
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  })) as unknown as typeof window.matchMedia;
  Object.defineProperty(root, "clientHeight", { configurable: true, value: viewport });
  root.style.setProperty("scroll-padding-top", `${options.top ?? 0}px`);
  root.style.setProperty("scroll-padding-bottom", `${options.bottom ?? 0}px`);

  return {
    scrolls,
    focuses,
    placePanel(box) {
      panelBox = box;
    },
    clear() {
      scrolls.length = 0;
      focuses.length = 0;
    },
    restore() {
      Element.prototype.scrollIntoView = original.scroll;
      HTMLElement.prototype.focus = original.focus;
      Element.prototype.getBoundingClientRect = original.rect;
      window.matchMedia = original.media;
      if (original.clientHeight) Object.defineProperty(root, "clientHeight", original.clientHeight);
      else Reflect.deleteProperty(root, "clientHeight");
      root.style.setProperty("scroll-padding-top", original.paddingTop);
      root.style.setProperty("scroll-padding-bottom", original.paddingBottom);
    },
  };
}
