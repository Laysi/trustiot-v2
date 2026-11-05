import { type ComponentType, createElement, type ReactNode } from "react";
import { ApiError } from "../api/errors.ts";
import { MembersApiProvider } from "../api/pending/MembersApiProvider.tsx";
import type {
  Invitation,
  InvitationPreview,
  Member,
  MemberList,
  MemberRole,
  MembersApi,
  Plan,
} from "../api/pending/members.ts";
import { fakeUuid } from "./fake-devices.ts";
import { PREVIEW_EMAIL } from "./fake-identity.ts";

/**
 * The tenant's member management (api/pending/members.ts), held in memory, until the identity
 * service has the endpoints (Laysi/trustiot-v2#105). It follows the rules the screens assume:
 *
 * - The seat quota counts members and pending invitations, expired ones included (the numbers of
 *   the design's pages); inviting at the limit is 403 `quota_exceeded` `member.quota_full`.
 * - Inviting a member's email is 422 on `/email` with `member.already_member`.
 * - Changing the role of, or removing, the last admin is 409 `conflict` `member.last_admin`;
 *   the check and the write are one step.
 * - A token that is unknown, used or revoked previews and accepts as 404 `invitation.invalid`;
 *   one made with `addLink(…, "mismatch")` is 403 `invitation.account_mismatch`.
 *
 * Every call is recorded in `calls`. `failNext` makes the next call to an operation fail, and
 * `hold` keeps it waiting until the returned function is called.
 */

export type Operation = keyof MembersApi;

const DAY_MS = 24 * 60 * 60 * 1000;

function conflict(messageKey: string): ApiError {
  return new ApiError(409, { code: "conflict", message_key: messageKey });
}

/** The error of a rejected invitation link: only these two kinds exist, and an invalid one says nothing more. */
function linkError(kind: "invalid" | "mismatch"): ApiError {
  return kind === "invalid"
    ? new ApiError(404, { code: "not_found", message_key: "invitation.invalid" })
    : new ApiError(403, { code: "permission_denied", message_key: "invitation.account_mismatch" });
}

interface Link {
  kind: "valid" | "invalid" | "mismatch";
  preview: InvitationPreview;
}

export class FakeMembersServer {
  readonly members = new Map<string, Member>();
  readonly invitations = new Map<string, Invitation>();
  readonly calls: { operation: Operation; args: unknown[] }[] = [];
  plan: Plan = "starter";
  /** The plan's member limit; `null` for none. */
  limit: number | null = 10;
  /** The tenant a successful acceptance joins. */
  joinedTenantId = fakeUuid();
  /** The signed-in user, who sends invitations. */
  private readonly self: { name: string | null; email: string };
  private readonly links = new Map<string, Link>();
  private readonly failures = new Map<Operation, ApiError[]>();
  private readonly holds = new Map<Operation, Promise<void>>();

  constructor(self: { id: string; name: string | null; email: string }, options: { plan?: Plan } = {}) {
    this.self = { name: self.name, email: self.email };
    if (options.plan) this.plan = options.plan;
    this.members.set(self.id, {
      user_id: self.id,
      name: self.name,
      email: self.email,
      role: "admin",
      joined_at: "2026-10-12T08:00:00Z",
    });
  }

  addMember(options: { name?: string | null; email: string; role?: MemberRole; joinedAt?: string }): Member {
    const member: Member = {
      user_id: fakeUuid(),
      name: options.name === undefined ? null : options.name,
      email: options.email,
      role: options.role ?? "viewer",
      joined_at: options.joinedAt ?? "2026-10-20T08:00:00Z",
    };
    this.members.set(member.user_id, member);
    return member;
  }

  /** A pending invitation; `expired` puts its expiry in the past. */
  addInvitation(options: {
    email: string;
    role?: MemberRole;
    invitedBy?: string;
    expired?: boolean;
    createdAt?: string;
  }): Invitation {
    const now = Date.now();
    const invitation: Invitation = {
      invitation_id: fakeUuid(),
      email: options.email,
      role: options.role ?? "viewer",
      invited_by: options.invitedBy ?? this.self.name ?? this.self.email,
      created_at: options.createdAt ?? new Date(now - 2 * DAY_MS).toISOString(),
      expires_at: new Date(now + (options.expired ? -5 : 5) * DAY_MS).toISOString(),
    };
    this.invitations.set(invitation.invitation_id, invitation);
    return invitation;
  }

  /** An invitation link: `token` leads to the preview, or to the refusal `kind` names. */
  addLink(token: string, kind: Link["kind"], preview?: Partial<InvitationPreview>): void {
    this.links.set(token, {
      kind,
      preview: {
        tenant_name: "Acme Precision Works",
        invited_by: { name: "Mei Lin", email: "mei.lin@example.com" },
        role: "editor",
        email: this.self.email,
        expires_at: new Date(Date.now() + 5 * DAY_MS).toISOString(),
        ...preview,
      },
    });
  }

  /** Makes the next call to `operation` fail with `error`. */
  failNext(operation: Operation, error: ApiError): void {
    this.failures.set(operation, [...(this.failures.get(operation) ?? []), error]);
  }

  /** Keeps the next call to `operation` waiting; calling the result lets it go on. */
  hold(operation: Operation): () => void {
    let release: () => void = () => {};
    this.holds.set(
      operation,
      new Promise<void>((resolve) => {
        release = resolve;
      }),
    );
    return release;
  }

  private async enter(operation: Operation, args: unknown[]): Promise<void> {
    this.calls.push({ operation, args });
    const held = this.holds.get(operation);
    if (held) {
      this.holds.delete(operation);
      await held;
    }
    const [failure, ...rest] = this.failures.get(operation) ?? [];
    if (failure) {
      this.failures.set(operation, rest);
      throw failure;
    }
  }

  private used(): number {
    return this.members.size + this.invitations.size;
  }

  private admins(): Member[] {
    return [...this.members.values()].filter((member) => member.role === "admin");
  }

  private snapshot(): MemberList {
    return {
      plan: this.plan,
      quota: { used: this.used(), limit: this.limit },
      members: [...this.members.values()].sort((a, b) => a.joined_at.localeCompare(b.joined_at)),
      invitations: [...this.invitations.values()].sort((a, b) => b.created_at.localeCompare(a.created_at)),
    };
  }

  api(): MembersApi {
    return {
      listMembers: async (signal) => {
        await this.enter("listMembers", [signal]);
        return this.snapshot();
      },
      listAdmins: async (signal) => {
        await this.enter("listAdmins", [signal]);
        return this.admins().map((member) => member.name ?? member.email);
      },
      invite: async (request) => {
        await this.enter("invite", [request]);
        const email = request.email.trim();
        const lower = email.toLowerCase();
        if ([...this.members.values()].some((member) => member.email.toLowerCase() === lower)) {
          throw new ApiError(422, {
            code: "validation_failed",
            message_key: "error.validation_failed",
            field_errors: [{ field: "/email", message_key: "member.already_member" }],
          });
        }
        if (this.limit !== null && this.used() >= this.limit) {
          throw new ApiError(403, { code: "quota_exceeded", message_key: "member.quota_full" });
        }
        return this.addInvitation({ email, role: request.role, createdAt: new Date().toISOString() });
      },
      resendInvitation: async (invitationId) => {
        await this.enter("resendInvitation", [invitationId]);
        const invitation = this.invitations.get(invitationId);
        if (!invitation) throw new ApiError(404, { code: "not_found", message_key: "error.not_found" });
        const renewed = { ...invitation, expires_at: new Date(Date.now() + 7 * DAY_MS).toISOString() };
        this.invitations.set(invitationId, renewed);
        return renewed;
      },
      revokeInvitation: async (invitationId) => {
        await this.enter("revokeInvitation", [invitationId]);
        if (!this.invitations.delete(invitationId)) {
          throw new ApiError(404, { code: "not_found", message_key: "error.not_found" });
        }
      },
      changeRole: async (userId, role) => {
        await this.enter("changeRole", [userId, role]);
        const member = this.members.get(userId);
        if (!member) throw new ApiError(404, { code: "not_found", message_key: "error.not_found" });
        if (member.role === "admin" && role !== "admin" && this.admins().length === 1) {
          throw conflict("member.last_admin");
        }
        const changed = { ...member, role };
        this.members.set(userId, changed);
        return changed;
      },
      removeMember: async (userId) => {
        await this.enter("removeMember", [userId]);
        const member = this.members.get(userId);
        if (!member) throw new ApiError(404, { code: "not_found", message_key: "error.not_found" });
        if (member.role === "admin" && this.admins().length === 1) throw conflict("member.last_admin");
        this.members.delete(userId);
      },
      previewInvitation: async (token, signal) => {
        await this.enter("previewInvitation", [token, signal]);
        const link = this.links.get(token);
        if (!link || link.kind === "invalid") throw linkError("invalid");
        if (link.kind === "mismatch") throw linkError("mismatch");
        return link.preview;
      },
      acceptInvitation: async (token) => {
        await this.enter("acceptInvitation", [token]);
        const link = this.links.get(token);
        if (!link || link.kind === "invalid") throw linkError("invalid");
        if (link.kind === "mismatch") throw linkError("mismatch");
        // One use only: the link is spent.
        this.links.set(token, { ...link, kind: "invalid" });
        return { tenant_id: this.joinedTenantId };
      },
    };
  }
}

/** The members of the design's pages (docs/design/tenant-admin/members--normal.html), `self` the admin among them. */
export function seedDesignMembers(server: FakeMembersServer): void {
  server.addMember({
    name: "Da-wen Chen",
    email: "dawen.chen@example.com",
    role: "admin",
    joinedAt: "2026-10-14T08:00:00Z",
  });
  server.addMember({
    name: "Xiao-fang Huang",
    email: "xiaofang.huang@example.com",
    role: "editor",
    joinedAt: "2026-10-20T08:00:00Z",
  });
  server.addMember({
    name: null,
    email: "line2-supervisor.with-a-very-long-address@factory-floor.example.com",
    role: "viewer",
    joinedAt: "2026-10-28T08:00:00Z",
  });
  server.addInvitation({ email: "dawei.zhang@example.com", role: "editor" });
  server.addInvitation({ email: "qc-team@example.com", role: "viewer", invitedBy: "Da-wen Chen", expired: true });
}

/** The server the UI preview uses: the preview user is the tenant's admin, the others are fictional. */
function previewServer(seed: "normal" | "empty"): FakeMembersServer {
  const server = new FakeMembersServer({ id: fakeUuid(), name: "Lin Mei", email: PREVIEW_EMAIL });
  if (seed === "normal") seedDesignMembers(server);
  server.addLink("preview-valid", "valid");
  server.addLink("preview-mismatch", "mismatch");
  server.addLink("preview-invalid", "invalid");
  return server;
}

export const preview = {
  name: "members",
  provider: (seed: "normal" | "empty"): ComponentType<{ children: ReactNode }> => {
    // One server for the preview's life. The provider sits outside the app, so it knows the
    // signed-in user only as the identity fake's preview user.
    const api = previewServer(seed).api();
    return function MembersPreviewProvider({ children }) {
      return createElement(MembersApiProvider, { api, children });
    };
  },
};
