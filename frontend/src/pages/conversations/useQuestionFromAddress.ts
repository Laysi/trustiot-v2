import { useEffect, useState } from "react";
import { useSearchParams } from "react-router";

import { QUESTION_PARAM } from "../../app/paths.ts";

/**
 * The question the address names (`?q=`), for the composer's first text: another page opens the
 * conversation with a question in it (「在對話中編輯」, 「在對話中新增」) and the user reads it,
 * changes it and sends it. It is read once and then taken out of the address (kept in the
 * history entry it came with, replaced here), so a reload or a later visit does not bring an old
 * question back. React sets the text as a value, never as markup.
 */
export function useQuestionFromAddress(): string {
  const [params, setParams] = useSearchParams();
  const [question] = useState(() => params.get(QUESTION_PARAM) ?? "");
  const named = params.has(QUESTION_PARAM);
  useEffect(() => {
    if (!named) return;
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        next.delete(QUESTION_PARAM);
        return next;
      },
      { replace: true },
    );
  }, [named, setParams]);
  return question;
}
