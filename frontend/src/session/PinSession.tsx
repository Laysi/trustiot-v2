import { createContext, type ReactNode, useContext, useMemo, useState } from "react";

import { type PinnedSession, pinSession } from "../api/client.ts";

/**
 * What the pages below were mounted for (Laysi/trustiot-v2#678): `Layout` pins the user and
 * `TenantGate` the tenant, each around the outlet it remounts when that changes. The value is
 * taken once, when the boundary mounts, and never follows the session: a page that is still
 * finishing work after the session moved on (an `await` before a request, a later step of a
 * mutation, which outlives the page) names what it was made for, and the cookie check refuses it.
 */
const PinnedSessionContext = createContext<PinnedSession | undefined>(undefined);

/**
 * Pins `pin` (and what an enclosing `PinSession` pinned) for the pages below, as they were when it
 * mounted. Give it a `key` of what it pins, so a change mounts a new one.
 */
export function PinSession({ pin, children }: { pin: PinnedSession; children: ReactNode }) {
  const parent = useContext(PinnedSessionContext);
  const [pinned] = useState<PinnedSession>(() => ({ ...parent, ...pin }));
  return <PinnedSessionContext.Provider value={pinned}>{children}</PinnedSessionContext.Provider>;
}

/** What the enclosing `PinSession`s pinned; `undefined` outside them (the sign-in page). */
export function usePinnedSession(): PinnedSession | undefined {
  return useContext(PinnedSessionContext);
}

/**
 * `target` (an API object, or the client) bound to the page: every request it makes names the
 * pinned user and tenant (`pinSession`). Outside any `PinSession` it is `target` itself.
 */
export function useSessionBound<T extends object>(target: T): T {
  const pinned = usePinnedSession();
  return useMemo(() => (pinned === undefined ? target : pinSession(target, pinned)), [target, pinned]);
}
