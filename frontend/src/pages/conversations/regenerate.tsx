import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RefreshCwIcon, XIcon } from "lucide-react";
import { createContext, type ReactNode, useContext, useEffect, useMemo, useRef, useState } from "react";

import type { ConversationDetail, ConversationMessage, ConversationRun } from "../../api/conversations.ts";
import { ApiError } from "../../api/errors.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSessionBound } from "../../session/PinSession.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import { conversationKey } from "./queries.ts";

/**
 * Regenerating the last answer of a conversation (Laysi/trustiot-v2#1047). The endpoint is
 * `POST /conversations/{id}/regenerate` of Laysi/trustiot-v2#649, which api/openapi.json does not
 * have yet, so the feature reaches this page only through the context below: its default is "not
 * available", and the UI preview's fake (test/fake-regenerate.ts) is the only provider. The pending
 * module (api/pending/regenerate.ts) implements `AnswerRegenerator`; this file never imports
 * `src/api/pending/` (`check:dist`). When the endpoint lands, the provider moves into `AppProviders`
 * over the real client and `seq` becomes required in api/conversations.ts.
 *
 * What the service does (services/assistant/README.md, 「提問」, Laysi/trustiot-v2#649):
 *
 * - The request names the answer by its position (`seq`): the conversation's last one, as this
 *   page's last read of the conversation gave it. One that is no longer the last answer (a retry, a
 *   double press, a stale tab) is 409 `question.regenerate_stale` and nothing is discarded. The page
 *   then reads the conversation again and shows how it is now; it never sends the request again
 *   with another position by itself.
 * - The last round is discarded when the new question starts, so the old answer never comes back:
 *   when the new answer fails or is stopped the conversation stays at the round before. The page
 *   then offers only to ask the question again as a new question (it goes back into the composer,
 *   as for any question that ends without an answer), never a second regeneration: the last answer
 *   is then an earlier round's, and a regeneration of it would discard that round.
 * - It is a question run in the background: 409 while one runs, 429 at the limits, 503 when busy,
 *   and a run to follow and stop.
 */

/** The endpoint of the pending API (api/pending/regenerate.ts implements it). */
export interface AnswerRegenerator {
  /** Regenerates the answer at position `answer`, the conversation's last; 202 gives the run that asks the question again. */
  regenerate(conversationId: string, answer: number): Promise<ConversationRun>;
}

const RegenerateContext = createContext<AnswerRegenerator | null>(null);
const NOTHING: AnswerRegenerator = {
  regenerate: () => Promise.reject(new Error("regenerating answers is not available in this build")),
};

/**
 * Says to the conversation page that this build has the regenerate endpoint. Whoever provides the
 * endpoint mounts it: the fake in the UI preview and in tests.
 */
export function RegenerateAvailable({ api, children }: { api: AnswerRegenerator; children: ReactNode }) {
  return <RegenerateContext.Provider value={api}>{children}</RegenerateContext.Provider>;
}

/** The regenerate endpoint bound to the page (its requests name the page's user and tenant), or `null` where the build has none. */
function useRegenerator(): AnswerRegenerator | null {
  const value = useContext(RegenerateContext);
  const bound = useSessionBound(value ?? NOTHING);
  return value === null ? null : bound;
}

/** What the page hands the turn (the composer below the thread) after a regeneration. */
export type RegenerationHandoff =
  /** The request was accepted: follow its run, and show the question while it is answered. */
  | { kind: "started"; run: ConversationRun; text: string }
  /** The request failed after the service had discarded the answer: the question goes back into the composer. */
  | { kind: "discarded"; text: string };

/** The page's end of the hand-off between the thread's button and the turn. */
export interface RegenerationTurn {
  handoff: RegenerationHandoff | null;
  /** The turn says whether a question is being answered, so the button hides. */
  setAnswering(answering: boolean): void;
}

/** The turn's side: tells the page when it answers, and takes what the page hands over. */
export function useRegenerationHandoff(
  turn: RegenerationTurn | undefined,
  answering: boolean,
  on: { started(run: ConversationRun, text: string): void; discarded(text: string): void },
) {
  const setAnswering = turn?.setAnswering;
  useEffect(() => {
    setAnswering?.(answering);
  }, [setAnswering, answering]);
  const handled = useRef<RegenerationHandoff | null>(null);
  const handoff = turn?.handoff ?? null;
  const latest = useRef(on);
  latest.current = on;
  useEffect(() => {
    if (handoff === null || handled.current === handoff) return;
    handled.current = handoff;
    if (handoff.kind === "started") latest.current.started(handoff.run, handoff.text);
    else latest.current.discarded(handoff.text);
  }, [handoff]);
}

/** The answer a regeneration was sent for; the page keeps it to tell when that answer is gone. */
interface Target {
  seq: number;
  text: string;
}

type Feedback = { kind: "stale" } | { kind: "failed"; reason: string; discarded: boolean };

/**
 * The regenerate button of the conversation page and what follows it. `renderActions` draws the
 * button in the action row of the conversation's last answer (chat/Thread.tsx), only when the build
 * has the endpoint, the answer has its position, no question is being answered and the last answer
 * is not one a regeneration of this page left behind (below). `feedback` is the alert under the
 * thread, and `turn` is what `ConversationTurn` takes.
 */
export function useRegeneration(conversation: ConversationDetail) {
  const api = useRegenerator();
  const i18n = useI18n();
  const { t } = i18n;
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [answering, setAnswering] = useState(false);
  const [handoff, setHandoff] = useState<RegenerationHandoff | null>(null);
  const [target, setTarget] = useState<Target | null>(null);
  const [feedback, setFeedback] = useState<Feedback | null>(null);

  const last = conversation.messages.at(-1);
  const lastSeq = last?.kind === "answer" ? last.seq : undefined;
  const question = conversation.messages.at(-2);
  const questionText = question?.kind === "question" ? question.text : undefined;
  // A regeneration of this page that left an earlier round's answer last (it failed or was stopped
  // after the service discarded its round): that answer is not the one the user asked to replace,
  // and regenerating it would discard the round before the failed one. The position of an answer
  // never repeats and grows, so a newer answer (another question, answered) lifts this.
  const left = target !== null && (lastSeq === undefined || lastSeq < target.seq);

  const key = conversationKey(tenant, conversation.id);
  const refresh = () => queryClient.invalidateQueries({ queryKey: key });

  const regenerate = useMutation({
    mutationFn: (request: Target) => {
      if (api === null) return Promise.reject(new Error("regenerating answers is not available in this build"));
      return api.regenerate(conversation.id, request.seq);
    },
    onMutate: (request) => {
      setFeedback(null);
      setTarget(request);
    },
    onSuccess: async (run, request) => {
      // The old round is discarded from now on: read the conversation without it before the
      // question is shown as being answered again.
      await refresh();
      setHandoff({ kind: "started", run, text: request.text });
    },
    onError: async (error, request) => {
      // The service knows best what is left: show that, whatever it refused. The request is never
      // sent again with the position of whatever the conversation shows now.
      await refresh();
      const stale = error instanceof ApiError && error.body?.message_key === "question.regenerate_stale";
      if (stale) {
        setFeedback({ kind: "stale" });
        return;
      }
      // Some refusals come after the service discarded the round (the identity service failed to
      // start the question): then the answer is gone and the question is offered again.
      const read = queryClient.getQueryData<ConversationDetail>(key);
      const discarded = read !== undefined && !read.messages.some((message) => message.seq === request.seq);
      setFeedback({ kind: "failed", reason: i18n.errorMessage(error), discarded });
      if (discarded) setHandoff({ kind: "discarded", text: request.text });
    },
  });

  // What was said about an earlier press is not about the question that is answered now.
  useEffect(() => {
    if (answering) setFeedback(null);
  }, [answering]);

  const turn = useMemo<RegenerationTurn>(() => ({ handoff, setAnswering }), [handoff]);
  const allowed =
    api !== null &&
    lastSeq !== undefined &&
    questionText !== undefined &&
    !answering &&
    conversation.latest_run?.status !== "running" &&
    !left;

  const renderActions = (message: ConversationMessage): ReactNode => {
    if (!allowed || message !== last || lastSeq === undefined || questionText === undefined) return null;
    return (
      <Button
        variant="ghost"
        size="sm"
        disabled={regenerate.isPending}
        onClick={() => regenerate.mutate({ seq: lastSeq, text: questionText })}
      >
        <RefreshCwIcon />
        {t(regenerate.isPending ? "chat.regenerate.pending" : "chat.regenerate.button")}
      </Button>
    );
  };

  const alert =
    feedback === null ? null : (
      <Alert
        role={feedback.kind === "failed" ? "alert" : "status"}
        variant={feedback.kind === "failed" ? "destructive" : "default"}
        className="grid-cols-[1fr_auto] items-center"
      >
        <AlertDescription className="col-start-1">
          <p>
            {feedback.kind === "stale"
              ? t("chat.regenerate.stale")
              : t(feedback.discarded ? "chat.regenerate.discarded" : "chat.regenerate.failed", {
                  reason: feedback.reason,
                })}
          </p>
        </AlertDescription>
        <Button variant="ghost" size="icon" aria-label={t("chat.dismiss")} onClick={() => setFeedback(null)}>
          <XIcon />
        </Button>
      </Alert>
    );

  return { renderActions, feedback: alert, turn };
}
