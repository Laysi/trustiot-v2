import { LogOutIcon, SettingsIcon } from "lucide-react";
import { Link } from "react-router";

import { Button } from "../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../components/ui/dropdown-menu.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { useSession } from "../session/SessionProvider.tsx";
import { SETTINGS_PATH } from "./paths.ts";

/** The account menu at the end of the top bar: who is signed in, their settings, and signing out. */
export function UserMenu({ onSignOut, signingOut }: { onSignOut: () => void; signingOut: boolean }) {
  const { t } = useI18n();
  const { session } = useSession();
  const email = session?.user.email ?? null;
  const initial = (email?.trim()[0] ?? "?").toUpperCase();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" className="rounded-full" aria-label={t("user_menu.label")}>
          <span className="flex size-8 items-center justify-center rounded-full bg-primary text-sm font-semibold text-primary-foreground">
            {initial}
          </span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        <DropdownMenuLabel className="grid gap-0.5 font-normal">
          <span className="text-xs text-muted-foreground">{t("user_menu.signed_in_as")}</span>
          <span className="truncate font-medium">{email ?? t("settings.account.no_email")}</span>
          {session?.user.platformAdmin ? (
            <span className="text-xs text-muted-foreground">{t("user_menu.platform_admin")}</span>
          ) : null}
        </DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuItem asChild>
          <Link to={SETTINGS_PATH}>
            <SettingsIcon />
            {t("user_menu.settings")}
          </Link>
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem disabled={signingOut} onSelect={onSignOut}>
          <LogOutIcon />
          {t("user_menu.sign_out")}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
