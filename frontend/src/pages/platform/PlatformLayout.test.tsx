import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { FakePlatform } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { PLATFORM_AUDIT_PATH, PLATFORM_GATEWAYS_PATH, PLATFORM_TENANTS_PATH, platformTenantPath } from "./paths.ts";
import { currentSection, platformSections } from "./sections.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

describe("the platform layout", () => {
  test("is one region with the cross-tenant notice and the sections, grouped, the current one marked", async () => {
    renderPlatform(PLATFORM_GATEWAYS_PATH, { api: new FakePlatform("empty") });
    const region = await screen.findByRole("region", { name: "Platform admin" });
    expect(within(region).getByText(/what you do here takes effect across tenants/)).toBeDefined();

    const nav = within(region).getByRole("navigation", { name: "Platform admin sections" });
    const links = within(nav)
      .getAllByRole("link")
      .map((link) => link.textContent);
    // The model roles come from the app (its API exists); the other AI sections need their fakes.
    expect(links).toEqual(["Tenants", "Health and device pool", "Provisioning events", "Audit log", "Model roles"]);
    // A group of one section named like the group is not named twice; "Gateways" has two sections.
    expect(
      within(nav)
        .getAllByRole("group")
        .map((group) => group.getAttribute("aria-label")),
    ).toEqual(["Tenants", "Gateways", "Records", "AI"]);
    expect(within(nav).getByRole("link", { name: "Health and device pool" }).getAttribute("aria-current")).toBe("page");
    expect(within(nav).getByRole("link", { name: "Tenants" }).getAttribute("aria-current")).toBeNull();
  });

  test("a tenant's page belongs to the tenants section", async () => {
    const api = new FakePlatform("normal");
    const id = [...api.tenants.values()][0]?.id ?? "";
    renderPlatform(platformTenantPath(id), { api });
    const nav = await screen.findByRole("navigation", { name: "Platform admin sections" });
    expect(within(nav).getByRole("link", { name: "Tenants" }).getAttribute("aria-current")).toBe("page");
  });

  test("a narrow screen's select holds every section with its group and goes to the one chosen", async () => {
    const { router } = renderPlatform(PLATFORM_TENANTS_PATH, { api: new FakePlatform("empty") });
    const select = (await screen.findByRole("combobox", { name: "Platform admin sections" })) as HTMLSelectElement;
    expect(
      within(select)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual([
      "Tenants",
      "Gateways: Health and device pool",
      "Gateways: Provisioning events",
      "Records: Audit log",
      "AI: Model roles",
    ]);
    expect(select.value).toBe(PLATFORM_TENANTS_PATH);
    fireEvent.change(select, { target: { value: PLATFORM_AUDIT_PATH } });
    await waitFor(() => expect(router.state.location.pathname).toBe(PLATFORM_AUDIT_PATH));
  });

  test("/platform goes to the tenants", async () => {
    const { router } = renderPlatform("/platform", { api: new FakePlatform("empty") });
    await screen.findByRole("heading", { level: 1, name: "Tenants" });
    expect(router.state.location.pathname).toBe(PLATFORM_TENANTS_PATH);
  });

  test("only a platform administrator sees the pages", async () => {
    renderPlatform(PLATFORM_AUDIT_PATH, { api: new FakePlatform("normal"), platformAdmin: false });
    expect(
      await screen.findByRole("heading", { name: "You don't have permission to open the platform admin" }),
    ).toBeDefined();
    expect(screen.queryByRole("region", { name: "Platform admin" })).toBeNull();
    expect(screen.getByRole("link", { name: "Go to the home page" })).toBeDefined();
  });

  test("the sections are what the pages plug into: the groups that have an available section, and the one a path is in", () => {
    const grouped = (available: readonly string[]) =>
      platformSections(new Set(available)).map(({ group, sections }) => [
        group.id,
        sections.map((section) => section.id),
      ]);
    expect(grouped(["tenants", "gateways", "provisioning", "audit"])).toEqual([
      ["tenants", ["tenants"]],
      ["gateways", ["gateways", "provisioning"]],
      ["records", ["audit"]],
    ]);
    // The product today: only the model roles have their API, so only the AI group is drawn.
    expect(grouped(["ai-model-roles"])).toEqual([["ai", ["ai-model-roles"]]]);
    expect(grouped([])).toEqual([]);
    expect(currentSection("/platform/tenants/abc")?.id).toBe("tenants");
    expect(currentSection("/platform/audit-log/")?.id).toBe("audit");
    expect(currentSection("/platform/unknown")).toBeNull();
  });
});
