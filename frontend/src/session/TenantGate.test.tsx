import { afterEach, describe, expect, test } from "bun:test";
import { useQuery } from "@tanstack/react-query";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { RouteObject } from "react-router";

import { Layout } from "../app/Layout.tsx";
import { ErrorPage } from "../pages/ErrorPage.tsx";
import { LoginPage } from "../pages/LoginPage.tsx";
import { HomePage } from "../pages/pages.tsx";
import { tenantSuspended, unauthenticated, unavailable } from "../test/fake-identity.ts";
import { fakeWithUser, renderAt } from "../test/render.tsx";
import { TenantGate } from "./TenantGate.tsx";

afterEach(() => {
  cleanup();
});

/** A session in no tenant yet, as after a first sign-in. */
function inNoTenant(tenants: readonly (string | null)[]) {
  const fake = fakeWithUser({ tenants });
  return { ...fake, browser: fake.server.browser(fake.server.createSession(fake.user.email, null)) };
}

describe("TenantGate", () => {
  test("with several tenants the user picks one, and the page opens in it", async () => {
    const { server, browser, tenants } = inNoTenant(["Acme Factory", "Beta Plant"]);
    renderAt("/", { identity: browser });
    expect(await screen.findByRole("heading", { name: "Choose an organization" })).toBeDefined();
    const list = screen.getByRole("list", { name: "Your organizations" });
    expect(
      within(list)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Acme FactoryAdministrator", "Beta PlantAdministrator"]);
    await act(async () => {
      fireEvent.click(within(list).getByRole("button", { name: /Beta Plant/ }));
    });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    expect(server.requests.filter((request) => request.operation === "switchTenant")).toEqual([
      { operation: "switchTenant", input: tenants[1]?.id },
    ]);
    await waitFor(() => expect(document.title).toBe("Beta Plant · TrustIoT"));
  });

  test("with exactly one tenant the session switches to it without asking", async () => {
    const { server, browser, tenants } = inNoTenant(["Acme Factory"]);
    renderAt("/", { identity: browser });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    expect(screen.queryByRole("heading", { name: "Choose an organization" })).toBeNull();
    expect(server.requests.filter((request) => request.operation === "switchTenant")).toEqual([
      { operation: "switchTenant", input: tenants[0]?.id },
    ]);
  });

  test("a failed automatic switch says so and can be tried again", async () => {
    const { server, browser } = inNoTenant(["Acme Factory"]);
    server.failNext("switchTenant", unavailable());
    renderAt("/", { identity: browser });
    expect(await screen.findByRole("heading", { name: "Could not open Acme Factory." })).toBeDefined();
    expect(screen.getByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    expect(server.operations().filter((operation) => operation === "switchTenant")).toHaveLength(2);
  });

  test("with no tenant it explains why there is nothing to show, and checks again on request", async () => {
    const { server, browser, user } = inNoTenant([]);
    renderAt("/", { identity: browser });
    expect(await screen.findByRole("heading", { name: "No organization yet" })).toBeDefined();
    expect(screen.getByText(/isn't a member of any organization yet/)).toBeDefined();
    expect(screen.getByRole("button", { name: "Sign out" })).toBeDefined();

    // An administrator adds the user; checking again finds the tenant and opens it.
    const tenant = server.addTenant("Acme Factory");
    user.memberships.set(tenant.id, "viewer");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
  });

  test("the settings page opens without a tenant", async () => {
    const { server, browser } = inNoTenant(["Acme Factory", "Beta Plant"]);
    renderAt("/settings", { identity: browser });
    expect(await screen.findByRole("heading", { name: "Settings" })).toBeDefined();
    expect(server.operations()).not.toContain("switchTenant");
  });

  test("the no-tenant page is translated like every other", async () => {
    const { browser } = inNoTenant([]);
    renderAt("/", { identity: browser, languages: ["zh-TW"] });
    // The account's language is "auto", which follows the browser's languages.
    expect(await screen.findByRole("heading", { name: "還沒有組織" })).toBeDefined();
  });
});

/** A page of the active tenant that reads its data, which the API refuses once the tenant is suspended. */
function tenantRoutes(load: () => Promise<string>): RouteObject[] {
  function Devices() {
    const query = useQuery({ queryKey: ["devices"], queryFn: load });
    return <p>{query.data ?? "loading devices"}</p>;
  }
  return [
    { path: "/login", element: <LoginPage /> },
    {
      path: "/",
      element: <Layout />,
      errorElement: <ErrorPage />,
      children: [
        {
          element: <TenantGate />,
          children: [
            { index: true, element: <HomePage /> },
            { path: "devices", element: <Devices /> },
          ],
        },
      ],
    },
  ];
}

describe("a suspended tenant", () => {
  test("replaces the tenant's pages, names the tenant, and lets the user continue in another", async () => {
    const fake = fakeWithUser({ tenants: ["Acme Factory", "Beta Plant"] });
    const [acme, beta] = fake.tenants;
    const browser = fake.server.browser(fake.server.createSession(fake.user.email, acme ?? null));
    let suspended = true;
    const load = async () => {
      if (suspended) throw tenantSuspended();
      return "devices of Beta Plant";
    };
    renderAt("/devices", { identity: browser, routes: tenantRoutes(load) });

    expect(await screen.findByRole("heading", { name: "Organization suspended" })).toBeDefined();
    expect(
      screen.getByText(
        "Acme Factory has been suspended, so its data and devices can't be used right now. Contact your organization's administrator.",
      ),
    ).toBeDefined();
    const others = screen.getByRole("list", { name: "Your organizations" });
    expect(
      within(others)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Beta PlantAdministrator"]);

    suspended = false;
    await act(async () => {
      fireEvent.click(within(others).getByRole("button", { name: /Beta Plant/ }));
    });
    expect(await screen.findByText("devices of Beta Plant")).toBeDefined();
    expect(fake.server.requests.filter((request) => request.operation === "switchTenant")).toEqual([
      { operation: "switchTenant", input: beta?.id },
    ]);
  });

  test("with no other tenant it offers only to sign out", async () => {
    const fake = fakeWithUser({ tenants: ["Acme Factory"] });
    const browser = fake.server.browser(fake.server.createSession(fake.user.email, fake.tenants[0] ?? null));
    renderAt("/devices", {
      identity: browser,
      routes: tenantRoutes(async () => {
        throw tenantSuspended();
      }),
    });
    expect(await screen.findByRole("heading", { name: "Organization suspended" })).toBeDefined();
    expect(screen.queryByRole("list", { name: "Your organizations" })).toBeNull();
    expect(screen.getByRole("button", { name: "Sign out" })).toBeDefined();
  });

  test("a refused tenant switch is the switch's error, not a suspension", async () => {
    const fake = fakeWithUser({ tenants: ["Acme Factory", "Beta Plant"] });
    const browser = fake.server.browser(fake.server.createSession(fake.user.email, null));
    fake.server.failNext("switchTenant", tenantSuspended());
    renderAt("/", { identity: browser });
    const list = await screen.findByRole("list", { name: "Your organizations" });
    await act(async () => {
      fireEvent.click(within(list).getByRole("button", { name: /Beta Plant/ }));
    });
    expect(await screen.findByText("Could not switch organization. Try again.")).toBeDefined();
    expect(screen.getByText("This organization has been suspended.")).toBeDefined();
    expect(screen.getByRole("heading", { name: "Choose an organization" })).toBeDefined();
  });
});

describe("a session that ended elsewhere", () => {
  test("sends the user to sign in, back to the page, and forgets what was cached for them", async () => {
    const fake = fakeWithUser({ tenants: ["Acme Factory"] });
    const browser = fake.server.browser(fake.server.createSession(fake.user.email, fake.tenants[0] ?? null));
    let ended = false;
    const { router, queryClient } = renderAt("/devices", {
      identity: browser,
      routes: tenantRoutes(async () => {
        if (ended) throw unauthenticated();
        return "devices of Acme Factory";
      }),
    });
    expect(await screen.findByText("devices of Acme Factory")).toBeDefined();

    // Signed out on another device: the next request of this page is refused.
    ended = true;
    await act(async () => {
      await queryClient.refetchQueries({ queryKey: ["devices"] });
    });
    expect(await screen.findByRole("heading", { name: "Sign in to TrustIoT" })).toBeDefined();
    expect(`${router.state.location.pathname}${router.state.location.search}`).toBe("/login?next=%2Fdevices");
    expect(queryClient.getQueryData(["devices"])).toBeUndefined();
    expect(queryClient.getQueryData(["session"])).toBeNull();

    // Signed out now, a language chosen here stays in this browser instead of going to the server.
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Language"), { target: { value: "zh-TW" } });
    });
    expect(await screen.findByRole("heading", { name: "登入 TrustIoT" })).toBeDefined();
    expect(fake.server.operations()).not.toContain("updatePreferences");
  });
});
