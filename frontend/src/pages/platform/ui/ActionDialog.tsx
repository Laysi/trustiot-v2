import { TriangleAlertIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";

import { ApiError } from "../../../api/errors.ts";
import type { TenantStateSnapshot } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../../components/ui/alert-dialog.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { StaleStateError } from "../errors.ts";
import { AuditNote } from "./audit-note.tsx";

/**
 * Whether a failed write may have gone through: no answer came, or the server timed out or failed
 * in the middle of it (500, 504). A 503 and every 4xx say nothing was written.
 */
export function outcomeUnknown(error: unknown): boolean {
  if (!(error instanceof ApiError)) return true;
  return error.status === 0 || error.status === 504 || error.status === 500 || error.body?.code === "timeout";
}

/**
 * The confirmation of a high-risk action of the back office (docs/design/platform-admin/
 * design.md, 「高風險操作的確認分級」 and 「互動與狀態的共同規則」): what will happen, the values
 * before and after, and what goes to the operation log, in an alert dialog whose focus starts on
 * Cancel. The button names the action and the target. While the request runs the dialog cannot
 * be closed and the button is disabled, so a second click cannot send it twice. A refusal is
 * shown inside the dialog, which stays open, and says whether anything changed; a state that
 * changed under the dialog (`StaleStateError`) stops it with a way to look at the new state. The
 * page is updated from the server's answer after `run` resolves (`onDone`), never optimistically.
 */
export function ActionDialog(props: {
  open: boolean;
  onOpenChange(open: boolean): void;
  title: ReactNode;
  description?: ReactNode;
  children?: ReactNode;
  /** What the operation log will hold, 「將記入操作紀錄」. */
  audit?: ReactNode;
  confirmLabel: string;
  pendingLabel: string;
  tone?: "default" | "destructive";
  /** The typed confirmation or checkbox is not complete yet. */
  confirmDisabled?: boolean;
  /** Sends the change; rejects with the error to show. */
  run(): Promise<unknown>;
  /** The change was made: the dialog is closed, the page refreshes and says what happened. */
  onDone(): void;
  /** The text of a refusal, when the page knows better than the error code (a rejected field). */
  describeError?(error: unknown): string | undefined;
  /** The person asked to look at the state that changed under the dialog. */
  onStale?(current: TenantStateSnapshot): void;
  /** Where focus goes when the dialog closes, when it was not opened by a trigger inside it. */
  returnFocus?: () => HTMLElement | null;
  className?: string;
}) {
  const { t, errorMessage } = useI18n();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  // A dialog opened again starts clean.
  const { open } = props;
  useEffect(() => {
    if (open) setError(null);
  }, [open]);

  const submit = async () => {
    setPending(true);
    setError(null);
    try {
      await props.run();
    } catch (cause) {
      if (mounted.current) {
        setError(cause);
        setPending(false);
      }
      return;
    }
    if (mounted.current) setPending(false);
    props.onDone();
  };

  const stale = error instanceof StaleStateError ? error : null;
  return (
    <AlertDialog
      open={props.open}
      onOpenChange={(next) => {
        if (pending) return;
        props.onOpenChange(next);
      }}
    >
      <AlertDialogContent
        className={cn("max-h-[90dvh] overflow-y-auto", props.className)}
        onCloseAutoFocus={(event) => {
          const target = props.returnFocus?.();
          if (target) {
            event.preventDefault();
            target.focus();
          }
        }}
      >
        <AlertDialogHeader>
          <AlertDialogTitle className="wrap-anywhere">{props.title}</AlertDialogTitle>
          <AlertDialogDescription>
            {props.description ?? t("platform.dialog.default_description")}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {props.children}
        {props.audit ? <AuditNote>{props.audit}</AuditNote> : null}
        {stale ? (
          <Alert className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
            <TriangleAlertIcon aria-hidden="true" />
            <AlertTitle>{t("platform.stale.title")}</AlertTitle>
            <AlertDescription className="text-foreground">
              <p>{t("platform.stale.body")}</p>
              {props.onStale ? (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    props.onStale?.(stale.current);
                  }}
                >
                  {t("platform.stale.look")}
                </Button>
              ) : null}
            </AlertDescription>
          </Alert>
        ) : error !== null ? (
          <Alert variant="destructive">
            <AlertTitle>{props.describeError?.(error) ?? errorMessage(error)}</AlertTitle>
            <AlertDescription>
              <p>{t(outcomeUnknown(error) ? "platform.error.outcome_unknown" : "platform.error.nothing_changed")}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>{t("platform.dialog.cancel")}</AlertDialogCancel>
          <Button
            variant={props.tone === "destructive" ? "destructive" : "default"}
            disabled={pending || props.confirmDisabled === true || stale !== null}
            onClick={() => void submit()}
          >
            {pending ? <Spinner /> : null}
            {pending ? props.pendingLabel : props.confirmLabel}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
