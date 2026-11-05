import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { createMemoryRouter } from "react-router";

import { assetsApi } from "../../api/assets.ts";
import { createApiClient } from "../../api/client.ts";
import { conversationApi } from "../../api/conversations.ts";
import { dashboardApi } from "../../api/dashboards.ts";
import { deviceApi } from "../../api/devices.ts";
import { identityApi } from "../../api/identity.ts";
import { App } from "../../app/App.tsx";
import { createQueryClient } from "../../app/query-client.ts";
import { lightMedia, memoryPreferences } from "../../test/render.tsx";
import { PreviewControls } from "../../ui-preview/controls.ts";
import { buildPreview, type PreviewModules } from "../../ui-preview/preview.tsx";
import { DEFAULT_SETTINGS, type PreviewSettings } from "../../ui-preview/settings.ts";
import { dialogClosed, press } from "./test-support.tsx";

const realFetch = window.fetch;

afterEach(() => {
  cleanup();
  window.fetch = realFetch;
});

const HERE = join(dirname(fileURLToPath(import.meta.url)), "../../ui-preview");
const SLOW_MS = 60_000;

/** The modules the preview's `import.meta.glob` calls load, found the way ui-preview/preview.test.tsx does. */
async function discovered(): Promise<PreviewModules> {
  const source = readFileSync(join(HERE, "main.tsx"), "utf8");
  const patterns = [...source.matchAll(/import\.meta\.glob\(\s*"([^"]+)"/g)].map((match) => match[1] as string);
  const load = async (pattern: string) => {
    const modules: Record<string, unknown> = {};
    const expanded = pattern.includes("{ts,tsx}")
      ? ["ts", "tsx"].map((ext) => pattern.replace("{ts,tsx}", ext))
      : [pattern];
    for (const one of expanded) {
      for (const file of new Bun.Glob(one).scanSync({ cwd: HERE })) modules[file] = await import(join(HERE, file));
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
  // As ui-preview/main.tsx does: the machine fake reads the preview's devices through window.fetch.
  window.fetch = ((input: Request | string | URL) =>
    preview.fetch(input instanceof Request ? input : new Request(input))) as typeof window.fetch;
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
          languages: ["en-US"],
          browserZone: "UTC",
          matchMedia: lightMedia(),
        }}
      />,
    ),
  );
  return { preview, router };
}

describe("the machines in the UI preview", () => {
  test(
    "the normal data shows the machines with the preview's own devices, and a changeover works",
    async () => {
      const modules = await discovered();
      const { preview } = renderPreview(modules, "/machines");
      expect(preview.menu.some((item) => item.id === "machines" && item.path === "/machines")).toBe(true);
      const press1 = await screen.findByRole("article", { name: "沖床 A" }, { timeout: 30_000 });
      expect(within(press1).getByText("In production")).toBeTruthy();
      // The devices are the preview's device fake's: the machine fake shared the first ones out.
      expect(await within(press1).findByText(/boiler-1/, {}, { timeout: 30_000 })).toBeTruthy();
      expect(within(screen.getByRole("article", { name: "空壓機 A1" })).getByText("Utility")).toBeTruthy();
      fireEvent.click(within(press1).getByRole("button", { name: "Change over, 沖床 A" }));
      const dialog = await screen.findByRole("dialog");
      fireEvent.change(within(dialog).getByLabelText("New part number"), { target: { value: "BRK-2000" } });
      await press(within(dialog).getByRole("button", { name: "Confirm changeover" }));
      await dialogClosed();
      expect(await screen.findByText(/Changed over: BRK-1023 → BRK-2000/)).toBeTruthy();
    },
    SLOW_MS,
  );

  test(
    "the empty data shows the first-machine state, and a viewer gets no add button",
    async () => {
      const modules = await discovered();
      renderPreview(modules, "/machines", { seed: "empty", role: "viewer" });
      expect(await screen.findByText("Add your first machine", {}, { timeout: 30_000 })).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Add machine" })).toBeNull();
      cleanup();
      renderPreview(modules, "/machines", { seed: "empty", role: "editor" });
      await screen.findByText("Add your first machine", {}, { timeout: 30_000 });
      await act(async () => {});
      expect(screen.getByRole("button", { name: "Add machine" })).toBeTruthy();
    },
    SLOW_MS,
  );
});
