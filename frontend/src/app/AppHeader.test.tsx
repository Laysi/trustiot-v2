import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { fakeEngine } from "../test/fake-chart.ts";
import { FakeDashboardServer } from "../test/fake-dashboards.ts";
import { fakeWithUser, renderAt } from "../test/render.tsx";
import { currentMenuItem, isSecondaryPage, MENU_ITEMS, menuGroups } from "./menu.ts";

declare const happyDOM: { setViewport(viewport: { width: number; height: number }): void };

const WIDE = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };

beforeEach(() => {
  happyDOM.setViewport(WIDE);
});

afterEach(() => {
  cleanup();
  happyDOM.setViewport(WIDE);
});

/** Opens a Radix menu from its trigger the way the keyboard does. */
async function openMenu(name: string) {
  const trigger = await screen.findByRole("button", { name });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  return { trigger, menu: await screen.findByRole("menu") };
}

describe("the top bar", () => {
  test("has the logo, the menu and the account, and no navigation bar", async () => {
    renderAt("/");
    const logo = await screen.findByRole("link", { name: "TrustIoT, back to the conversation home" });
    expect(logo.getAttribute("href")).toBe("/");
    expect(screen.getByRole("button", { name: "Menu" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Account" })).toBeDefined();
    expect(screen.queryByRole("navigation")).toBeNull();
    // The conversation home is not a secondary page: nothing leads back to a conversation from it.
    expect(screen.queryByRole("link", { name: "Back to the conversation" })).toBeNull();
    // The skip link comes before the top bar.
    const skip = screen.getByRole("link", { name: "Skip to main content" });
    expect(skip.compareDocumentPosition(logo) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  test("the menu lists the pages that exist for this user, and marks no page on the home page", async () => {
    renderAt("/");
    const { menu } = await openMenu("Menu");
    const entries = within(menu).getAllByRole("menuitem");
    expect(entries.map((entry) => entry.textContent)).toEqual(["Saved dashboards", "Settings"]);
    expect(entries.map((entry) => entry.getAttribute("href"))).toEqual(["/dashboards", "/settings"]);
    expect(entries.some((entry) => entry.hasAttribute("aria-current"))).toBe(false);
    // Two groups, divided by one line; the account is not in the wide form's menu.
    expect(within(menu).getAllByRole("separator")).toHaveLength(1);
    expect(within(menu).queryByRole("menuitem", { name: "Sign out" })).toBeNull();
  });

  test("the entry of the page the user is on is marked as the current page, and a dashboard's page counts as the list's", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ title: "Energy overview" });
    for (const [path, current] of [
      ["/dashboards", "Saved dashboards"],
      [`/dashboards/${dashboard.id}`, "Saved dashboards"],
      ["/settings", "Settings"],
    ] as const) {
      const { unmount } = renderAt(path, { dashboards: server.api(), chartEngine: fakeEngine().engine });
      const { menu } = await openMenu("Menu");
      const marked = within(menu)
        .getAllByRole("menuitem")
        .filter((entry) => entry.getAttribute("aria-current") === "page");
      expect(marked.map((entry) => entry.textContent)).toEqual([current]);
      unmount();
      cleanup();
    }
  });

  test("a secondary page has the way back to the conversation, which leads to the home page for now", async () => {
    renderAt("/settings");
    const back = await screen.findByRole("link", { name: "Back to the conversation" });
    expect(back.getAttribute("href")).toBe("/");
    expect(back.textContent).toContain("Back to the conversation");
  });

  test("Escape closes the menu and gives the focus back to its button", async () => {
    renderAt("/settings");
    const { trigger, menu } = await openMenu("Menu");
    expect(trigger.getAttribute("aria-haspopup")).toBe("menu");
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    await act(async () => {
      fireEvent.keyDown(menu, { key: "Escape" });
    });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(trigger);
  });

  test("the account menu shows who is signed in, the settings and signing out", async () => {
    renderAt("/");
    const { menu } = await openMenu("Account");
    expect(within(menu).getByText("alice@example.com")).toBeDefined();
    expect(within(menu).getByRole("menuitem", { name: "Account settings" }).getAttribute("href")).toBe("/settings");
    expect(within(menu).getByRole("menuitem", { name: "Sign out" })).toBeDefined();
  });

  test("a platform administrator's account says so", async () => {
    const { server, user, tenants } = fakeWithUser({ platformAdmin: true });
    renderAt("/", { identity: server.browser(server.createSession(user.email, tenants[0] ?? null)) });
    const { menu } = await openMenu("Account");
    expect(within(menu).getByText("Platform administrator")).toBeDefined();
  });
});

describe("the top bar on a phone", () => {
  test("is one row of icon buttons: back, logo, organization, new conversation and menu, with the account in the menu", async () => {
    happyDOM.setViewport(PHONE);
    renderAt("/settings");
    const back = await screen.findByRole("link", { name: "Back to the conversation" });
    expect(back.getAttribute("title")).toBe("Back to the conversation");
    expect(back.textContent).toBe("");
    expect(screen.getByRole("link", { name: "New conversation" }).getAttribute("href")).toBe("/");
    expect(screen.queryByRole("button", { name: "Account" })).toBeNull();

    const { menu } = await openMenu("Menu");
    // The account is the menu's last group.
    const entries = within(menu).getAllByRole("menuitem");
    expect(entries.map((entry) => entry.textContent)).toEqual([
      "Saved dashboards",
      "Settings",
      "Account settings",
      "Sign out",
    ]);
    expect(within(menu).getByText("alice@example.com")).toBeDefined();
  });

  test("a conversation page has no back button", async () => {
    happyDOM.setViewport(PHONE);
    renderAt("/");
    await screen.findByRole("link", { name: "New conversation" });
    expect(screen.queryByRole("link", { name: "Back to the conversation" })).toBeNull();
  });
});

describe("the menu's list of pages", () => {
  const session = (tenants: number) => ({
    user: { id: "u", email: "a@example.test", platformAdmin: false },
    activeTenant: null,
    tenants: Array.from({ length: tenants }, (_, index) => ({
      id: String(index),
      name: `Tenant ${index}`,
      role: "viewer" as const,
      state: "active" as const,
    })),
  });

  test("lists an entry only to a user who may use its page, and drops a group with no entry", () => {
    expect(menuGroups(null)).toEqual([]);
    expect(menuGroups(session(1)).map((group) => group.map((item) => item.id))).toEqual([["dashboards"], ["settings"]]);
    // Someone in no tenant has no dashboards: their group goes, and with it its divider.
    expect(menuGroups(session(0)).map((group) => group.map((item) => item.id))).toEqual([["settings"]]);
  });

  test("every entry is in one group of the design's table and leads to a path of its own", () => {
    expect(new Set(MENU_ITEMS.map((item) => item.path)).size).toBe(MENU_ITEMS.length);
    expect(MENU_ITEMS.map((item) => item.group)).toEqual([1, 4]);
  });

  test("a page is the current entry's only when it is the entry's path or below it", () => {
    expect(currentMenuItem("/dashboards")).toBe("dashboards");
    expect(currentMenuItem("/dashboards/abc")).toBe("dashboards");
    expect(currentMenuItem("/dashboardsx")).toBeNull();
    expect(currentMenuItem("/settings")).toBe("settings");
    expect(currentMenuItem("/")).toBeNull();
    expect(isSecondaryPage("/")).toBe(false);
    expect(isSecondaryPage("/dashboards")).toBe(true);
  });
});
