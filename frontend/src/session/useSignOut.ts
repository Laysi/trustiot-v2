import { useMutation } from "@tanstack/react-query";
import { useNavigate } from "react-router";

import { LOGIN_PATH } from "../app/paths.ts";
import { z } from "../lib/zod.ts";
import { useSession } from "./SessionProvider.tsx";

export type SignOutScope = "this" | "everywhere";

/**
 * Signs out of this browser or of every device, then shows the sign-in page with a notice. The
 * page moves first and the cached data goes after, so the signed-in layout never sees a missing
 * session and sends the user to sign in with a `next` they did not ask for.
 */
export function useSignOut() {
  const navigate = useNavigate();
  const { endSession, forget } = useSession();
  return useMutation({
    mutationFn: (scope: SignOutScope) => endSession(scope),
    onSuccess: async (_result, scope) => {
      await navigate(LOGIN_PATH, { replace: true, state: { signedOut: scope } });
      await forget();
    },
  });
}

/** The history entry's state is kept by the browser across reloads, so it is read with a schema. */
const SignedOutState = z.object({ signedOut: z.enum(["this", "everywhere"]) });

/** Which sign-out brought the user to the sign-in page, if one did. */
export function signedOutScope(state: unknown): SignOutScope | null {
  const parsed = SignedOutState.safeParse(state);
  return parsed.success ? parsed.data.signedOut : null;
}
