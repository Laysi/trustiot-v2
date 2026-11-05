import { ArrowLeftIcon } from "lucide-react";
import { Link } from "react-router";

import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { focusRing } from "../ui/button.tsx";
import { useAskOrigin } from "./origin.ts";

/**
 * 「回到空壓機 A2」, at the top of a conversation a button on another page opened (docs/design/
 * home-ai/design.md, decision 5): the page the question came from, as it was (its address, range
 * included). Nothing when the conversation was not opened that way.
 */
export function BackToOrigin() {
  const { t } = useI18n();
  const origin = useAskOrigin();
  if (origin === null) return null;
  return (
    <Link
      to={origin.to}
      className={cn(
        "inline-flex max-w-full min-w-0 items-center gap-1.5 justify-self-start rounded-sm text-sm text-muted-foreground underline-offset-4 hover:text-foreground hover:underline",
        focusRing,
      )}
    >
      <ArrowLeftIcon aria-hidden="true" className="size-4 shrink-0" />
      <span className="truncate">{t("ask.back_to", { name: origin.label })}</span>
    </Link>
  );
}
