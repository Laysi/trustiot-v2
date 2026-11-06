import type { ReactNode } from "react";

/** 「會發生什麼」: the concrete consequences of an action, with numbers. */
export function ImpactList({ title, items }: { title: string; items: readonly ReactNode[] }) {
  return (
    <div className="grid gap-1.5 rounded-md bg-muted px-3 py-2.5 text-sm">
      <p className="font-medium">{title}</p>
      <ul className="grid list-disc gap-1 pl-5 text-muted-foreground marker:text-muted-foreground">
        {items.map((item, index) => (
          // The list is fixed for a dialog; nothing reorders it.
          // biome-ignore lint/suspicious/noArrayIndexKey: a static list of consequences
          <li key={index}>{item}</li>
        ))}
      </ul>
    </div>
  );
}
