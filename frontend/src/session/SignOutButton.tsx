import { LogOutIcon } from "lucide-react";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { Button } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { useSignOut } from "./useSignOut.ts";

/** Signs out of this browser, for the screens where there is nothing else to do. */
export function SignOutButton() {
  const { t, errorMessage } = useI18n();
  const signOut = useSignOut();
  return (
    <>
      <Button variant="outline" disabled={signOut.isPending} onClick={() => signOut.mutate("this")}>
        <LogOutIcon />
        {t("page.action.sign_out")}
      </Button>
      {signOut.isError ? (
        <Alert variant="destructive" className="basis-full">
          <AlertDescription>{t("user_menu.sign_out_failed", { reason: errorMessage(signOut.error) })}</AlertDescription>
        </Alert>
      ) : null}
    </>
  );
}
