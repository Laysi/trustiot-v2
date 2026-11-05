import { useLocation } from "react-router";

import { spaPath } from "../../app/paths.ts";
import { z } from "../../lib/zod.ts";

/**
 * Where a question came from, for the conversation it opens: a button on a page (the device page)
 * starts the conversation, and its page leads back to that page, 「回到空壓機 A2」
 * (docs/design/home-ai/design.md, decision 5). The router state carries it from the button to the
 * conversation page (`BackToOrigin`), beside the asked question (`AskedState`).
 */
const OriginState = z.object({ askedFrom: z.object({ to: z.string(), label: z.string() }) });

export interface AskOrigin {
  /** A path of this app. */
  to: string;
  /** What the link is called: the device's name. */
  label: string;
}

/** The router state that says the conversation was opened from `origin`. */
export function originState(origin: AskOrigin): z.input<typeof OriginState> {
  return { askedFrom: origin };
}

/**
 * The page the conversation was opened from, or `null` when it was not opened from a button. The
 * state is the app's own, but a path is only ever followed when it is one of this app's
 * (`spaPath`), so a state that names anything else is ignored.
 */
export function useAskOrigin(): AskOrigin | null {
  const state = OriginState.safeParse(useLocation().state);
  if (!state.success) return null;
  const to = spaPath(state.data.askedFrom.to);
  return to === null ? null : { to, label: state.data.askedFrom.label };
}
