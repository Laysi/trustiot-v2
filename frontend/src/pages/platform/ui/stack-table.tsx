import type { ComponentProps, ReactNode } from "react";

import { cn } from "../../../lib/utils.ts";

/**
 * A data table that becomes a stack of cards under 640px (docs/design/platform-admin/design.md,
 * 「各畫面的重點」): the header row is hidden, each row is a card, and each cell names its column
 * itself. The name is real text that only shows on a narrow screen (`Td`'s `label`), not a
 * `::before`, so a screen reader reads it too; on a wide screen the column header says it, and the
 * hidden label is not rendered into the tree. The frame is `relative` so the visually hidden
 * caption never sits outside the page.
 */
export function StackTable({
  caption,
  head,
  children,
  className,
}: {
  caption: string;
  head: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("relative w-full rounded-lg border bg-card text-card-foreground sm:overflow-x-auto", className)}>
      <table className="block w-full text-sm sm:table">
        <caption className="sr-only">{caption}</caption>
        <thead className="hidden bg-muted/60 text-left text-muted-foreground sm:table-header-group">
          <tr>{head}</tr>
        </thead>
        <tbody className="block sm:table-row-group">{children}</tbody>
      </table>
    </div>
  );
}

export function Th({ className, ...props }: ComponentProps<"th">) {
  return <th scope="col" className={cn("px-4 py-2.5 text-left font-medium whitespace-nowrap", className)} {...props} />;
}

export function Tr({ className, ...props }: ComponentProps<"tr">) {
  return (
    <tr
      className={cn("block border-t p-3 first:border-t-0 sm:table-row sm:p-0 sm:first:border-t", className)}
      {...props}
    />
  );
}

/** A cell; `label` is the column's name, shown on a narrow screen only. */
export function Td({
  label,
  className,
  children,
  ...props
}: Omit<ComponentProps<"td">, "children"> & { label: string; children?: ReactNode }) {
  return (
    <td
      className={cn(
        "flex items-baseline justify-between gap-3 py-1 sm:table-cell sm:px-4 sm:py-3 sm:align-top",
        className,
      )}
      {...props}
    >
      <span className="text-xs text-muted-foreground sm:hidden">{label}</span>
      <span className="min-w-0 text-right wrap-anywhere sm:block sm:text-left">{children}</span>
    </td>
  );
}
