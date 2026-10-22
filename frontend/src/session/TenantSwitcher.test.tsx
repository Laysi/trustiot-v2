import { afterEach, describe, expect, test } from "bun:test";
import { QueryClient } from "@tanstack/react-query";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import { ROUTES } from "../app/routes.tsx";
import { memoryPreferences, renderAt } from "../test/render.tsx";
import type { SessionInfo, SessionSource } from "./session.ts";

afterEach(() => {
  cleanup();
});

const USER = { id: "01928f6e-7a3c-7d4e-8f00-0123456789ab", displayName: "Dev User" };
const TENANTS = [
  { id: "t-1", name: "Acme Factory", role: "admin" },
  { id: "t-2", name: "Beta Plant", role: "viewer" },
];

function fakeSource(initial: SessionInfo) {
  const switched: string[] = [];
  let current = initial;
  const source: SessionSource = {
    load: async () => current,
    switchTenant: async (tenantId) => {
      switched.push(tenantId);
      const tenant = current.tenants.find((item) => item.id === tenantId);
      if (!tenant) throw new Error("not a member");
      current = { ...current, activeTenant: { id: tenant.id, name: tenant.name } };
      return current;
    },
  };
  return { source, switched };
}

describe("TenantSwitcher", () => {
  test("shows the active tenant's name and puts it in the tab title", async () => {
    const { source } = fakeSource({
      user: USER,
      activeTenant: { id: "t-1", name: "Acme Factory" },
      tenants: [TENANTS[0] as (typeof TENANTS)[0]],
    });
    renderAt("/", { routes: ROUTES, session: source });
    expect(await screen.findByText("Acme Factory")).toBeDefined();
    await waitFor(() => expect(document.title).toBe("Acme Factory · TrustIoT"));
  });

  test("switching tenants drops every cached query and shows the new tenant", async () => {
    const { source, switched } = fakeSource({
      user: USER,
      activeTenant: { id: "t-1", name: "Acme Factory" },
      tenants: TENANTS,
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    queryClient.setQueryData(["devices", "t-1"], ["a device of Acme"]);
    renderAt("/", { routes: ROUTES, session: source, queryClient });
    const select = (await screen.findByLabelText("Organization")) as HTMLSelectElement;
    expect(select.value).toBe("t-1");

    await act(async () => {
      fireEvent.change(select, { target: { value: "t-2" } });
    });
    await waitFor(() => expect(document.title).toBe("Beta Plant · TrustIoT"));
    expect(switched).toEqual(["t-2"]);
    expect(queryClient.getQueryData(["devices", "t-1"])).toBeUndefined();
    expect((screen.getByLabelText("Organization") as HTMLSelectElement).value).toBe("t-2");
  });

  test("a failed switch says so and keeps the current tenant", async () => {
    const { source } = fakeSource({ user: USER, activeTenant: { id: "t-1", name: "Acme Factory" }, tenants: TENANTS });
    const failing: SessionSource = {
      load: source.load,
      switchTenant: async () => {
        throw new Error("503");
      },
    };
    renderAt("/", { routes: ROUTES, session: failing });
    const select = (await screen.findByLabelText("Organization")) as HTMLSelectElement;
    await act(async () => {
      fireEvent.change(select, { target: { value: "t-2" } });
    });
    expect(await screen.findByRole("alert")).toBeDefined();
    expect(screen.getByRole("alert").textContent).toBe("Could not switch organization. Try again.");
    expect((screen.getByLabelText("Organization") as HTMLSelectElement).value).toBe("t-1");
  });

  test("without a session nothing is shown and the title is the app's", async () => {
    renderAt("/", { routes: ROUTES });
    await screen.findByRole("heading", { name: "Home" });
    expect(screen.queryByLabelText("Organization")).toBeNull();
    await waitFor(() => expect(document.title).toBe("TrustIoT"));
  });
});

describe("PreferencesMenu", () => {
  test("changing the language, time zone and theme applies at once and is saved", async () => {
    const preferences = memoryPreferences({ locale: "en", timeZone: "auto", theme: "system" });
    renderAt("/", { routes: ROUTES, preferences });
    await screen.findByRole("heading", { name: "Home" });

    await act(async () => {
      fireEvent.change(screen.getByLabelText("Theme"), { target: { value: "dark" } });
    });
    expect(document.documentElement.dataset.theme).toBe("dark");

    await act(async () => {
      fireEvent.change(screen.getByLabelText("Time zone"), { target: { value: "Asia/Taipei" } });
    });
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Language"), { target: { value: "zh-TW" } });
    });
    expect(await screen.findByRole("heading", { name: "首頁" })).toBeDefined();
    expect(document.documentElement.lang).toBe("zh-TW");
    expect(preferences.saved.at(-1)).toEqual({ locale: "zh-TW", timeZone: "Asia/Taipei", theme: "dark" });
  });
});
