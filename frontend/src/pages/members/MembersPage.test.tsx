import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../api/errors.ts";
import { MENU_ITEMS } from "../../app/menu.ts";
import type { SessionInfo } from "../../session/session.ts";
import { seedDesignMembers } from "../../test/fake-members.ts";
import { renderMembers } from "../../test/render-members.tsx";
import { MEMBERS_PATH } from "./paths.ts";
import { previewMenu } from "./preview.ts";

afterEach(() => {
  cleanup();
});

/** The design's normal state: the admin, three more members and two pending invitations (one expired). */
const normal = { setup: seedDesignMembers };

async function open(path = MEMBERS_PATH, options: Parameters<typeof renderMembers>[1] = normal) {
  const rendered = renderMembers(path, options);
  await screen.findByRole("heading", { level: 1, name: "Members" });
  return rendered;
}

/** The wide form of a list (the phone's list holds the same rows); `hidden` because an open dialog hides the page. */
const memberTable = () => screen.getByRole("table", { name: "Members of Acme Factory", hidden: true });
const inviteTable = () => screen.getByRole("table", { name: "Pending invitations", hidden: true });

async function openRowMenu(name: string) {
  const trigger = within(memberTable()).getByRole("button", { name: `Manage ${name}'s membership` });
  fireEvent.keyDown(trigger, { key: "Enter" });
  return screen.findByRole("menu");
}

describe("the member list", () => {
  test("lists members with role and join date, marks the signed-in one, and shows seats and invitations", async () => {
    await open();
    const table = await screen.findByRole("table", { name: "Members of Acme Factory" });

    const alice = within(table).getByRole("row", { name: /Alice Admin/ });
    expect(within(alice).getByText("You")).toBeDefined();
    expect(within(alice).getByText("Administrator")).toBeDefined();
    expect(within(alice).getByText("alice@example.com")).toBeDefined();
    const fang = within(table).getByRole("row", { name: /Xiao-fang Huang/ });
    expect(within(fang).getByText("Editor")).toBeDefined();
    expect(within(fang).queryByText("You")).toBeNull();
    // A member without a name shows the email once, long as it is.
    expect(
      within(table).getAllByText("line2-supervisor.with-a-very-long-address@factory-floor.example.com"),
    ).toHaveLength(1);

    // 4 members + 2 invitations of 10 seats.
    expect(screen.getByText("6 / 10")).toBeDefined();
    expect(screen.getByText("Members (Starter plan)")).toBeDefined();
    expect(screen.getByRole("img", { name: "6 of 10 seats used" })).toBeDefined();

    const invitations = inviteTable();
    expect(within(invitations).getByText("dawei.zhang@example.com")).toBeDefined();
    const expired = within(invitations).getByRole("row", { name: /qc-team@example.com/ });
    expect(within(expired).getByText(/\(expired\)/)).toBeDefined();
    expect(within(expired).getByRole("button", { name: "Invite again" })).toBeDefined();
    expect(within(invitations).getByRole("button", { name: "Send again" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Invite a member" })).toHaveProperty("disabled", false);
    expect(screen.getByText("What each role can do")).toBeDefined();
  });

  test("shows a skeleton and a disabled invite button while it loads", async () => {
    const { server } = renderMembers(MEMBERS_PATH, {
      setup: (s) => {
        s.hold("listMembers");
      },
    });
    await screen.findByRole("heading", { level: 1, name: "Members" });
    expect(await screen.findByText("Loading members…")).toBeDefined();
    expect(screen.getByRole("button", { name: "Invite a member" })).toHaveProperty("disabled", true);
    expect(server.calls.map((call) => call.operation)).toEqual(["listMembers"]);
  });

  test("says why it failed and tries again on request", async () => {
    const { server } = renderMembers(MEMBERS_PATH, {
      setup: (s) =>
        s.failNext("listMembers", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" })),
    });
    await screen.findByRole("heading", { level: 1, name: "Members" });
    expect(await screen.findByText(/Couldn't load the members: The service is temporarily unavailable/)).toBeDefined();
    expect(screen.getByRole("button", { name: "Invite a member" })).toHaveProperty("disabled", true);

    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await within(await screen.findByRole("table", { name: "Members of Acme Factory" })).findByText("Alice Admin");
    expect(server.calls.filter((call) => call.operation === "listMembers")).toHaveLength(2);
  });

  test("a tenant with only the signed-in member suggests the first invitation", async () => {
    await open(MEMBERS_PATH, {});
    expect(await screen.findByText("You're the only one in this organization")).toBeDefined();
    expect(screen.queryByRole("table", { name: "Pending invitations" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Invite the first member" }));
    expect(await screen.findByRole("alertdialog", { name: "Invite a member" })).toBeDefined();
  });
});

describe("who may open the page", () => {
  test("a viewer is told who the administrators are and the member list is not even requested", async () => {
    const { server } = renderMembers(MEMBERS_PATH, {
      role: "viewer",
      setup: (s) => s.addMember({ name: "Da-wen Chen", email: "dawen.chen@example.com", role: "admin" }),
    });
    await screen.findByRole("heading", { level: 1, name: "Only administrators can manage members" });
    expect(await screen.findByText(/Alice Admin, Da-wen Chen/)).toBeDefined();
    expect(screen.getByText(/Your role in Acme Factory is Viewer/)).toBeDefined();
    expect(screen.queryByRole("button", { name: "Invite a member" })).toBeNull();
    expect(server.calls.map((call) => call.operation)).not.toContain("listMembers");
  });

  test("an editor gets the same page", async () => {
    renderMembers(MEMBERS_PATH, { role: "editor" });
    await screen.findByRole("heading", { level: 1, name: "Only administrators can manage members" });
    expect(screen.getByText(/Your role in Acme Factory is Editor/)).toBeDefined();
  });

  test("the menu entry is for administrators only and the entry is not in the product's menu", () => {
    const session = (role: "viewer" | "editor" | "admin"): SessionInfo => {
      const tenant = { id: "t1", name: "Acme", role, state: "active" as const };
      return {
        user: { id: "u1", email: "a@example.com", platformAdmin: false },
        activeTenant: tenant,
        tenants: [tenant],
      };
    };
    const [entry] = previewMenu;
    expect(entry?.visibleTo(session("admin"))).toBe(true);
    expect(entry?.visibleTo(session("editor"))).toBe(false);
    expect(entry?.visibleTo(session("viewer"))).toBe(false);
    expect(entry?.covers(MEMBERS_PATH)).toBe(true);
    expect(MENU_ITEMS.some((item) => item.id === "members")).toBe(false);
  });
});

describe("inviting", () => {
  test("sends an invitation, announces it, marks the new row and gives the focus back to the button", async () => {
    const { server } = await open();
    const button = screen.getByRole("button", { name: "Invite a member" });
    fireEvent.click(button);
    const dialog = await screen.findByRole("alertdialog", { name: "Invite a member" });
    // The first field has the focus, not Cancel.
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByLabelText("Email")));

    fireEvent.change(within(dialog).getByLabelText("Email"), { target: { value: " xiaoling.wu@example.com " } });
    fireEvent.click(within(dialog).getByRole("radio", { name: /Editor/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Send invitation" }));

    expect(await screen.findByText("Invitation sent to xiaoling.wu@example.com")).toBeDefined();
    expect(screen.getByRole("status").textContent).toContain("They appear in the member list once they accept.");
    expect(screen.queryByRole("alertdialog")).toBeNull();
    const row = within(inviteTable()).getByRole("row", { name: /xiaoling.wu@example.com/ });
    expect(within(row).getByText("Editor")).toBeDefined();
    expect(row.className).toContain("bg-brand-soft");
    expect(await screen.findByText("7 / 10")).toBeDefined();
    expect(server.calls.find((call) => call.operation === "invite")?.args).toEqual([
      { email: "xiaoling.wu@example.com", role: "editor" },
    ]);
    await waitFor(() => expect(document.activeElement).toBe(button));
  });

  test("an email that is plainly not one is refused in the form before anything is sent", async () => {
    const { server } = await open();
    fireEvent.click(screen.getByRole("button", { name: "Invite a member" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.change(within(dialog).getByLabelText("Email"), { target: { value: "not-an-email" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Send invitation" }));
    expect(await within(dialog).findByText("Enter a valid email address.")).toBeDefined();
    expect(within(dialog).getByLabelText("Email").getAttribute("aria-invalid")).toBe("true");
    expect(server.calls.map((call) => call.operation)).not.toContain("invite");
  });

  test("the server's field error lands under the email, takes the focus, and the dialog keeps what was typed", async () => {
    await open();
    fireEvent.click(screen.getByRole("button", { name: "Invite a member" }));
    const dialog = await screen.findByRole("alertdialog");
    const email = within(dialog).getByLabelText("Email");
    fireEvent.change(email, { target: { value: "xiaofang.huang@example.com" } });
    fireEvent.click(within(dialog).getByRole("radio", { name: /Administrator/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Send invitation" }));

    expect(await within(dialog).findByText("This email is already a member.")).toBeDefined();
    expect(email.getAttribute("aria-invalid")).toBe("true");
    await waitFor(() => expect(document.activeElement).toBe(email));
    expect((email as HTMLInputElement).value).toBe("xiaofang.huang@example.com");
    expect((within(dialog).getByRole("radio", { name: /Administrator/ }) as HTMLInputElement).checked).toBe(true);
  });

  test("a link to the dialog followed while the page is open opens it too", async () => {
    const { router } = await open();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    await act(async () => {
      await router.navigate(`${MEMBERS_PATH}?invite`);
    });
    expect(await screen.findByRole("alertdialog", { name: "Invite a member" })).toBeDefined();
  });

  test("the address can open the dialog, and closing it takes the parameter out", async () => {
    const { router } = await open(`${MEMBERS_PATH}?invite`);
    const dialog = await screen.findByRole("alertdialog", { name: "Invite a member" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(router.state.location.search).toBe("");
  });
});

describe("the plan's member limit", () => {
  const full = {
    setup(server: Parameters<NonNullable<Parameters<typeof renderMembers>[1]>["setup"] & object>[0]) {
      seedDesignMembers(server);
      server.limit = 6;
    },
  };

  test("shows the upgrade notice, disables inviting with the reason, and links to the plan", async () => {
    await open(MEMBERS_PATH, full);
    const notice = await screen.findByText("The Starter plan's member limit is reached (6 / 6)");
    expect(notice).toBeDefined();
    expect(screen.getByText(/Existing members keep working/)).toBeDefined();
    const button = screen.getByRole("button", { name: "Invite a member" });
    expect(button).toHaveProperty("disabled", true);
    expect(button.getAttribute("aria-describedby")).toBe("quota-note");
    expect(document.getElementById("quota-note")?.textContent).toContain("member limit is reached");
    expect(screen.getByRole("link", { name: "See plan and usage" }).getAttribute("href")).toBe("/settings");
    // Existing members still have their actions.
    expect(within(memberTable()).getAllByRole("button", { name: /^Manage / })).toHaveLength(4);
    // ?invite does not open a dialog that could not send.
    cleanup();
    await open(`${MEMBERS_PATH}?invite`, full);
    await screen.findByText("The Starter plan's member limit is reached (6 / 6)");
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  test("when the limit was reached since the page was read, the refusal is shown and the notice appears", async () => {
    const { server } = await open();
    expect(screen.queryByText(/member limit is reached \(/)).toBeNull();
    server.limit = 6;
    fireEvent.click(screen.getByRole("button", { name: "Invite a member" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.change(within(dialog).getByLabelText("Email"), { target: { value: "late@example.com" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Send invitation" }));
    expect(await within(dialog).findByText(/Your plan's member limit is reached/)).toBeDefined();
    expect(await screen.findByText("The Starter plan's member limit is reached (6 / 6)")).toBeDefined();
  });
});

describe("changing a role", () => {
  test("opens from the row menu, applies the choice and announces it", async () => {
    const { server } = await open();
    const menu = await openRowMenu("Xiao-fang Huang");
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Change role…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Change Xiao-fang Huang's role" });
    expect((within(dialog).getByRole("radio", { name: /Editor/ }) as HTMLInputElement).checked).toBe(true);
    // Nothing to save until the choice changes.
    expect(within(dialog).getByRole("button", { name: "Save role" })).toHaveProperty("disabled", true);

    fireEvent.click(within(dialog).getByRole("radio", { name: /Administrator/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Save role" }));

    expect(await screen.findByText("Xiao-fang Huang is now Administrator")).toBeDefined();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    const row = within(memberTable()).getByRole("row", { name: /Xiao-fang Huang/ });
    await waitFor(() => expect(within(row).getByText("Administrator")).toBeDefined());
    expect(server.calls.find((call) => call.operation === "changeRole")?.args).toEqual([expect.any(String), "admin"]);
  });

  test("the last administrator cannot be lowered: the server's code is translated in the dialog, which stays open", async () => {
    await open(MEMBERS_PATH, {});
    const menu = await openRowMenu("Alice Admin");
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Change role…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Change your role, Alice Admin" });

    fireEvent.click(within(dialog).getByRole("radio", { name: /Viewer/ }));
    // Lowering oneself warns first.
    expect(within(dialog).getByText("You will lose the right to manage members")).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Save role" }));

    expect(await within(dialog).findByText("The role could not be changed")).toBeDefined();
    expect(within(dialog).getByText(/needs at least one administrator/)).toBeDefined();
    expect(within(dialog).getByText("Error code member.last_admin")).toBeDefined();
    expect(screen.getByRole("alertdialog")).toBeDefined();
    // The page still shows the administrator.
    expect(within(memberTable()).getByText("Administrator")).toBeDefined();
  });

  test("with a second administrator the first can lower themselves", async () => {
    await open(MEMBERS_PATH, {
      setup: (s) => s.addMember({ name: "Da-wen Chen", email: "dawen.chen@example.com", role: "admin" }),
    });
    const menu = await openRowMenu("Alice Admin");
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Change role…" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("radio", { name: /Editor/ }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Save role" }));
    expect(await screen.findByText("Alice Admin is now Editor")).toBeDefined();
  });
});

describe("removing a member", () => {
  test("asks first with the focus on Cancel, names the member in the button, and removes only on confirmation", async () => {
    const { server } = await open();
    const menu = await openRowMenu("Xiao-fang Huang");
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Remove from organization…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Remove Xiao-fang Huang from Acme Factory?" });
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" })));
    expect(within(dialog).getByText("xiaofang.huang@example.com")).toBeDefined();
    expect(server.calls.map((call) => call.operation)).not.toContain("removeMember");

    fireEvent.click(within(dialog).getByRole("button", { name: "Remove Xiao-fang Huang" }));
    expect(await screen.findByText("Removed Xiao-fang Huang")).toBeDefined();
    await waitFor(() => expect(within(memberTable()).queryByText("Xiao-fang Huang")).toBeNull());
    expect(screen.getByText("5 / 10")).toBeDefined();
    // The focus goes to the member card's heading.
    await waitFor(() => expect(document.activeElement?.textContent).toBe("Members"));
  });

  test("a refusal is shown in the dialog, which stays open", async () => {
    const { server } = await open();
    const menu = await openRowMenu("Xiao-fang Huang");
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Remove from organization…" }));
    const dialog = await screen.findByRole("alertdialog");
    server.failNext(
      "removeMember",
      new ApiError(403, { code: "permission_denied", message_key: "error.permission_denied" }),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Remove Xiao-fang Huang" }));
    expect(await within(dialog).findByText("You don't have permission to do this.")).toBeDefined();
    expect(within(memberTable()).getByText("Xiao-fang Huang")).toBeDefined();
  });

  test("the signed-in member's own row offers no removal", async () => {
    await open();
    const menu = await openRowMenu("Alice Admin");
    expect(within(menu).getByRole("menuitem", { name: "Change role…" })).toBeDefined();
    expect(within(menu).queryByRole("menuitem", { name: "Remove from organization…" })).toBeNull();
  });
});

describe("pending invitations", () => {
  test("sending one again renews it and an expired one is invited again", async () => {
    const { server } = await open();
    const row = within(inviteTable()).getByRole("row", { name: /qc-team@example.com/ });
    fireEvent.click(within(row).getByRole("button", { name: "Invite again" }));
    expect(await screen.findByText("Invitation sent again to qc-team@example.com")).toBeDefined();
    await waitFor(() =>
      expect(
        within(within(inviteTable()).getByRole("row", { name: /qc-team@example.com/ })).queryByText(/expired/),
      ).toBeNull(),
    );
    expect(server.calls.some((call) => call.operation === "resendInvitation")).toBe(true);
  });

  test("revoking one frees the seat", async () => {
    await open();
    fireEvent.click(
      within(inviteTable()).getByRole("button", { name: "Revoke the invitation to dawei.zhang@example.com" }),
    );
    expect(await screen.findByText("Revoked the invitation to dawei.zhang@example.com")).toBeDefined();
    await waitFor(() => expect(screen.queryByText("dawei.zhang@example.com")).toBeNull());
    expect(screen.getByText("5 / 10")).toBeDefined();
  });

  test("a failure is shown above the lists and can be dismissed", async () => {
    const { server } = await open();
    server.failNext("revokeInvitation", new ApiError(404, { code: "not_found", message_key: "error.not_found" }));
    fireEvent.click(
      within(inviteTable()).getByRole("button", { name: "Revoke the invitation to dawei.zhang@example.com" }),
    );
    expect(await screen.findByText("We couldn't find what you were looking for.")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("We couldn't find what you were looking for.")).toBeNull();
  });
});
