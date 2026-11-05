import type { AiPause, RuleOrigin, RuleStatus } from "../../api/pending/alerts.ts";
import { useSession } from "../../session/SessionProvider.tsx";

/**
 * What a rule shows as its state: its own status, except that an active AI-made rule is paused
 * while every AI rule is (decision Q1: the pause leaves member-made rules running, and a rule a
 * person disabled or the backtest refused stays what it was).
 */
export type RuleState = RuleStatus | "paused";

export function ruleState(rule: { origin: RuleOrigin; status: RuleStatus }, pause: AiPause | undefined): RuleState {
  return pause?.paused === true && rule.origin.kind === "ai" && rule.status === "active" ? "paused" : rule.status;
}

/**
 * What the signed-in member may do here (decision Q2, design 「角色」): everyone reads and asks
 * the assistant; an editor or an admin disables and enables rules and pauses the AI rules. The
 * API refuses the rest as well; the buttons are left out, not disabled, for a viewer.
 */
export function useAlertsRole(): { canManage: boolean; isAdmin: boolean } {
  const role = useSession().session?.activeTenant?.role;
  return { canManage: role === "editor" || role === "admin", isAdmin: role === "admin" };
}

export function useTenantId(): string {
  return useSession().session?.activeTenant?.id ?? "";
}
