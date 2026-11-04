import { CircleUserRoundIcon, LogOutIcon, UserIcon } from "lucide-react";
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

/** Who is signed in: the email, and that the user is a platform administrator. */
export function AccountLabel() {
  const { t } = useI18n();
  const { session } = useSession();
  return (
    <DropdownMenuLabel className="grid gap-0.5 font-normal">
      <span className="text-xs text-muted-foreground">{t("user_menu.signed_in_as")}</span>
      <span className="truncate font-medium">{session?.user.email ?? t("settings.account.no_email")}</span>
      {session?.user.platformAdmin ? (
        <span className="text-xs text-muted-foreground">{t("user_menu.platform_admin")}</span>
      ) : null}
    </DropdownMenuLabel>
  );
}

/** The account's two entries, as the account menu and the phone's menu show them. */
export function AccountItems({ onSignOut, signingOut }: { onSignOut: () => void; signingOut: boolean }) {
  const { t } = useI18n();
  return (
    <>
      <DropdownMenuItem asChild>
        <Link to={SETTINGS_PATH}>
          <UserIcon />
          {t("user_menu.account_settings")}
        </Link>
      </DropdownMenuItem>
      <DropdownMenuItem disabled={signingOut} onSelect={onSignOut}>
        <LogOutIcon />
        {t("user_menu.sign_out")}
      </DropdownMenuItem>
    </>
  );
}

/** The account button at the end of the top bar on a wide screen; on a phone the menu holds these. */
export function AccountMenu({ onSignOut, signingOut }: { onSignOut: () => void; signingOut: boolean }) {
  const { t } = useI18n();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label={t("user_menu.label")}>
          <CircleUserRoundIcon className="size-5" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        <AccountLabel />
        <DropdownMenuSeparator />
        <AccountItems onSignOut={onSignOut} signingOut={signingOut} />
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
