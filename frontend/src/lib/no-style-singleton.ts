/**
 * Stands in for `react-style-singleton` (vite.config.ts aliases it here; src/test/setup.ts does
 * the same for the tests). Radix's modal components (Dialog, AlertDialog, DropdownMenu) lock the
 * page's scroll through `react-remove-scroll`, which writes its rules into a `<style>` element
 * through this package. The page's CSP allows no inline style, so the browser would refuse the
 * element and report a violation each time a menu or dialog opens.
 *
 * This version keeps the same exports and never touches the DOM. The scroll lock still works:
 * `react-remove-scroll-bar` marks the body with `data-scroll-locked` itself, and
 * src/styles/global.css holds the rule for that attribute. `react-remove-scroll` also uses each
 * returned component as the identity of a lock in its stack, so every call returns a new one.
 */
import type { FC } from "react";

interface StyleProps {
  styles: string;
  dynamic?: boolean;
}

export const styleSingleton = (): FC<StyleProps> =>
  function NoStyle() {
    return null;
  };

export const styleHookSingleton = (): ((styles: string, isDynamic?: boolean) => void) => () => {};

export const stylesheetSingleton = (): { add: (style: string) => void; remove: () => void } => ({
  add: () => {},
  remove: () => {},
});
