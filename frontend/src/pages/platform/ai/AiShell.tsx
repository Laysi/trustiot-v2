import { ShieldCheckIcon } from "lucide-react";
import { Link, Outlet, useLocation } from "react-router";

import { focusRing } from "../../../components/ui/button.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { useSession } from "../../../session/SessionProvider.tsx";
import { ForbiddenPage } from "../../pages.tsx";
import { availableAiSections } from "./plugin.ts";

/**
 * The frame of the platform AI pages until the platform administration's own layout
 * (Laysi/trustiot-v2#132, whose navigation reads `aiSections` in `plugin.ts`) is on main: the
 * strip that says every action works across tenants, the AI sections, and the page. Only a
 * platform administrator gets the page; anyone else sees the no-permission page (the API refuses
 * them with 403 anyway). Remove it when the layout takes over.
 */
export function AiShell() {
  const { t } = useI18n();
  const { session } = useSession();
  const { pathname } = useLocation();
  if (session !== null && !session.user.platformAdmin) {
    return <ForbiddenPage />;
  }
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-6">
      <section aria-label={t("platform_ai.region_label")} className="grid gap-3">
        <p className="flex items-center gap-2 rounded-md border border-brand/40 bg-brand-soft px-3 py-2 text-sm">
          <ShieldCheckIcon aria-hidden="true" className="size-4 shrink-0" />
          <span>
            <strong>{t("platform_ai.scope_title")}</strong>
            {t("platform_ai.scope_body")}
          </span>
        </p>
        <nav aria-label={t("platform_ai.sections_label")} className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span aria-hidden="true" className="px-1 text-xs font-medium text-muted-foreground">
            {t("platform_ai.group")}
          </span>
          {availableAiSections().map((section) => {
            const current = section.covers(pathname);
            return (
              <Link
                key={section.id}
                to={section.path}
                {...(current ? { "aria-current": "page" as const } : {})}
                className={cn(
                  "rounded-md px-3 py-1.5 text-sm font-medium",
                  focusRing,
                  current
                    ? "border-b-2 border-primary bg-accent text-foreground"
                    : "text-muted-foreground hover:bg-accent hover:text-foreground",
                )}
              >
                {t(section.label)}
              </Link>
            );
          })}
        </nav>
      </section>
      <Outlet />
    </div>
  );
}
