import type { ReactNode } from "react";
import { Link } from "react-router";

import { focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";

/**
 * A page's header as docs/design/tenant-admin/design.md 「共用框架」 sets it: breadcrumbs on a
 * second-level page, the `h1`, one line of description and the page's actions at the end; on a
 * narrow screen the actions wrap under the title.
 */
export function PageHeader({
  title,
  description,
  crumbs,
  meta,
  actions,
}: {
  title: string;
  description?: ReactNode;
  /** The pages above this one, outermost first; the current page ends the trail. */
  crumbs?: readonly { label: string; to: string }[];
  /** A line under the description (a version, a state). */
  meta?: ReactNode;
  actions?: ReactNode;
}) {
  const { t } = useI18n();
  return (
    <header className="grid gap-3">
      {crumbs && crumbs.length > 0 ? (
        <nav aria-label={t("page.breadcrumbs")}>
          <ol className="flex flex-wrap items-center gap-1.5 text-sm text-muted-foreground">
            {crumbs.map((crumb) => (
              <li key={crumb.to} className="flex items-center gap-1.5">
                <Link
                  to={crumb.to}
                  className={cn(
                    "rounded-sm underline decoration-border underline-offset-3 hover:text-foreground",
                    focusRing,
                  )}
                >
                  {crumb.label}
                </Link>
                <span aria-hidden="true">/</span>
              </li>
            ))}
            <li aria-current="page" className="min-w-0 truncate">
              {title}
            </li>
          </ol>
        </nav>
      ) : null}
      <div className="flex flex-wrap items-end gap-x-6 gap-y-3">
        <div className="grid min-w-0 flex-[1_1_20rem] gap-1">
          <h1 className="text-2xl font-semibold tracking-tight wrap-anywhere">{title}</h1>
          {description ? <p className="text-sm text-muted-foreground text-pretty">{description}</p> : null}
          {meta ? <div className="text-sm text-muted-foreground">{meta}</div> : null}
        </div>
        {actions ? <div className="flex w-full flex-wrap gap-2 sm:w-auto">{actions}</div> : null}
      </div>
    </header>
  );
}
