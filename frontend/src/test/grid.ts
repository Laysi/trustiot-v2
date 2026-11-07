// The grid of a version's components (`pages/dashboards/VersionItems.tsx`) in tests. happy-dom
// computes no layout, so whether an item takes the whole row is read from the class that does it
// in a browser and the condition the class's selector names (happy-dom matches `:has()`).

/** The class a one-cell item (a KPI or a status) has: the whole row while its source panel is open. */
export const ROW_WHILE_SOURCE_OPEN = "has-[[data-source-panel]]:col-span-full";

/** The grid item that holds the component card `card` (a region named by its title). */
export function itemOf(card: HTMLElement): HTMLElement {
  const item = card.parentElement;
  if (item === null) throw new Error("the component card is not in a grid item");
  return item;
}

/** Whether the one-cell item `item` takes the whole row now: it has the class and an open panel. */
export function spansRow(item: HTMLElement): boolean {
  return item.classList.contains(ROW_WHILE_SOURCE_OPEN) && item.matches(":has([data-source-panel])");
}
