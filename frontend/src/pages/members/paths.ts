/** The member management page, admins only; `?invite` opens its invitation dialog (design.md 「5. 路由與深層連結」). */
export const MEMBERS_PATH = "/members";

/** The query parameter that opens the invitation dialog. */
export const INVITE_PARAM = "invite";

/**
 * Accepting an invitation. The token is the URL fragment (`#<token>`), never a query parameter
 * (design decision 3): a fragment is not sent to the server, so it stays out of access logs and
 * Referer headers, and the page removes it from the address as soon as it has read it.
 */
export const INVITATION_ACCEPT_PATH = "/invitations/accept";
