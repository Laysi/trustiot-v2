import type { ReactNode } from "react";

/**
 * Why a part of the page has nothing to show and what to do next (checklist L3), in place of an
 * empty table or a blank chart. `centered` is the larger form for a whole card's worth of
 * nothing (the device page's empty history): an icon on a tile, the title and the text centered.
 */
export function EmptyState({
  icon,
  title,
  children,
  action,
  role = "status",
  centered = false,
}: {
  icon: ReactNode;
  title: string;
  children: ReactNode;
  action?: ReactNode;
  /** `alert` when the page could not read at all; the rest is a state, not a failure. Not used by `centered`. */
  role?: "status" | "alert";
  centered?: boolean;
}) {
  if (centered) {
    return (
      <div role="status" className="grid justify-items-center gap-2.5 px-5 py-9 text-center">
        <div className="flex size-11 items-center justify-center rounded-xl bg-muted text-muted-foreground [&_svg]:size-5.5">
          {icon}
        </div>
        <p className="font-semibold">{title}</p>
        <div className="grid max-w-[34em] gap-1 text-sm text-muted-foreground text-pretty">{children}</div>
        {action ? <div className="mt-1 flex flex-wrap justify-center gap-2">{action}</div> : null}
      </div>
    );
  }
  return (
    <div role={role} className="grid justify-items-start gap-2 rounded-lg border border-dashed px-4 py-5">
      <div className="flex items-center gap-2 font-medium [&_svg]:size-4 [&_svg]:shrink-0 [&_svg]:text-muted-foreground">
        {icon}
        <p>{title}</p>
      </div>
      <div className="grid gap-1 text-sm text-muted-foreground text-pretty">{children}</div>
      {action ? <div className="mt-1 flex flex-wrap gap-2">{action}</div> : null}
    </div>
  );
}
