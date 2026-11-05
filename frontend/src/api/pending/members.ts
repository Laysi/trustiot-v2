// Pending Laysi/trustiot-v2#105: replace with a module over the generated client when api/openapi.json has these endpoints.
import type { components } from "../schema.gen.ts";

/**
 * The tenant's member management: members, invitations, the seat quota and accepting an
 * invitation (docs/design/tenant-admin/design.md 「3.1 成員」, Laysi/trustiot-v2#125). The backend
 * (Laysi/trustiot-v2#105, #110) has no endpoints yet, so this is the shape the screens assume;
 * the assumptions are listed in frontend/README.md, section 「成員頁」. Nothing here is served by
 * a real service: the production build never provides a `MembersApi` (pages/members/preview.ts).
 *
 * Errors are `ApiError`s (api/errors.ts) whose `message_key` the page translates. The keys the
 * screens know (all assumed until #105 names them):
 *
 * - `member.last_admin` (409 `conflict`): changing the role of, or removing, the last admin.
 * - `member.already_member` (422 `validation_failed`, field `/email`): the email is a member.
 * - `member.quota_full` (403 `quota_exceeded`): the plan's member limit is reached, counting
 *   pending invitations (design decision 4).
 * - `invitation.invalid` (404 `not_found`): expired, revoked or already accepted, never told apart.
 * - `invitation.account_mismatch` (403 `permission_denied`): another account than the invited one.
 */

export type MemberRole = components["schemas"]["MemberRole"];
export type Plan = components["schemas"]["Plan"];

/** A member of the tenant. */
export interface Member {
  user_id: string;
  /** The display name; `null` for a member who has not set one (the page shows the email). */
  name: string | null;
  email: string;
  role: MemberRole;
  /** RFC 3339. */
  joined_at: string;
}

/** An invitation that has not been accepted or revoked. */
export interface Invitation {
  invitation_id: string;
  email: string;
  role: MemberRole;
  /** Who sent it: a member's name, or their email. */
  invited_by: string;
  /** RFC 3339. */
  created_at: string;
  /** RFC 3339; an invitation past it is listed as expired and can only be sent again. */
  expires_at: string;
}

/** Seats: `used` counts members and pending invitations; `limit` is `null` for a plan without one. */
export interface MemberQuota {
  used: number;
  limit: number | null;
}

/** What `/members` shows. */
export interface MemberList {
  plan: Plan;
  quota: MemberQuota;
  members: Member[];
  invitations: Invitation[];
}

/** What an invitation link leads to, shown before the user accepts. */
export interface InvitationPreview {
  tenant_name: string;
  /** The inviter's name, and their email. */
  invited_by: { name: string | null; email: string };
  role: MemberRole;
  /** The account the invitation is for: the signed-in user's. */
  email: string;
  /** RFC 3339. */
  expires_at: string;
}

export interface MembersApi {
  /** Members, pending invitations and the seat quota; admins only. */
  listMembers(signal?: AbortSignal): Promise<MemberList>;
  /**
   * The names of the tenant's admins, for the page a non-admin sees (decision 1). Any member may
   * read it; it is the only part of the list that is not admin-only.
   */
  listAdmins(signal?: AbortSignal): Promise<string[]>;
  /** Sends an invitation by email. */
  invite(request: { email: string; role: MemberRole }): Promise<Invitation>;
  /** Sends an invitation again with a new expiry (an expired one is invited again). */
  resendInvitation(invitationId: string): Promise<Invitation>;
  /** Revokes a pending invitation. */
  revokeInvitation(invitationId: string): Promise<void>;
  /** Changes a member's role; it applies on the member's next request. */
  changeRole(userId: string, role: MemberRole): Promise<Member>;
  /** Removes a member from the tenant. */
  removeMember(userId: string): Promise<void>;
  /**
   * What the invitation behind `token` is. The token travels in the body of a POST, never in a
   * URL (decision 3).
   */
  previewInvitation(token: string, signal?: AbortSignal): Promise<InvitationPreview>;
  /** Accepts the invitation as the signed-in user; resolves with the tenant they joined. */
  acceptInvitation(token: string): Promise<{ tenant_id: string }>;
}
