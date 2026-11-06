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
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { lightMedia, memoryPreferences } from "../../test/render.tsx";
import { PreviewControls } from "../../ui-preview/controls.ts";
import { buildPreview, type PreviewModules } from "../../ui-preview/preview.tsx";
import { DEFAULT_SETTINGS } from "../../ui-preview/settings.ts";
import { PLATFORM_TENANTS_PATH } from "./paths.ts";

allowGenerousTimeouts();

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

/** The UI preview's own assembly (its routes, fakes and providers) at `path`, as a platform administrator. */
function renderPreview(modules: PreviewModules, path: string) {
  const preview = buildPreview(modules, { ...DEFAULT_SETTINGS, platformAdmin: true }, new PreviewControls());
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
  return { router };
}

const EVERY_SECTION = [
  "Tenants",
  "Health and device pool",
  "Provisioning events",
  "Audit log",
  "Model roles",
  "System prompt",
  "Usage and reconciliation",
];

async function sectionLinks(): Promise<{ names: (string | null)[]; current: (string | null)[] }> {
  const nav = await screen.findByRole("navigation", { name: "Platform admin sections" });
  const links = within(nav).getAllByRole("link");
  return {
    names: links.map((link) => link.textContent),
    current: links.filter((link) => link.getAttribute("aria-current") === "page").map((link) => link.textContent),
  };
}

describe("the platform back office in the UI preview", () => {
  test("every page, the AI group's included, sits in the platform layout with every section", async () => {
    const modules = await discovered();
    const cases: [string, string, string][] = [
      // The product's page (its route is in app/routes.tsx, next to TenantGate).
      ["/platform/ai/model-roles", "Model roles", "Model roles"],
      // The preview's pages (pages/platform/preview.ts, under TenantGate).
      [PLATFORM_TENANTS_PATH, "Tenants", "Tenants"],
      ["/platform/ai/prompts", "System prompt", "System prompt"],
      ["/platform/ai/usage", "AI usage and reconciliation", "Usage and reconciliation"],
    ];
    for (const [path, heading, section] of cases) {
      renderPreview(modules, path);
      await screen.findByRole("heading", { level: 1, name: heading });
      expect(await sectionLinks()).toEqual({ names: EVERY_SECTION, current: [section] });
      cleanup();
    }
  });

  test("/platform goes to the first section, the tenants", async () => {
    const { router } = renderPreview(await discovered(), "/platform");
    await screen.findByRole("heading", { level: 1, name: "Tenants" });
    expect(router.state.location.pathname).toBe(PLATFORM_TENANTS_PATH);
  });
});
