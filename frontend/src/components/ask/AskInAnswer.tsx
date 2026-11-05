import { SparklesIcon } from "lucide-react";
import { type ReactNode, useMemo } from "react";

import { type ComponentExtras, ComponentExtrasProvider } from "../../dashboard/extensions.tsx";
import type { PlatformComponent } from "../../dashboard/spec.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { AskAiButton } from "./AskAiButton.tsx";
import { useConversationAsk } from "./conversation-ask.ts";
import { componentQuestions } from "./questions.ts";
import { StretchAsk } from "./StretchAsk.tsx";

/**
 * The row under a component in an answer (design 08): 「問 AI」 and two or three questions from
 * the component's kind and title, separated from the content by a dashed line. The group names
 * the component, so the buttons can say only what they ask.
 */
export function ComponentAskRow({ spec }: { spec: PlatformComponent }) {
  const { t } = useI18n();
  const questions = componentQuestions(t, spec);
  if (questions.length === 0) return null;
  return (
    <fieldset className="m-0 flex min-w-0 flex-wrap items-center gap-2 border-0 border-t border-dashed p-0 pt-3">
      <legend className="sr-only">{t("ask.row.group", { title: spec.title })}</legend>
      <span aria-hidden="true" className="mr-0.5 inline-flex items-center gap-1 text-xs text-muted-foreground">
        <SparklesIcon className="size-3.5" />
        {t("ask.row.label")}
      </span>
      {questions.map((question) => (
        <AskAiButton key={question.label} size="sm" label={question.label} question={question.text} />
      ))}
    </fieldset>
  );
}

/**
 * Gives the components below the 「問 AI」 row and, on a zoomed chart, 「分析這段」, when they are
 * in a conversation that can be asked in (the conversation page provides `ConversationAsk`; a
 * shared conversation, which is read only, does not). Elsewhere the components stay as they are.
 */
export function AskInAnswer({ children }: { children: ReactNode }) {
  const here = useConversationAsk();
  const extras = useMemo<ComponentExtras>(
    () => ({
      footer: (spec) => <ComponentAskRow spec={spec} />,
      stretch: (stretch) => <StretchAsk subject={{ kind: "component", title: stretch.title }} stretch={stretch} />,
    }),
    [],
  );
  if (here === null) return children;
  return <ComponentExtrasProvider extras={extras}>{children}</ComponentExtrasProvider>;
}
