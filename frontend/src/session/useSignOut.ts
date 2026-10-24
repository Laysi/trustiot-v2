import { useMutation } from "@tanstack/react-query";
import { useNavigate } from "react-router";

import { classifyError } from "../api/errors.ts";
import { LOGIN_PATH, loginPath, SETTINGS_PATH } from "../app/paths.ts";
import { HANDLES_UNAUTHENTICATED } from "../app/query-client.ts";
import { z } from "../lib/zod.ts";
import { useSession } from "./SessionProvider.tsx";

export type SignOutScope = "this" | "everywhere";

/**
 * Signs out of this browser or of every device, then shows the sign-in page with a notice. The
 * page moves first and the cached data goes after, so the signed-in layout never sees a missing
 * session and sends the user to sign in with a `next` they did not ask for.
 *
 * Signing out everywhere answers its own 401: this session had already ended, so the request
 * ended nothing and the other devices are still signed in. The sign-in page says so, and so does
 * the settings page it returns to, instead of the plain sign-in redirect that would leave the user
 * believing it worked. Signing out of this browser never fails with a 401 (`logOut` takes an
 * ended session as signed out).
 */
export function useSignOut() {
  const navigate = useNavigate();
  const { endSession, forget } = useSession();
  return useMutation({
    mutationFn: (scope: SignOutScope) => endSession(scope),
    meta: HANDLES_UNAUTHENTICATED,
    onSuccess: async (_result, scope) => {
      await navigate(LOGIN_PATH, { replace: true, state: { signedOut: scope } });
      await forget();
    },
    onError: async (error, scope) => {
      if (scope !== "everywhere" || classifyError(error) !== "unauthenticated") return;
      await navigate(loginPath(SETTINGS_PATH), { replace: true, state: SIGN_OUT_EVERYWHERE_UNFINISHED });
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

/**
 * The history state of the sign-in page after signing out everywhere found this session already
 * ended, and of the page it returns to after sign-in.
 */
export const SIGN_OUT_EVERYWHERE_UNFINISHED = { signOutEverywhereUnfinished: true } as const;

const UnfinishedState = z.object({ signOutEverywhereUnfinished: z.literal(true) });

/** Whether signing out everywhere did not go through, so the other devices are still signed in. */
export function signOutEverywhereUnfinished(state: unknown): boolean {
  return UnfinishedState.safeParse(state).success;
}
