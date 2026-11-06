import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { usePlatformApi } from "../../api/pending/PlatformApiProvider.tsx";
import type { LoggedActor } from "../../api/pending/platform.ts";

// The back office's query keys. They all start with "platform", so one write refreshes what it may
// have changed (`platformKey`): the figures of a tenant are on the list, the tenant's page, the
// gateways' tenant column and the operation log at once.

export const platformKey = ["platform"] as const;
export const tenantsKey = [...platformKey, "tenants"] as const;
export const tenantKey = (tenantId: string) => [...platformKey, "tenant", tenantId] as const;
export const membersKey = (tenantId: string) => [...platformKey, "members", tenantId] as const;
export const eventsKey = (scope: string, from: string, to: string) =>
  [...platformKey, "events", scope, from, to] as const;
export const gatewaysKey = (tenantId: string) => [...platformKey, "gateways", tenantId] as const;
export const provisioningKey = (workstation: string, from: string, to: string) =>
  [...platformKey, "provisioning", workstation, from, to] as const;

/** The first 8 and last 4 characters of an ID: enough to tell two apart in a list. */
export function shortId(id: string): string {
  return id.length <= 14 ? id : `${id.slice(0, 8)}…${id.slice(-4)}`;
}

/**
 * The emails of the users an operation log or a state names (decision Q14: events carry user IDs,
 * and only a platform administrator's lookup turns them into emails). Returns a function that
 * names a user: the email, or the short ID while the lookup is on its way or has no answer for it.
 */
export function useUserNames(userIds: readonly (string | null | undefined)[]): (userId: string) => string {
  const api = usePlatformApi();
  const ids = [...new Set(userIds.filter((id): id is string => typeof id === "string"))].sort();
  const lookup = useQuery({
    queryKey: [...platformKey, "emails", ids.join(",")],
    queryFn: ({ signal }) => api.lookupUserEmails(ids, signal),
    enabled: ids.length > 0,
    staleTime: 5 * 60_000,
  });
  return (userId) => lookup.data?.[userId] ?? shortId(userId);
}

export function actorUserId(actor: LoggedActor | null): string | null {
  return actor?.kind === "user" ? actor.userId : null;
}

/** The current time, refreshed every `periodMs`, for a waiting time that counts up. */
export function useNow(periodMs: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), periodMs);
    return () => clearInterval(timer);
  }, [periodMs]);
  return now;
}
