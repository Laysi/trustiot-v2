import type { LoggedEvent } from "../../../api/pending/platform.ts";
import type { MessageId } from "../../../i18n/messages.ts";
import type { I18n } from "../../../i18n/useI18n.ts";
import { REASON_LABELS } from "../format.ts";

type Fields = Record<string, unknown>;

function fields(payload: unknown): Fields {
  return typeof payload === "object" && payload !== null && !Array.isArray(payload) ? (payload as Fields) : {};
}

function text(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function show(value: unknown): string {
  if (value === null || value === undefined) return "—";
  return typeof value === "object" ? JSON.stringify(value) : String(value);
}

/** The user an event is about besides its actor (a member whose role changed), for the email lookup. */
export function subjectUserId(event: LoggedEvent): string | null {
  return event.type === "identity.member_role_changed" ? text(fields(event.payload).user_id) : null;
}

interface Known {
  label: MessageId;
  summary(payload: Fields, t: I18n["t"], nameOf: (userId: string) => string): string;
}

const REASONS = new Set(Object.keys(REASON_LABELS));

/** `from → to` of a `{from, to}` pair a payload carries. */
function change(value: unknown): string | null {
  const pair = fields(value);
  return "from" in pair && "to" in pair ? `${show(pair.from)} → ${show(pair.to)}` : null;
}

/**
 * The event types this build can read, each with a name and a one-line summary built from the
 * payload. A type that is not here is still listed, by its code, and its payload can be opened
 * (design.md, 「操作紀錄」): the producers add events faster than this page learns them. Summaries
 * read the payload defensively; a missing member leaves its part out, never fails the row.
 * `identity.quota_changed` and the gateway unbinding, repair, revocation and decommissioning events
 * are provisional names (their backend issues, #110 and #115, have not fixed them yet).
 */
const KNOWN: Record<string, Known> = {
  "identity.tenant_created": {
    label: "platform.event.tenant_created",
    summary: (payload, t) =>
      text(payload.name) ? t("platform.event.tenant_created.summary", { name: text(payload.name) ?? "" }) : "",
  },
  "identity.tenant_state_changed": {
    label: "platform.event.tenant_state_changed",
    summary: (payload, t) => {
      const version = show(payload.state_version);
      if (payload.state === "active") return t("platform.event.tenant_state_changed.resumed", { version });
      const reason = text(payload.reason);
      return t("platform.event.tenant_state_changed.suspended", {
        reason:
          reason && REASONS.has(reason) ? t(REASON_LABELS[reason as keyof typeof REASON_LABELS]) : (reason ?? "—"),
        version,
      });
    },
  },
  "certs.tenant_state_applied": {
    label: "platform.event.tenant_state_applied",
    summary: (payload, t) => t("platform.event.tenant_state_applied.summary", { version: show(payload.state_version) }),
  },
  "identity.member_role_changed": {
    label: "platform.event.member_role_changed",
    summary: (payload, t, nameOf) => {
      const user = text(payload.user_id) ? nameOf(text(payload.user_id) as string) : "—";
      const previous = text(payload.previous_role);
      return previous
        ? t("platform.event.member_role_changed.changed", { user, from: previous, to: show(payload.role) })
        : t("platform.event.member_role_changed.joined", { user, role: show(payload.role) });
    },
  },
  "identity.tenant_entitlements_changed": {
    label: "platform.event.entitlements_changed",
    summary: (payload, t) =>
      t("platform.event.entitlements_changed.summary", {
        plan: show(payload.plan),
        version: show(payload.entitlements_version),
      }),
  },
  "identity.quota_changed": {
    label: "platform.event.quota_changed",
    summary: (payload, t) => {
      const parts = [
        change(payload.max_gateways)
          ? t("platform.event.quota_changed.gateways", { change: change(payload.max_gateways) ?? "" })
          : null,
        change(payload.ai_extra_usd)
          ? t("platform.event.quota_changed.ai", { change: change(payload.ai_extra_usd) ?? "" })
          : null,
        text(payload.reason) ? t("platform.event.quota_changed.reason", { reason: text(payload.reason) ?? "" }) : null,
      ];
      return parts.filter(Boolean).join("; ");
    },
  },
  "assets.gateway_bound": {
    label: "platform.event.gateway_bound",
    summary: (payload, t) => t("platform.event.gateway_bound.summary", { id: show(payload.hardware_id) }),
  },
  "assets.gateway_unbound": {
    label: "platform.event.gateway_unbound",
    summary: (payload, t) => t("platform.event.gateway_unbound.summary", { id: show(payload.hardware_id) }),
  },
  "assets.gateway_sent_for_repair": {
    label: "platform.event.gateway_repair",
    summary: (payload, t) => t("platform.event.gateway_repair.summary", { id: show(payload.hardware_id) }),
  },
  "certs.gateway_certificates_revoked": {
    label: "platform.event.gateway_revoked",
    summary: (payload, t) =>
      t("platform.event.gateway_revoked.summary", { id: show(payload.hardware_id), reason: show(payload.reason) }),
  },
  "certs.gateway_decommissioned": {
    label: "platform.event.gateway_decommissioned",
    summary: (payload, t) => t("platform.event.gateway_decommissioned.summary", { id: show(payload.hardware_id) }),
  },
  "certs.gateway_registered": {
    label: "platform.event.gateway_registered",
    summary: (payload, t) =>
      t("platform.event.gateway_registered.summary", {
        id: show(payload.hardware_id),
        workstation: show(payload.workstation),
      }),
  },
  "identity.ai_model_role_changed": {
    label: "platform.event.ai_model_role_changed",
    summary: (payload, t) => {
      const models = Array.isArray(payload.models) ? payload.models.map(show).join(", ") : "—";
      return t("platform.event.ai_model_role_changed.summary", {
        role: show(payload.role),
        models,
        version: show(payload.version),
      });
    },
  },
};

export interface EventDescription {
  /** Whether this build knows the type. */
  known: boolean;
  /** The name of a known type; the code itself otherwise. */
  title: string;
  summary: string;
}

export function describeEvent(event: LoggedEvent, t: I18n["t"], nameOf: (userId: string) => string): EventDescription {
  // Own properties only: a type named "constructor" or "toString" is an unknown type, not a known one.
  const known = Object.hasOwn(KNOWN, event.type) ? KNOWN[event.type] : undefined;
  if (!known) return { known: false, title: event.type, summary: t("platform.audit.unknown_summary") };
  return { known: true, title: t(known.label), summary: known.summary(fields(event.payload), t, nameOf) };
}
