import type { SessionChannel } from "../session/session-channel.ts";

/**
 * Channels held in memory, for tests: each `tab()` is one tab's channel, and an announcement
 * reaches every other tab on a later task, as `BroadcastChannel` delivers it.
 */
export function memorySessionChannels(): { tab(): SessionChannel } {
  const tabs = new Set<Set<() => void>>();
  return {
    tab() {
      const listeners = new Set<() => void>();
      tabs.add(listeners);
      return {
        announce() {
          for (const other of tabs) {
            if (other === listeners) continue;
            for (const listener of other) setTimeout(listener, 0);
          }
        },
        subscribe(listener) {
          listeners.add(listener);
          return () => listeners.delete(listener);
        },
      };
    },
  };
}
