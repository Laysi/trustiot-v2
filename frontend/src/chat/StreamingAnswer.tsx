import {
  AssistantRuntimeProvider,
  MessagePrimitive,
  type ThreadMessageLike,
  ThreadPrimitive,
  useExternalStoreRuntime,
} from "@assistant-ui/react";
import { useMemo } from "react";

import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { AnswerOriginContext, MarkdownText } from "./MarkdownText.tsx";

/** The message ID of the answer being written: the only message of its own runtime. */
const STREAMING_ID = "answer-being-written";

/**
 * An answer while it is being written (Laysi/trustiot-v2#1046): the text that arrived so far,
 * drawn exactly as a finished answer is (assistant-ui's message parts through MarkdownText and
 * its rules), with a line saying the rest is on its way. It has a runtime of its own, holding
 * this one message, so the conversation's thread (Thread.tsx) does not change while it streams;
 * when the question is answered the thread shows the stored answer in its place.
 */
export function StreamingAnswer({ conversationId, text }: { conversationId: string; text: string }) {
  const { t } = useI18n();
  const origin = useMemo(() => ({ conversationId }), [conversationId]);
  const messages = useMemo<readonly ThreadMessageLike[]>(
    () => [{ id: STREAMING_ID, role: "assistant", content: text, status: { type: "running" } }],
    [text],
  );
  const runtime = useExternalStoreRuntime({
    messages,
    convertMessage: (message: ThreadMessageLike) => message,
    isRunning: false,
    isDisabled: true,
    onNew: () => Promise.reject(new Error("asking is not part of this answer")),
  });
  return (
    <AnswerOriginContext.Provider value={origin}>
      <AssistantRuntimeProvider runtime={runtime}>
        <ThreadPrimitive.Messages>
          {() => (
            <article aria-label={t("chat.thread.writing_label")} aria-busy="true" className="grid min-w-0 gap-2">
              <MessagePrimitive.Parts components={{ Text: MarkdownText }} />
              <p role="status" className="inline-flex items-center gap-2 text-sm text-muted-foreground">
                <Spinner />
                {t("chat.composer.writing")}
              </p>
            </article>
          )}
        </ThreadPrimitive.Messages>
      </AssistantRuntimeProvider>
    </AnswerOriginContext.Provider>
  );
}
