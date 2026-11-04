import {
  AssistantRuntimeProvider,
  MessagePrimitive,
  ThreadPrimitive,
  useExternalStoreRuntime,
} from "@assistant-ui/react";
import { CheckIcon, CopyIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import type { ConversationMessage } from "../api/conversations.ts";
import { Button } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { AnswerOriginContext, MarkdownText } from "./MarkdownText.tsx";

/** How long the copy button says it copied. */
const COPIED_MS = 3000;

/** The message ID assistant-ui keys a message by: its place in the conversation. */
const messageId = (index: number) => `message-${index}`;

/**
 * Copies an answer's text, exactly as the API gave it, with the Clipboard API, and says so next
 * to the button (a live region): copied, or why it could not.
 */
function CopyAnswer({ text }: { text: string }) {
  const { t } = useI18n();
  const [state, setState] = useState<"idle" | "copied" | "failed">("idle");
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const copy = async () => {
    clearTimeout(timer.current);
    try {
      await navigator.clipboard.writeText(text);
      setState("copied");
    } catch {
      setState("failed");
    }
    timer.current = setTimeout(() => setState("idle"), COPIED_MS);
  };
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Button variant="ghost" size="sm" onClick={() => void copy()}>
        {state === "copied" ? <CheckIcon /> : <CopyIcon />}
        {t("chat.answer.copy")}
      </Button>
      <p role="status" className={state === "failed" ? "text-sm text-destructive" : "text-sm text-muted-foreground"}>
        {state === "copied" ? t("chat.answer.copied") : state === "failed" ? t("chat.answer.copy_failed") : null}
      </p>
    </div>
  );
}

function Question({ text }: { text: string }) {
  const { t } = useI18n();
  return (
    <li className="max-w-[80%] self-end rounded-xl border border-ask-border bg-ask px-[15px] py-[9px] text-base leading-6 whitespace-pre-wrap text-on-ask wrap-anywhere">
      <span className="sr-only">{t("chat.thread.you_asked")}</span>
      {text}
    </li>
  );
}

function Answer({ text }: { text: string }) {
  const { t } = useI18n();
  return (
    <li className="grid grid-cols-[2rem_minmax(0,1fr)] items-start gap-x-3">
      <img src="/favicon.svg" alt="" className="size-8" />
      <article aria-label={t("chat.thread.answer_label")} className="grid min-w-0 gap-2">
        <MessagePrimitive.Parts components={{ Text: MarkdownText }} />
        <CopyAnswer text={text} />
      </article>
    </li>
  );
}

/**
 * A conversation's questions and answers, oldest first, through assistant-ui's thread
 * (`useExternalStoreRuntime` over the messages the API gave): a question is its text as typed, an
 * answer is Markdown drawn by MarkdownText's rules. Nothing can be asked here yet: the runtime is
 * disabled, and there is no composer (Laysi/trustiot-v2#149 adds it).
 */
export function Thread({
  conversationId,
  messages,
}: {
  conversationId: string;
  messages: readonly ConversationMessage[];
}) {
  const { t } = useI18n();
  // assistant-ui hands a message back by its ID; the copy button takes the text from here, as the API gave it.
  const texts = useMemo(() => new Map(messages.map((message, index) => [messageId(index), message.text])), [messages]);
  const origin = useMemo(() => ({ conversationId }), [conversationId]);
  const runtime = useExternalStoreRuntime({
    messages,
    convertMessage: (message, index) => ({
      id: messageId(index),
      role: message.kind === "question" ? "user" : "assistant",
      content: message.text,
      ...(message.kind === "answer" ? { status: { type: "complete", reason: "stop" } as const } : {}),
    }),
    isRunning: false,
    isDisabled: true,
    onNew: () => Promise.reject(new Error("asking is not part of this page")),
  });
  return (
    <AnswerOriginContext.Provider value={origin}>
      <AssistantRuntimeProvider runtime={runtime}>
        <ol aria-label={t("chat.thread.label")} className="flex flex-col gap-6">
          <ThreadPrimitive.Messages>
            {({ message }) => {
              const text = texts.get(message.id) ?? "";
              return message.role === "user" ? <Question text={text} /> : <Answer text={text} />;
            }}
          </ThreadPrimitive.Messages>
        </ol>
      </AssistantRuntimeProvider>
    </AnswerOriginContext.Provider>
  );
}
