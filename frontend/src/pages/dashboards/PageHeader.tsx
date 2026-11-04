import { ChevronRightIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router";

import { focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";

/**
 * The head of a page below the top bar, as docs/design/chat-dashboards/ draws it: the trail of
 * pages above this one, the `h1` with its state tags, a line of facts under it, and the page's
 * actions at the end (they wrap under the title on a narrow screen).
 */
export function PageHeader({
  title,
  tags,
  meta,
  crumbs,
  actions,
}: {
  title: string;
  /** State tags next to the title. */
  tags?: ReactNode;
  /** A line under the title (a description, the time of the data). */
  meta?: ReactNode;
  /** The pages above this one, outermost first; the current page ends the trail. */
  crumbs?: readonly { label: string; to: string }[];
  actions?: ReactNode;
}) {
  const { t } = useI18n();
  return (
    <header className="grid min-w-0 grid-cols-[minmax(0,1fr)] gap-3">
      {crumbs && crumbs.length > 0 ? (
        <nav aria-label={t("page.breadcrumbs")}>
          <ol className="flex flex-wrap items-center gap-1 text-sm text-muted-foreground">
            {crumbs.map((crumb) => (
              <li key={crumb.to} className="flex items-center gap-1">
                <Link
                  to={crumb.to}
                  className={cn("rounded-sm underline-offset-4 hover:text-foreground hover:underline", focusRing)}
                >
                  {crumb.label}
                </Link>
                <ChevronRightIcon aria-hidden="true" className="size-3.5" />
              </li>
            ))}
            <li aria-current="page" className="min-w-0 max-w-full truncate text-foreground">
              {title}
            </li>
          </ol>
        </nav>
      ) : null}
      <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
        <div className="grid min-w-0 flex-[1_1_20rem] gap-1.5">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <h1 className="min-w-0 text-2xl font-semibold tracking-tight wrap-anywhere">{title}</h1>
            {tags}
          </div>
          {meta ? <div className="text-sm text-muted-foreground">{meta}</div> : null}
        </div>
        {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
      </div>
    </header>
  );
}
