import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { useModelRoleApi } from "../../../api/ModelRoleApiProvider.tsx";
import { ApiError } from "../../../api/errors.ts";
import type { ModelRole, ModelRoles } from "../../../api/modelRoles.ts";
import { MODEL_ROLES_KEY } from "./queries.ts";

/** Where a change to a role stands. */
export type ChangeState =
  | { kind: "idle" }
  | { kind: "pending" }
  /** The AI gateway refused or failed; the error says what, and nothing was changed unless it says otherwise. */
  | { kind: "error"; error: unknown }
  /** Another version is current (409): `fresh` is the role as it is now, when it could be read. */
  | { kind: "conflict"; currentVersion: number; fresh: ModelRole | null; rereadFailed?: boolean }
  /** The outcome was not known (504): the roles are being read again, with nothing sent a second time. */
  | { kind: "checking" }
  /** The re-read shows the change took effect. */
  | { kind: "applied"; role: ModelRole }
  /** The re-read shows the role as it was: sending it again is safe. */
  | { kind: "not_applied"; role: ModelRole }
  /** The re-read failed too, so nobody knows. */
  | { kind: "unknown" };

const CONFLICT_KEY = "ai_model_role.version_conflict";
const UNKNOWN_KEY = "ai_model_role.outcome_unknown";

function isConflict(error: unknown): error is ApiError {
  return error instanceof ApiError && error.status === 409 && error.body?.message_key === CONFLICT_KEY;
}

function isOutcomeUnknown(error: unknown): error is ApiError {
  return error instanceof ApiError && error.status === 504 && error.body?.message_key === UNKNOWN_KEY;
}

/**
 * Sends a change to a role and keeps what the platform administrator has to know about it: a 409
 * keeps the draft and offers the latest version, and a 504 (the AI gateway's answer was lost) is
 * never resent: the roles are read again and the page says whether the change took effect.
 * `tookEffect` says, of the role as it is read again, whether the change is in it.
 */
export function useRoleChange(options: {
  base: ModelRole;
  send: (base: ModelRole) => Promise<ModelRole>;
  tookEffect: (fresh: ModelRole) => boolean;
  /** The change is in (also found out by reading again): the caller closes and tells the page. */
  onApplied: (role: ModelRole) => void;
}) {
  const api = useModelRoleApi();
  const queryClient = useQueryClient();
  const [state, setState] = useState<ChangeState>({ kind: "idle" });

  /** The role as it is now, and the list in the page updated with it. */
  const reread = async (): Promise<ModelRole | null> => {
    try {
      const roles: ModelRoles = await api.list();
      queryClient.setQueryData(MODEL_ROLES_KEY, roles);
      return roles.roles.find((role) => role.role === options.base.role) ?? null;
    } catch {
      return null;
    }
  };

  const run = async () => {
    setState({ kind: "pending" });
    try {
      const role = await options.send(options.base);
      queryClient.setQueryData<ModelRoles>(MODEL_ROLES_KEY, (roles) =>
        roles ? { ...roles, roles: roles.roles.map((item) => (item.role === role.role ? role : item)) } : roles,
      );
      options.onApplied(role);
    } catch (error) {
      if (isConflict(error)) {
        const fresh = await reread();
        setState({ kind: "conflict", currentVersion: fresh?.version ?? Number(error.body?.details?.current_version ?? 0), fresh });
      } else if (isOutcomeUnknown(error)) {
        setState({ kind: "checking" });
        const fresh = await reread();
        if (fresh === null) setState({ kind: "unknown" });
        else if (options.tookEffect(fresh)) setState({ kind: "applied", role: fresh });
        else if (fresh.version === options.base.version) setState({ kind: "not_applied", role: fresh });
        else setState({ kind: "conflict", currentVersion: fresh.version, fresh });
      } else {
        setState({ kind: "error", error });
      }
    }
  };

  return { state, setState, run, reread };
}
