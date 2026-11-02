/**
 * How the tabs of one browser tell each other that the session changed (Laysi/trustiot-v2#678).
 * Every tab shares the session cookie, and so the session and its current tenant: after one tab
 * switches tenants, signs in or signs out, the others read the session again instead of showing
 * the one they cached. The message carries nothing: a tab that gets one only reads the session,
 * which is always safe, so a forged or garbled message can do no more than cause one read. A tab
 * that misses a message (frozen in the background, or a browser without `BroadcastChannel`) still
 * reads the session when it becomes visible again, and the cookie check refuses its requests for
 * a user or a tenant the session no longer has (`session_changed`), so this is how the tabs learn
 * quickly, not what keeps a write out of the wrong tenant or account.
 */
export interface SessionChannel {
  /** Tells the browser's other tabs that the session changed; never this one. */
  announce(): void;
  /** Calls `listener` whenever another tab announces; returns the unsubscribe. */
  subscribe(listener: () => void): () => void;
}

/** The `BroadcastChannel` name. */
export const SESSION_CHANNEL_NAME = "trustiot.session";

/** What a tab posts; read by nobody (see `SessionChannel`), so it never needs parsing. */
const ANNOUNCEMENT = "session_changed";

/**
 * The channel over `BroadcastChannel`, which reaches every other tab of this origin in this
 * browser (and its other windows), but not this one. `undefined` where the browser has none or
 * refuses to open one: the tabs then learn of changes when they become visible, and from the
 * cookie check.
 */
export function browserSessionChannel(): SessionChannel | undefined {
  if (typeof BroadcastChannel === "undefined") return undefined;
  let channel: BroadcastChannel;
  try {
    channel = new BroadcastChannel(SESSION_CHANNEL_NAME);
  } catch {
    return undefined;
  }
  return {
    announce() {
      try {
        channel.postMessage(ANNOUNCEMENT);
      } catch {
        // A closed channel; the other tabs still learn when they become visible.
      }
    },
    subscribe(listener) {
      const onMessage = () => listener();
      channel.addEventListener("message", onMessage);
      return () => channel.removeEventListener("message", onMessage);
    },
  };
}
