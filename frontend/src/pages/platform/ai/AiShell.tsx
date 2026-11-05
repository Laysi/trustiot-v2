import { ShieldCheckIcon } from "lucide-react";
import { Link, Outlet, useLocation } from "react-router";

import { focusRing } from "../../../components/ui/button.tsx";
import type { MessageId } from "../../../i18n/messages.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { useSession } from "../../../session/SessionProvider.tsx";
import { ForbiddenPage } from "../../pages.tsx";
import { MODEL_ROLES_PATH, PROMPTS_PATH, USAGE_PATH } from "./paths.ts";

interface Section {
  path: string;
  label: MessageId;
}

/**
 * The AI sections of the platform administration that can be opened. Only the model roles have
 * an API today; the pages whose API is pending add their sections when the preview loads them
 * (`preview.ts`), so the production build lists nothing that is not there.
 */
export const AI_SECTIONS: Section[] = [{ path: MODEL_ROLES_PATH, label: "platform_ai.section.model_roles" }];

export const PENDING_AI_SECTIONS: Section[] = [
  { path: PROMPTS_PATH, label: "platform_ai.section.prompts" },
  { path: USAGE_PATH, label: "platform_ai.section.usage" },
];

/**
 * The frame of the platform AI pages until the platform administration's own layout
 * (Laysi/trustiot-v2#132) is on main: the strip that says every action works across tenants, the
 * AI sections, and the page. Only a platform administrator gets the page; anyone else sees the
 * no-permission page (the API refuses them with 403 anyway).
 */
export function AiShell() {
  const { t } = useI18n();
  const { session } = useSession();
  const { pathname } = useLocation();
  if (session !== null && !session.user.platformAdmin) {
    return <ForbiddenPage />;
  }
  return (
    <div className="grid gap-6">
      <div role="region" aria-label={t("platform_ai.region_label")} className="grid gap-3">
        <p className="flex items-center gap-2 rounded-md border border-brand/40 bg-brand-soft px-3 py-2 text-sm">
          <ShieldCheckIcon aria-hidden="true" className="size-4 shrink-0" />
          <span>
            <strong>{t("platform_ai.scope_title")}</strong>
            {t("platform_ai.scope_body")}
          </span>
        </p>
        <nav aria-label={t("platform_ai.sections_label")} className="flex flex-wrap items-center gap-x-1 gap-y-1">
          <span aria-hidden="true" className="px-2 text-xs font-medium text-muted-foreground">
            {t("platform_ai.group")}
          </span>
          <div role="group" aria-label={t("platform_ai.group")} className="flex flex-wrap gap-1">
            {AI_SECTIONS.map((section) => {
              const current = pathname === section.path || pathname.startsWith(`${section.path}/`);
              return (
                <Link
                  key={section.path}
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
          </div>
        </nav>
      </div>
      <Outlet />
    </div>
  );
}
