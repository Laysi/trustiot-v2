import type { ReactNode } from "react";

/**
 * Why a part of the page has nothing to show and what to do next (checklist L3), in place of an
 * empty table or a blank chart.
 */
export function EmptyState({
  icon,
  title,
  children,
  action,
}: {
  icon: ReactNode;
  title: string;
  children: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div role="status" className="grid justify-items-start gap-2 rounded-lg border border-dashed px-4 py-5">
      <div className="flex items-center gap-2 font-medium [&_svg]:size-4 [&_svg]:shrink-0 [&_svg]:text-muted-foreground">
        {icon}
        <p>{title}</p>
      </div>
      <div className="grid gap-1 text-sm text-muted-foreground text-pretty">{children}</div>
      {action ? <div className="mt-1 flex flex-wrap gap-2">{action}</div> : null}
    </div>
  );
}
