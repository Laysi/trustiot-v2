import { ArrowLeftIcon, MenuIcon, PlusIcon } from "lucide-react";
import { Link, useLocation } from "react-router";

import { Button, focusRing } from "../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../components/ui/dropdown-menu.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { useMediaQuery } from "../lib/use-media-query.ts";
import { cn } from "../lib/utils.ts";
import { useSession } from "../session/SessionProvider.tsx";
import { TenantSwitcher } from "../session/TenantSwitcher.tsx";
import { AccountItems, AccountLabel, AccountMenu } from "./AccountMenu.tsx";
import { currentMenuItem, isSecondaryPage, type MenuItem, menuGroups } from "./menu.ts";

/**
 * The conversation home, which is "/" until the conversation pages exist (Laysi/trustiot-v2#149);
 * then the way back is the conversation the user came from, or the latest one (app-shell.md, 「原則」).
 */
const CONVERSATION_PATH = "/";

/** The widths from which the header has its wide form: Tailwind's `sm`. */
const WIDE = "(min-width: 640px)";

function Logo({ wide }: { wide: boolean }) {
  const { t } = useI18n();
  return (
    <Link
      to={CONVERSATION_PATH}
      aria-label={t("shell.logo")}
      className={cn("flex shrink-0 items-center gap-2 rounded-md text-sm font-semibold", focusRing)}
    >
      {/* The mark is the favicon, so it is one file (public/favicon.svg). */}
      <img src="/favicon.svg" alt="" className="size-8" />
      {wide ? <span aria-hidden="true">{t("app.name")}</span> : null}
    </Link>
  );
}

/** The way back to the conversation, on a secondary page: a text button, or the icon on a phone. */
function BackToConversation({ wide }: { wide: boolean }) {
  const { t } = useI18n();
  return wide ? (
    <Button asChild variant="outline" size="sm">
      <Link to={CONVERSATION_PATH}>
        <ArrowLeftIcon />
        {t("shell.back")}
      </Link>
    </Button>
  ) : (
    <Button asChild variant="ghost" size="icon">
      <Link to={CONVERSATION_PATH} aria-label={t("shell.back")} title={t("shell.back")}>
        <ArrowLeftIcon />
      </Link>
    </Button>
  );
}

function MenuLink({ item, current, wide }: { item: MenuItem; current: boolean; wide: boolean }) {
  const { t } = useI18n();
  const Icon = item.icon;
  return (
    <DropdownMenuItem
      asChild
      className={cn(
        !wide && "min-h-11",
        current &&
          "bg-accent font-semibold before:absolute before:inset-y-1 before:left-0 before:w-[3px] before:rounded-full before:bg-primary",
      )}
    >
      <Link to={item.path} aria-current={current ? "page" : undefined}>
        <Icon className={current ? "text-primary" : undefined} />
        {t(item.label)}
      </Link>
    </DropdownMenuItem>
  );
}

/**
 * The menu: every page a member may open and that exists (menu.ts), grouped; on a phone the
 * account is its last group, because the phone's top bar has no account button.
 */
function MainMenu({ wide, onSignOut, signingOut }: { wide: boolean; onSignOut: () => void; signingOut: boolean }) {
  const { t } = useI18n();
  const { session } = useSession();
  const { pathname } = useLocation();
  const current = currentMenuItem(pathname);
  const groups = menuGroups(session);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        {wide ? (
          <Button variant="ghost" size="sm">
            <MenuIcon />
            {t("shell.menu")}
          </Button>
        ) : (
          <Button variant="ghost" size="icon" aria-label={t("shell.menu")}>
            <MenuIcon />
          </Button>
        )}
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className={cn(
          wide ? "w-60" : "w-[min(20rem,calc(100vw-1rem))]",
          "max-h-(--radix-dropdown-menu-content-available-height)",
        )}
      >
        {groups.map((group, index) => (
          <div key={group[0]?.group} role="none">
            {index > 0 ? <DropdownMenuSeparator /> : null}
            {group.map((item) => (
              <MenuLink key={item.id} item={item} current={item.id === current} wide={wide} />
            ))}
          </div>
        ))}
        {wide ? null : (
          <div role="none">
            {groups.length > 0 ? <DropdownMenuSeparator /> : null}
            <AccountLabel />
            <AccountItems onSignOut={onSignOut} signingOut={signingOut} />
          </div>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/**
 * The top bar of every signed-in page (docs/design/app-shell.md): the logo, the organization, the
 * menu and the account, and nothing else; the pages are in the menu, not in a row of links. A
 * secondary page, one that is not a conversation, also has the way back to the conversation.
 * Wide screens and phones (639 px and below) have different forms, one line each.
 */
export function AppHeader({ onSignOut, signingOut }: { onSignOut: () => void; signingOut: boolean }) {
  const { t } = useI18n();
  const wide = useMediaQuery(WIDE);
  const { pathname } = useLocation();
  const secondary = isSecondaryPage(pathname);
  const menu = <MainMenu wide={wide} onSignOut={onSignOut} signingOut={signingOut} />;

  return (
    <header className="sticky top-0 z-40 border-b bg-background">
      {wide ? (
        <div className="flex h-14 items-center gap-3 px-4">
          <Logo wide />
          <TenantSwitcher />
          <div className="ml-auto flex items-center gap-2">
            {secondary ? <BackToConversation wide /> : null}
            {menu}
            <AccountMenu onSignOut={onSignOut} signingOut={signingOut} />
          </div>
        </div>
      ) : (
        // The ends of the bar are 8 px from the screen's edge so the icon buttons mirror each
        // other; a conversation page, which has no back button, starts with the logo at 16 px.
        <div className={cn("flex h-13 items-center gap-1 pr-2", secondary ? "pl-2" : "pl-4")}>
          {secondary ? <BackToConversation wide={false} /> : null}
          <Logo wide={false} />
          <TenantSwitcher compact />
          <Button asChild variant="ghost" size="icon">
            <Link to={CONVERSATION_PATH} aria-label={t("shell.new_conversation")} title={t("shell.new_conversation")}>
              <PlusIcon />
            </Link>
          </Button>
          {menu}
        </div>
      )}
    </header>
  );
}
