import { InfoIcon } from "lucide-react";
import { useId, useState } from "react";

import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { Label } from "../../components/ui/label.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useDisableRule } from "./actions.ts";

/** The longest reason the box takes; the backend's limit is not set yet, so this only keeps the box sane. */
const REASON_MAX = 500;

/**
 * Disabling one rule (design 「停用與暫停」, decisions Q2 and Q3): the effect in words, an optional
 * reason that goes to the agent as input for its next adjustment, and a button that names the
 * action. The focus starts on Cancel, a refusal shows in the dialog, which stays open, and it
 * cannot be closed while the request runs. From the Email link (`fromEmail`) it says the link
 * only led here: nothing is disabled until this button.
 */
export function DisableRuleDialog({
  rule,
  fromEmail = false,
  onOpenChange,
}: {
  /** The rule to disable; `null` keeps the dialog closed. */
  rule: { id: string; name: string } | null;
  fromEmail?: boolean;
  onOpenChange(open: boolean): void;
}) {
  const { t, errorMessage } = useI18n();
  const mutation = useDisableRule();
  const [reason, setReason] = useState("");
  const reasonId = useId();
  const hintId = `${reasonId}-hint`;
  const close = (open: boolean) => {
    if (mutation.isPending) return;
    if (!open) {
      setReason("");
      mutation.reset();
    }
    onOpenChange(open);
  };
  return (
    <AlertDialog open={rule !== null} onOpenChange={close}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("alerts.disable.title", { rule: rule?.name ?? "" })}</AlertDialogTitle>
          <AlertDialogDescription>{t("alerts.disable.body")}</AlertDialogDescription>
        </AlertDialogHeader>
        {fromEmail ? (
          <Alert role="status" className="grid-cols-[1rem_1fr]">
            <InfoIcon aria-hidden="true" />
            <AlertDescription>
              <p>{t("alerts.disable.from_email")}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <div className="grid gap-2">
          <Label htmlFor={reasonId}>{t("alerts.disable.reason")}</Label>
          <textarea
            id={reasonId}
            aria-describedby={hintId}
            value={reason}
            maxLength={REASON_MAX}
            placeholder={t("alerts.disable.reason_placeholder")}
            disabled={mutation.isPending}
            className={cn(controlClasses, "h-auto min-h-20 py-2")}
            onChange={(event) => setReason(event.target.value)}
          />
          <p id={hintId} className="text-sm text-muted-foreground">
            {t("alerts.disable.reason_hint")}
          </p>
        </div>
        {mutation.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutation.isPending}>{t("alerts.cancel")}</AlertDialogCancel>
          <Button
            variant="destructive"
            disabled={mutation.isPending || rule === null}
            onClick={() => {
              if (rule === null) return;
              const trimmed = reason.trim();
              mutation.mutate(
                { ruleId: rule.id, ...(trimmed !== "" ? { reason: trimmed } : {}) },
                {
                  onSuccess: () => {
                    setReason("");
                    onOpenChange(false);
                  },
                },
              );
            }}
          >
            {mutation.isPending ? <Spinner /> : null}
            {mutation.isPending ? t("alerts.disable.pending") : t("alerts.disable.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
