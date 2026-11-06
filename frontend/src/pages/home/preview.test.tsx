import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { cleanup, render, screen, within } from "@testing-library/react";
import { createMemoryRouter } from "react-router";
import { assetsApi } from "../../api/assets.ts";
import { createApiClient } from "../../api/client.ts";
import { conversationApi } from "../../api/conversations.ts";
import { dashboardApi } from "../../api/dashboards.ts";
import { deviceApi } from "../../api/devices.ts";
import { identityApi } from "../../api/identity.ts";
import { modelRoleApi } from "../../api/modelRoles.ts";
import { App } from "../../app/App.tsx";
import { createQueryClient } from "../../app/query-client.ts";
import { lightMedia, memoryPreferences } from "../../test/render.tsx";
import { PreviewControls } from "../../ui-preview/controls.ts";
import { buildPreview, type PreviewModules } from "../../ui-preview/preview.tsx";
import { DEFAULT_SETTINGS, type PreviewSettings } from "../../ui-preview/settings.ts";

afterEach(() => {
  cleanup();
});

const UI_PREVIEW = join(dirname(fileURLToPath(import.meta.url)), "../../ui-preview");

/** The modules the preview's `import.meta.glob` calls load, found by the patterns in its main.tsx (bun has no glob import). */
async function discovered(): Promise<PreviewModules> {
  const source = readFileSync(join(UI_PREVIEW, "main.tsx"), "utf8");
  const patterns = [...source.matchAll(/import\.meta\.glob\(\s*"([^"]+)"/g)].map((match) => match[1] as string);
  const load = async (pattern: string) => {
    const modules: Record<string, unknown> = {};
    const expanded = pattern.includes("{ts,tsx}")
      ? ["ts", "tsx"].map((ext) => pattern.replace("{ts,tsx}", ext))
      : [pattern];
    for (const one of expanded) {
      for (const file of new Bun.Glob(one).scanSync({ cwd: UI_PREVIEW })) {
        modules[file] = await import(join(UI_PREVIEW, file));
      }
    }
    return modules;
  };
  const [fakes, pages] = patterns as [string, string];
  return { fakes: await load(fakes), pages: await load(pages) };
}

function renderPreview(modules: PreviewModules, path: string, settings: Partial<PreviewSettings> = {}) {
  const preview = buildPreview(modules, { ...DEFAULT_SETTINGS, ...settings }, new PreviewControls());
  const router = createMemoryRouter(preview.routes, { initialEntries: [path] });
  const queryClient = createQueryClient({ onUnauthenticated: () => {} });
  queryClient.setDefaultOptions({ queries: { retry: false }, mutations: { retry: false } });
  const client = createApiClient({ fetch: preview.fetch });
  render(
    preview.wrap(
      <App
        router={router}
        deps={{
          queryClient,
          preferences: memoryPreferences({ language: "en" }),
          identity: identityApi(client),
          devices: deviceApi(client),
          assets: assetsApi(client),
          dashboards: dashboardApi(client),
          conversations: conversationApi(client),
          modelRoles: modelRoleApi(client),
          languages: ["en-US"],
          browserZone: "UTC",
          matchMedia: lightMedia(),
        }}
      />,
    ),
  );
}

const LOADED_TIMEOUT_MS = 30_000;

describe("the setup checklist and the generic home in the UI preview", () => {
  test("both fakes are loaded, and the overview page is a preview route with no menu entry", async () => {
    const modules = await discovered();
    const preview = buildPreview(modules, DEFAULT_SETTINGS, new PreviewControls());
    expect(preview.fakes.map((fake) => fake.declaration.name)).toEqual(
      expect.arrayContaining(["Setup checklist", "Generic home"]),
    );
    expect(Object.keys(modules.pages)).toContain("../pages/home/preview.ts");
  });

  test("the home page of a tenant midway shows the checklist with the demo data", async () => {
    renderPreview(await discovered(), "/");
    const region = await screen.findByRole(
      "region",
      { name: "Get started with TrustIoT" },
      { timeout: LOADED_TIMEOUT_MS },
    );
    expect(within(region).getByText(/2 of 5 steps done/)).toBeTruthy();
    expect(within(region).getByText(/Waiting for the first reading/)).toBeTruthy();
    // The conversation home is still there under it.
    expect(screen.getByRole("heading", { level: 1 })).toBeTruthy();
  });

  test("the empty seed is a new tenant, and a viewer (a role the session gives) sees no checklist", async () => {
    const modules = await discovered();
    renderPreview(modules, "/", { seed: "empty" });
    const region = await screen.findByRole(
      "region",
      { name: "Get started with TrustIoT" },
      { timeout: LOADED_TIMEOUT_MS },
    );
    expect(within(region).getByText(/0 of 5 steps done/)).toBeTruthy();
    cleanup();
    renderPreview(modules, "/", { role: "viewer" });
    await screen.findByRole("heading", { level: 1 }, { timeout: LOADED_TIMEOUT_MS });
    expect(screen.queryByRole("region", { name: "Get started with TrustIoT" })).toBeNull();
  });

  test("/overview shows the demo tenant's counts and devices, and the empty seed a tenant with no gateway", async () => {
    const modules = await discovered();
    renderPreview(modules, "/overview");
    expect(
      await screen.findByRole("heading", { level: 1, name: "Overview" }, { timeout: LOADED_TIMEOUT_MS }),
    ).toBeTruthy();
    expect(await screen.findByRole("table", {}, { timeout: LOADED_TIMEOUT_MS })).toBeTruthy();
    expect(screen.getAllByText("Boiler B1").length).toBeGreaterThan(0);
    cleanup();
    renderPreview(modules, "/overview", { seed: "empty" });
    expect(await screen.findByText("No gateway yet", {}, { timeout: LOADED_TIMEOUT_MS })).toBeTruthy();
  });
});
