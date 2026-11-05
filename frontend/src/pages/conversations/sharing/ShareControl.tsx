import { ShareIcon, UsersIcon } from "lucide-react";
import { useState } from "react";

import { Alert, AlertDescription } from "../../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "../../../components/ui/alert-dialog.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { useSession } from "../../../session/SessionProvider.tsx";
import { useShareChange } from "./queries.ts";

/**
 * The owner's way to share a conversation with the whole tenant, or to stop (docs/design/
 * chat-dashboards/design.md 「分享給整個租戶」): a button in the conversation's head that says
 * whether it is shared, and a confirmation either way. Sharing says who will see the answers
 * and that they may hold data other members cannot see; both say that stopping, or deleting,
 * does not take back the copies. `sharedAt` is the conversation's `shared_at`; `onDone` gets
 * what was done, for the page's notice.
 */
export function ShareControl({
  conversationId,
  sharedAt,
  onDone,
}: {
  conversationId: string;
  sharedAt: string | null | undefined;
  onDone(text: string): void;
}) {
  const { t, formatDateTime, errorMessage } = useI18n();
  const tenant = useSession().session?.activeTenant?.name ?? "";
  const change = useShareChange(conversationId);
  const [open, setOpen] = useState(false);
  const shared = sharedAt != null;

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (change.isPending) return;
        if (next) change.reset();
        setOpen(next);
      }}
    >
      <AlertDialogTrigger asChild>
        <Button variant="outline" size="sm" aria-haspopup="dialog">
          {shared ? <UsersIcon /> : <ShareIcon />}
          {t(shared ? "share.button_shared" : "share.button")}
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t(shared ? "share.stop.title" : "share.start.title", { tenant })}</AlertDialogTitle>
          <AlertDialogDescription>
            {shared
              ? t("share.stop.body", { tenant, time: formatDateTime(sharedAt) })
              : t("share.start.body", { tenant })}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <Alert role="note">
          <AlertDescription>
            <p className="text-foreground">{t(shared ? "share.stop.warning" : "share.start.warning")}</p>
          </AlertDescription>
        </Alert>
        {change.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{t("share.failed", { reason: errorMessage(change.error) })}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={change.isPending}>{t("share.cancel")}</AlertDialogCancel>
          <Button
            variant={shared ? "destructive" : "default"}
            disabled={change.isPending}
            onClick={() =>
              change.mutate(!shared, {
                onSuccess: () => {
                  setOpen(false);
                  onDone(t(shared ? "share.message.stopped" : "share.message.shared", { tenant }));
                },
              })
            }
          >
            {change.isPending ? <Spinner /> : null}
            {change.isPending ? t("share.pending") : t(shared ? "share.stop.confirm" : "share.start.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
