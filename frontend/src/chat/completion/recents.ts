import { z } from "../../lib/zod.ts";

// The items a user mentioned last (design: 「只輸入 @ 時先列『最近使用』」). They are the user's own
// convenience, so they are kept in this browser only, under the user and the tenant: another
// tenant's list is another key. The list holds keys, never names: it is shown only where a key is
// still in the tenant's current devices, models and ranges, so a stale or edited entry shows nothing.

export const RECENT_LIMIT = 5;

const RecentSchema = z.array(z.string().max(300)).max(RECENT_LIMIT * 4);

const keyOf = (user: string, tenant: string) => `trustiot.mentions.recent.${user}.${tenant}`;

function storage(): Storage | undefined {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

/** The entry keys last mentioned, newest first; none when the browser keeps nothing or it holds something else. */
export function loadRecents(user: string, tenant: string): string[] {
  try {
    const raw = storage()?.getItem(keyOf(user, tenant));
    if (raw === null || raw === undefined) return [];
    const parsed = RecentSchema.safeParse(JSON.parse(raw));
    return parsed.success ? parsed.data.slice(0, RECENT_LIMIT) : [];
  } catch {
    return [];
  }
}

/** Puts `entryKey` first and returns the new list; a browser that keeps nothing still gets it back for this session. */
export function rememberRecent(user: string, tenant: string, entryKey: string, current: readonly string[]): string[] {
  const next = [entryKey, ...current.filter((key) => key !== entryKey)].slice(0, RECENT_LIMIT);
  try {
    storage()?.setItem(keyOf(user, tenant), JSON.stringify(next));
  } catch {
    // A full or blocked store only loses the convenience.
  }
  return next;
}
