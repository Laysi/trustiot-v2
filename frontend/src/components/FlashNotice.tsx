import { XIcon } from "lucide-react";
import { useLocation, useNavigate } from "react-router";

import { isMessageId, type MessageId } from "../i18n/messages.ts";
import type { MessageValues } from "../i18n/translate.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { Alert, AlertDescription } from "./ui/alert.tsx";
import { Button } from "./ui/button.tsx";

/** A success message a page leaves for the next one in the navigation's state ("Device created"). */
export interface Flash {
  message: MessageId;
  values?: MessageValues;
}

/** The navigation state that carries `message` to the page navigated to. */
export function flashState(message: MessageId, values?: MessageValues): { flash: Flash } {
  return { flash: { message, ...(values ? { values } : {}) } };
}

function readFlash(state: unknown): Flash | null {
  if (typeof state !== "object" || state === null || !("flash" in state)) return null;
  const flash = state.flash;
  if (typeof flash !== "object" || flash === null || !("message" in flash)) return null;
  const { message } = flash;
  if (typeof message !== "string" || !isMessageId(message)) return null;
  const values = "values" in flash && typeof flash.values === "object" && flash.values !== null ? flash.values : {};
  const clean: Record<string, string | number> = {};
  for (const [name, value] of Object.entries(values)) {
    if (typeof value === "string" || typeof value === "number") clean[name] = value;
  }
  return { message, values: clean };
}

/**
 * The message the previous page left (`flashState`), as a status line with a way to dismiss it.
 * Dismissing replaces the history entry without it, so a reload does not show it again.
 */
export function FlashNotice() {
  const { t } = useI18n();
  const location = useLocation();
  const navigate = useNavigate();
  const flash = readFlash(location.state);
  if (flash === null) return null;
  return (
    <Alert role="status" className="grid-cols-[1fr_auto] items-center gap-x-3">
      <AlertDescription className="col-start-1">
        <p className="text-foreground">{t(flash.message, flash.values)}</p>
      </AlertDescription>
      <Button
        variant="ghost"
        size="icon"
        aria-label={t("page.action.dismiss")}
        onClick={() => void navigate(`${location.pathname}${location.search}${location.hash}`, { replace: true })}
      >
        <XIcon aria-hidden="true" />
      </Button>
    </Alert>
  );
}
