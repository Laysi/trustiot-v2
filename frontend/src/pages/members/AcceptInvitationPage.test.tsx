import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../api/errors.ts";
import { fakeUuid } from "../../test/fake-devices.ts";
import type { FakeMembersServer } from "../../test/fake-members.ts";
import { renderMembers } from "../../test/render-members.tsx";
import { INVITATION_ACCEPT_PATH } from "./paths.ts";

afterEach(() => {
  cleanup();
});

// Built at run time: a fake credential is never written as a literal.
const TOKEN = `tok-${crypto.randomUUID()}`;
const LINK = `${INVITATION_ACCEPT_PATH}#${TOKEN}`;

function open(kind: "valid" | "invalid" | "mismatch", setup?: (server: FakeMembersServer) => void) {
  return renderMembers(LINK, {
    setup: (server) => {
      server.addLink(TOKEN, kind);
      setup?.(server);
    },
  });
}

describe("accepting an invitation", () => {
  test("says what the user would join, as whom and with which role, and takes the token out of the address", async () => {
    const { router } = open("valid");
    expect(await screen.findByRole("heading", { level: 1, name: "Join Acme Precision Works" })).toBeDefined();
    expect(screen.getByText("Mei Lin (mei.lin@example.com)")).toBeDefined();
    expect(screen.getByText(/^Editor: Can also add and change devices/)).toBeDefined();
    expect(screen.getByText("alice@example.com")).toBeDefined();
    // The fragment is gone from the address, and nothing replaced it in a query.
    await waitFor(() => expect(router.state.location.hash).toBe(""));
    expect(router.state.location.search).toBe("");
    expect(router.state.location.pathname).toBe(INVITATION_ACCEPT_PATH);
  });

  test("the token travels only in the call's argument: previewing and accepting send it, the page keeps no copy in the URL", async () => {
    const { server, router } = open("valid");
    fireEvent.click(await screen.findByRole("button", { name: "Accept and join" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    const calls = server.calls.filter(
      (call) => call.operation === "previewInvitation" || call.operation === "acceptInvitation",
    );
    expect(calls.map((call) => [call.operation, call.args[0]])).toEqual([
      ["previewInvitation", TOKEN],
      ["acceptInvitation", TOKEN],
    ]);
    expect(router.state.location.hash).toBe("");
    // Joined: the session works in the organization now.
    expect(await screen.findByRole("heading", { level: 1 })).toBeDefined();
  });

  test("a link to another invitation followed while the page is open starts over with the new token", async () => {
    const other = `tok-${crypto.randomUUID()}`;
    const { server, router } = open("valid", (s) => s.addLink(other, "invalid"));
    await screen.findByRole("heading", { level: 1, name: "Join Acme Precision Works" });
    await act(async () => {
      await router.navigate(`${INVITATION_ACCEPT_PATH}#${other}`);
    });
    expect(await screen.findByRole("heading", { level: 1, name: "This invitation can't be used" })).toBeDefined();
    expect(server.calls.filter((call) => call.operation === "previewInvitation").map((call) => call.args[0])).toEqual([
      TOKEN,
      other,
    ]);
    await waitFor(() => expect(router.state.location.hash).toBe(""));
  });

  test("an invitation can be accepted only once: the link is spent afterwards", async () => {
    const { server } = open("valid");
    fireEvent.click(await screen.findByRole("button", { name: "Accept and join" }));
    await waitFor(() => expect(server.calls.some((call) => call.operation === "acceptInvitation")).toBe(true));
    // Opening the same link again is the page every unusable invitation gets.
    cleanup();
    const again = renderMembers(LINK, { setup: (s) => s.addLink(TOKEN, "invalid") });
    expect(await screen.findByRole("heading", { level: 1, name: "This invitation can't be used" })).toBeDefined();
    expect(again.server.calls.some((call) => call.operation === "acceptInvitation")).toBe(false);
  });

  test("an expired, revoked or used invitation all show the same page", async () => {
    open("invalid");
    expect(await screen.findByRole("heading", { level: 1, name: "This invitation can't be used" })).toBeDefined();
    expect(screen.getByText(/expired, been revoked, or been accepted already/)).toBeDefined();
    expect(screen.queryByRole("button", { name: "Accept and join" })).toBeNull();
  });

  test("a link without a token is that page too, and nothing is asked of the server", async () => {
    const { server } = renderMembers(INVITATION_ACCEPT_PATH);
    expect(await screen.findByRole("heading", { level: 1, name: "This invitation can't be used" })).toBeDefined();
    expect(server.calls).toEqual([]);
  });

  test("another account: no details of the organization, and a way to sign out", async () => {
    open("mismatch");
    expect(
      await screen.findByRole("heading", { level: 1, name: "This invitation wasn't sent to this account" }),
    ).toBeDefined();
    expect(screen.getByText(/You're signed in as alice@example.com/)).toBeDefined();
    expect(screen.queryByText(/Acme Precision Works/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Sign out and use another account" }));
    await screen.findByRole("heading", { name: /sign in/i });
  });

  test("a server that cannot be reached is not mistaken for an invalid invitation, and trying again works", async () => {
    const { server } = open("valid", (s) =>
      s.failNext("previewInvitation", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" })),
    );
    expect(await screen.findByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Join Acme Precision Works" })).toBeDefined();
    expect(server.calls.filter((call) => call.operation === "previewInvitation")).toHaveLength(2);
  });

  test("accepted but the organization cannot be opened: says so with the name and a way home, not a spinner", async () => {
    const { server } = open("valid", (s) => {
      // A tenant the session does not know: switching to it is refused.
      s.joinedTenantId = fakeUuid();
    });
    fireEvent.click(await screen.findByRole("button", { name: "Accept and join" }));
    expect(await screen.findByRole("heading", { level: 1, name: "You joined Acme Precision Works" })).toBeDefined();
    expect(screen.getByRole("link", { name: "Go to the home page" }).getAttribute("href")).toBe("/");
    expect(screen.queryByText("Joining…")).toBeNull();
    expect(server.calls.filter((call) => call.operation === "acceptInvitation")).toHaveLength(1);
  });

  test("a refusal when accepting is shown and the invitation can be accepted again", async () => {
    const { server } = open("valid", (s) =>
      s.failNext("acceptInvitation", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" })),
    );
    fireEvent.click(await screen.findByRole("button", { name: "Accept and join" }));
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Accept and join" }));
    await waitFor(() => expect(server.calls.filter((call) => call.operation === "acceptInvitation")).toHaveLength(2));
  });
});
