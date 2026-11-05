import { useEffect, useRef, useState } from "react";

import { type AnswerProgress, followAnswer, NO_PROGRESS } from "../../api/answer-stream.ts";
import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import { useStreamSession } from "../../session/SessionProvider.tsx";

/**
 * Follows the answer stream of the question `runId` while the component is mounted
 * (api/answer-stream.ts): what has arrived of its answer, and `onEnd` once the stream says the
 * question ended, so the page reads the conversation at once instead of at its next poll. The
 * stream names the page's user and tenant (`useStreamSession`) on every connection.
 *
 * Opened from the start of the answer: a page opened (or reloaded) while the question runs gets
 * what was written so far, then the rest as it is written.
 */
export function useAnswerStream(conversationId: string, runId: string, onEnd: () => void): AnswerProgress {
  const api = useConversationApi();
  const session = useStreamSession();
  const [progress, setProgress] = useState<AnswerProgress>(NO_PROGRESS);
  // The stream lives as long as the question: the API object, the session and the callback are
  // read when they are needed, so a new identity of one of them does not open it again.
  const current = useRef({ api, session, onEnd });
  useEffect(() => {
    current.current = { api, session, onEnd };
  });
  useEffect(() => {
    setProgress(NO_PROGRESS);
    const follower = followAnswer({
      open: (handlers) =>
        current.current.api.streamAnswer(conversationId, runId, { ...handlers, session: current.current.session }),
      onChange: setProgress,
      onEnd: () => current.current.onEnd(),
    });
    return () => follower.close();
  }, [conversationId, runId]);
  return progress;
}
