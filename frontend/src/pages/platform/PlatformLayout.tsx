import { LockIcon, ShieldIcon } from "lucide-react";
import { useId } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router";

import { Button, focusRing } from "../../components/ui/button.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { StatusPage } from "../StatusPage.tsx";
import { currentSection, platformSections } from "./sections.ts";

/**
 * The frame of every page of the platform back office (docs/design/platform-admin/design.md,
 * 「資訊架構」): a notice that what is done here reaches across tenants and is written to the
 * operation log, and the sections of the back office. A wide screen (from 1024px) shows the
 * sections as a row of links by group, the current one marked `aria-current="page"`; a narrow
 * one shows a native select (decision Q7). The notice and the sections are one region named
 * 「平台後台」. Only a platform administrator (the session's flag) sees the pages: the API refuses
 * everyone else with 403 anyway, so this only spares them a page of errors.
 *
 * It is a route element: the pages are its children (`Outlet`), including the AI pages that
 * plug in through `ai/plugin.ts`.
 */
export function PlatformLayout() {
  const { session } = useSession();
  if (!session?.user.platformAdmin) return <NoPermission />;
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-6">
      <SectionNav />
      <Outlet />
    </div>
  );
}

function NoPermission() {
  const { t } = useI18n();
  return (
    <StatusPage
      title={t("platform.forbidden.title")}
      icon={<LockIcon />}
      actions={
        <Button asChild variant="outline">
          <Link to="/">{t("page.action.home")}</Link>
        </Button>
      }
    >
      <p>{t("platform.forbidden.body")}</p>
    </StatusPage>
  );
}

function SectionNav() {
  const { t } = useI18n();
  const { pathname } = useLocation();
  const navigate = useNavigate();
  const selectId = useId();
  const groups = platformSections();
  const current = currentSection(pathname);
  return (
    <section aria-label={t("platform.region")} className="grid gap-3">
      <p className="flex items-center gap-2 rounded-md bg-brand-soft px-3 py-2 text-sm">
        <ShieldIcon aria-hidden="true" className="size-4 shrink-0 text-primary" />
        <span>
          <strong className="font-semibold">{t("platform.scope.strong")}</strong>
          {t("platform.scope.rest")}
        </span>
      </p>

      <nav aria-label={t("platform.sections")} className="hidden flex-wrap items-stretch gap-x-4 gap-y-2 lg:flex">
        {groups.map(({ group, sections }, index) => {
          const only = sections.length === 1 ? sections[0] : undefined;
          // A group of one section that is named like the group is not named twice.
          const showName = !(only && t(only.label) === t(group.label));
          return (
            <fieldset
              key={group.id}
              aria-label={t(group.label)}
              className={cn("m-0 flex min-w-0 items-center gap-2 border-0 p-0", index > 0 && "border-l pl-4")}
            >
              {showName ? <span className="text-xs font-medium text-muted-foreground">{t(group.label)}</span> : null}
              {sections.map((section) => (
                <Link
                  key={section.id}
                  to={section.path}
                  aria-current={current?.id === section.id ? "page" : undefined}
                  className={cn(
                    "rounded-md border-b-2 border-transparent px-3 py-1.5 text-sm hover:bg-accent",
                    current?.id === section.id && "border-primary bg-accent font-medium",
                    focusRing,
                  )}
                >
                  {t(section.label)}
                </Link>
              ))}
            </fieldset>
          );
        })}
      </nav>

      <div className="grid gap-1.5 lg:hidden">
        <label htmlFor={selectId} className="sr-only">
          {t("platform.sections")}
        </label>
        <NativeSelect
          id={selectId}
          value={current?.path ?? ""}
          onChange={(event) => {
            if (event.target.value !== "") void navigate(event.target.value);
          }}
        >
          {current === null ? <option value="" /> : null}
          {groups.map(({ group, sections }) =>
            sections.map((section) => (
              <option key={section.id} value={section.path}>
                {t(group.label) === t(section.label) ? t(section.label) : `${t(group.label)}: ${t(section.label)}`}
              </option>
            )),
          )}
        </NativeSelect>
      </div>
    </section>
  );
}
