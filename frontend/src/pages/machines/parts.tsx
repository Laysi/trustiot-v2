import { CloudOffIcon, RefreshCwIcon, XIcon } from "lucide-react";
import { type ReactNode, useEffect } from "react";

import { isServiceUnavailable } from "../../api/pending/machines.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";

/**
 * The field-context service does not answer (docs/design/ms4-ms5/machines.html, state 8): what to
 * do meanwhile is the design's wording (field-context.md, point 6). `retry` shows a button.
 */
export function UnavailableNotice({ onRetry, retrying }: { onRetry?: () => void; retrying?: boolean }) {
  const { t } = useI18n();
  return (
    <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
      <CloudOffIcon aria-hidden="true" />
      <AlertTitle>{t("machines.unavailable.title")}</AlertTitle>
      <AlertDescription>
        <p>{t("machines.unavailable.body")}</p>
        {onRetry ? (
          <Button variant="outline" size="sm" onClick={onRetry} disabled={retrying}>
            <RefreshCwIcon aria-hidden="true" />
            {t("page.action.retry")}
          </Button>
        ) : null}
      </AlertDescription>
    </Alert>
  );
}

/** A failed request: the service-unavailable notice when that is what happened, else the error's text. */
export function RequestError({ error, onRetry, retrying }: { error: unknown; onRetry?: () => void; retrying?: boolean }) {
  const { errorMessage } = useI18n();
  if (isServiceUnavailable(error)) return <UnavailableNotice {...(onRetry ? { onRetry } : {})} retrying={retrying ?? false} />;
  return (
    <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
      <AlertDescription className="col-start-1">
        <p>{errorMessage(error)}</p>
      </AlertDescription>
      {onRetry ? <RetryButton onRetry={onRetry} retrying={retrying ?? false} /> : null}
    </Alert>
  );
}

function RetryButton({ onRetry, retrying }: { onRetry(): void; retrying: boolean }) {
  const { t } = useI18n();
  return (
    <Button variant="outline" onClick={onRetry} disabled={retrying}>
      {t("page.action.retry")}
    </Button>
  );
}

/** How long a toast stays when nobody closes it. */
const TOAST_MS = 15_000;

/**
 * The message after a changeover (machines.html, state 4): a `role="status"` block at the bottom of
 * the screen with a link, closed by a button or after a while. Radix's toast is not used (the
 * design notes ask to check that it injects no `<style>` first); this is plain markup.
 */
export function Toast({ children, onClose }: { children: ReactNode; onClose(): void }) {
  const { t } = useI18n();
  useEffect(() => {
    const timer = setTimeout(onClose, TOAST_MS);
    return () => clearTimeout(timer);
  }, [onClose]);
  return (
    <div className="pointer-events-none fixed inset-x-4 bottom-4 z-40 flex justify-center max-sm:bottom-20">
      <div
        role="status"
        className="pointer-events-auto flex max-w-lg items-center gap-3 rounded-lg border bg-popover px-4 py-3 text-sm text-popover-foreground shadow-lg"
      >
        {children}
        <button
          type="button"
          aria-label={t("machines.toast.close")}
          className={cn("rounded-sm p-1 text-muted-foreground hover:text-foreground", focusRing)}
          onClick={onClose}
        >
          <XIcon aria-hidden="true" className="size-4" />
        </button>
      </div>
    </div>
  );
}

/** A part number: the figure people read off the line, so it is monospaced and never broken. */
export function PartNumber({ children, className }: { children: ReactNode; className?: string }) {
  return <span className={cn("font-mono font-semibold break-all", className)}>{children}</span>;
}
