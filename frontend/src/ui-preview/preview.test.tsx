import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { CpuIcon } from "lucide-react";
import { createContext, type ReactNode, useContext } from "react";
import { createMemoryRouter } from "react-router";

import { createApiClient } from "../api/client.ts";
import { conversationApi } from "../api/conversations.ts";
import { dashboardApi } from "../api/dashboards.ts";
import { deviceApi } from "../api/devices.ts";
import { identityApi } from "../api/identity.ts";
import { App } from "../app/App.tsx";
import type { MenuItem } from "../app/menu.ts";
import { createQueryClient } from "../app/query-client.ts";
import { ChartEngineContext } from "../dashboard/chart/engine.ts";
import { useSession } from "../session/SessionProvider.tsx";
import { fakeEngine } from "../test/fake-chart.ts";
import { lightMedia, memoryPreferences } from "../test/render.tsx";
import type { PreviewSeed } from "./contract.ts";
import { PreviewControls } from "./controls.ts";
import { buildPreview, type PreviewModules } from "./preview.tsx";
import { DEFAULT_SETTINGS, type PreviewSettings } from "./settings.ts";
import { Toolbar } from "./Toolbar.tsx";

afterEach(() => {
  cleanup();
});

/** Six real pages are rendered one after another; a loaded machine needs time, nothing waits on it. */
const PAGES_TIMEOUT_MS = 120_000;

const HERE = dirname(fileURLToPath(import.meta.url));

/**
 * The modules main.tsx's `import.meta.glob` calls load (Vite resolves them; bun test has no
 * `import.meta.glob`): the patterns are read from main.tsx itself and matched against the files
 * here, so these tests see what the preview sees.
 */
async function discovered(): Promise<PreviewModules> {
  const source = readFileSync(join(HERE, "main.tsx"), "utf8");
  const patterns = [...source.matchAll(/import\.meta\.glob\(\s*"([^"]+)"/g)].map((match) => match[1] as string);
  expect(patterns).toHaveLength(2);
  const load = async (pattern: string) => {
    const modules: Record<string, unknown> = {};
    // Bun's Glob has no `{ts,tsx}` braces in the middle of a name the way Vite does: expand them.
    const expanded = pattern.includes("{ts,tsx}")
      ? ["ts", "tsx"].map((ext) => pattern.replace("{ts,tsx}", ext))
      : [pattern];
    for (const one of expanded) {
      for (const file of new Bun.Glob(one).scanSync({ cwd: HERE })) {
        modules[file] = await import(join(HERE, file));
      }
    }
    return modules;
  };
  const [fakes, pages] = patterns as [string, string];
  return { fakes: await load(fakes), pages: await load(pages) };
}

/** The app as the preview starts it (main.tsx), in a memory router at `path`. */
function renderPreview(modules: PreviewModules, path: string, settings: Partial<PreviewSettings> = {}) {
  const controls = new PreviewControls();
  const preview = buildPreview(modules, { ...DEFAULT_SETTINGS, ...settings }, controls);
  const router = createMemoryRouter(preview.routes, { initialEntries: [path] });
  const queryClient = createQueryClient({ onUnauthenticated: () => {} });
  queryClient.setDefaultOptions({ queries: { retry: false }, mutations: { retry: false } });
  const client = createApiClient({ fetch: preview.fetch });
  const app = (
    <App
      router={router}
      deps={{
        queryClient,
        preferences: memoryPreferences({ language: "en" }),
        identity: identityApi(client),
        devices: deviceApi(client),
        dashboards: dashboardApi(client),
        conversations: conversationApi(client),
        languages: ["en-US"],
        browserZone: "UTC",
        matchMedia: lightMedia(),
      }}
    />
  );
  render(<ChartEngineContext.Provider value={fakeEngine().engine}>{preview.wrap(app)}</ChartEngineContext.Provider>);
  return { preview, controls, router };
}

async function openMenu() {
  const trigger = await screen.findByRole("button", { name: "Menu" });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  return screen.findByRole("menu");
}

// An example of what a screen issue adds: a fake of an API that does not exist yet, as a provider,
// and a page directory's preview.ts with the page's route and menu entry. Nothing else.
interface ExampleApi {
  machines(): string[];
}
const ExampleContext = createContext<ExampleApi | null>(null);

function ExamplePage() {
  const api = useContext(ExampleContext);
  const { session } = useSession();
  return (
    <main>
      <h1>Machines</h1>
      <ul>
        {api?.machines().map((machine) => (
          <li key={machine}>{machine}</li>
        ))}
      </ul>
      {session?.activeTenant?.role === "viewer" ? null : <button type="button">Add machine</button>}
    </main>
  );
}

const exampleFake = {
  preview: {
    name: "machines",
    provider: (seed: PreviewSeed) =>
      function MachinesFake({ children }: { children: ReactNode }) {
        const machines = seed === "normal" ? ["Press 1", "Press 2"] : [];
        return <ExampleContext.Provider value={{ machines: () => machines }}>{children}</ExampleContext.Provider>;
      },
  },
};

const exampleMenuItem: MenuItem = {
  id: "machines",
  group: 2,
  label: "shell.menu.devices",
  icon: CpuIcon,
  path: "/machines",
  covers: (pathname) => pathname === "/machines",
  visibleTo: (session) => session.tenants.length > 0,
};

const examplePages = { previewRoutes: [{ path: "machines", Component: ExamplePage }], previewMenu: [exampleMenuItem] };

async function withExample(): Promise<PreviewModules> {
  const modules = await discovered();
  return {
    fakes: { ...modules.fakes, "../test/fake-machines.ts": exampleFake },
    pages: { ...modules.pages, "../pages/machines/preview.ts": examplePages },
  };
}

describe("the UI preview", () => {
  test("loads every fake that declares `preview`, and only those", async () => {
    const { fakes } = await discovered();
    expect(Object.keys(fakes)).toContain("../test/fake-chart.ts");
    const preview = buildPreview(await discovered(), DEFAULT_SETTINGS, new PreviewControls());
    const names = preview.fakes.map((fake) => fake.declaration.name);
    expect(names).toEqual(expect.arrayContaining(["identity", "devices", "dashboards", "conversations"]));
    expect(preview.fakes.some((fake) => fake.file === "../test/fake-chart.ts")).toBe(false);
  });

  test(
    "opens the home page, the conversations, the dashboards, the devices, a device and the settings without a backend",
    async () => {
      const modules = await discovered();
      const cases: [string, RegExp][] = [
        ["/", /Line 2 energy this week/],
        ["/conversations", /Cold room temperature alarms/],
        ["/dashboards", /Plant energy overview/],
        ["/devices", /line-2-meter/],
        ["/settings", /lin\.mei@example\.com/],
      ];
      for (const [path, shown] of cases) {
        renderPreview(modules, path);
        expect((await screen.findAllByText(shown, {}, { timeout: 30_000 })).length).toBeGreaterThan(0);
        cleanup();
      }
      // A device's page, reached from the list as a user would.
      renderPreview(modules, "/devices");
      fireEvent.click((await screen.findAllByRole("link", { name: "cold-room-1" }))[0] as HTMLElement);
      await screen.findByRole("heading", { level: 1, name: "cold-room-1" });
    },
    PAGES_TIMEOUT_MS,
  );

  test(
    "the Shared with me tab and a shared conversation's read-only page work, and a copy lands among the user's own",
    async () => {
      // The conversations fake answers /shared-conversations too (one owner of both prefixes), so
      // the copy of a shared conversation is in the same store the history lists.
      renderPreview(await discovered(), "/conversations?tab=shared");
      fireEvent.click(
        (await screen.findAllByRole("link", { name: "Boiler room energy" }, { timeout: 30_000 }))[0] as HTMLElement,
      );
      await screen.findByRole("heading", { level: 1, name: "Boiler room energy" }, { timeout: 30_000 });
      expect(screen.getByText("Read only")).toBeDefined();
      expect(screen.queryByRole("button", { name: "Rename" })).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "Copy to my conversations" }));
      // The copy is a conversation page of the user's own, with the owner's actions.
      await screen.findByRole("button", { name: "Rename" }, { timeout: 30_000 });
      expect(screen.getByRole("heading", { level: 1, name: "Boiler room energy" })).toBeDefined();
    },
    PAGES_TIMEOUT_MS,
  );

  test("an example declared only in a fake-*.ts and a page's preview.ts is in the routes and the menu", async () => {
    renderPreview(await withExample(), "/machines");
    await screen.findByRole("heading", { level: 1, name: "Machines" });
    expect(screen.getByText("Press 2")).toBeDefined();
    const menu = await openMenu();
    const entry = within(menu)
      .getAllByRole("menuitem")
      .find((item) => item.getAttribute("href") === "/machines");
    expect(entry?.getAttribute("aria-current")).toBe("page");
  });

  test("the empty seed starts every fake without data", async () => {
    renderPreview(await withExample(), "/machines", { seed: "empty" });
    await screen.findByRole("heading", { level: 1, name: "Machines" });
    expect(screen.queryByText("Press 1")).toBeNull();
  });

  test(
    "as a viewer, the actions of editors and admins are gone",
    async () => {
      const modules = await withExample();
      renderPreview(modules, "/machines", { role: "admin" });
      await screen.findByRole("button", { name: "Add machine" });
      cleanup();

      // Each preview creates its fakes again, with new IDs: the dashboard is opened from the list.
      const openDashboard = async () => {
        fireEvent.click(await screen.findByRole("link", { name: "Energy by site" }));
        await screen.findByRole("heading", { level: 1, name: "Energy by site" });
      };
      renderPreview(modules, "/dashboards", { role: "admin" });
      await openDashboard();
      await screen.findByRole("button", { name: "Archive" });
      cleanup();

      renderPreview(modules, "/dashboards", { role: "viewer" });
      await openDashboard();
      await waitFor(() => expect(screen.queryByRole("button", { name: "Archive" })).toBeNull());
      cleanup();

      renderPreview(modules, "/machines", { role: "viewer" });
      await screen.findByRole("heading", { level: 1, name: "Machines" });
      expect(screen.queryByRole("button", { name: "Add machine" })).toBeNull();
    },
    PAGES_TIMEOUT_MS,
  );

  test("a declaration with a mistake names its file", async () => {
    const modules = await discovered();
    const broken = { ...modules, fakes: { ...modules.fakes, "../test/fake-broken.ts": { preview: { name: "x" } } } };
    expect(() => buildPreview(broken, DEFAULT_SETTINGS, new PreviewControls())).toThrow(
      "../test/fake-broken.ts: `preview` needs exactly one of `http` and `provider`",
    );
    const twice = {
      ...modules,
      pages: { "../pages/a/preview.ts": examplePages, "../pages/b/preview.ts": examplePages },
    };
    expect(() => buildPreview(twice, DEFAULT_SETTINGS, new PreviewControls())).toThrow(
      "../pages/b/preview.ts: the menu entry machines is already in ../pages/a/preview.ts",
    );
  });
});

describe("the toolbar", () => {
  test("switching the role saves settings with the new role", () => {
    const saved: PreviewSettings[] = [];
    render(
      <Toolbar
        settings={DEFAULT_SETTINGS}
        controls={new PreviewControls()}
        fakes={["identity"]}
        onSettings={(next) => saved.push(next)}
      />,
    );
    fireEvent.change(screen.getByLabelText("Role"), { target: { value: "viewer" } });
    fireEvent.click(screen.getByLabelText("Platform admin"));
    expect(saved).toEqual([
      { ...DEFAULT_SETTINGS, role: "viewer" },
      { ...DEFAULT_SETTINGS, platformAdmin: true },
    ]);
  });

  test("a role change keeps the slow delay chosen since the page loaded", () => {
    const saved: PreviewSettings[] = [];
    const controls = new PreviewControls();
    render(
      <Toolbar settings={DEFAULT_SETTINGS} controls={controls} fakes={[]} onSettings={(next) => saved.push(next)} />,
    );
    fireEvent.change(screen.getByLabelText("Slow"), { target: { value: "2000" } });
    fireEvent.change(screen.getByLabelText("Role"), { target: { value: "editor" } });
    expect(controls.get().slowMs).toBe(2000);
    expect(saved.at(-1)).toEqual({ ...DEFAULT_SETTINGS, role: "editor", slowMs: 2000 });
  });

  test("a failure armed for the next request is used up by one request", async () => {
    const modules = await discovered();
    const controls = new PreviewControls();
    const preview = buildPreview(modules, DEFAULT_SETTINGS, controls);
    render(<Toolbar settings={DEFAULT_SETTINGS} controls={controls} fakes={[]} onSettings={() => {}} />);
    fireEvent.change(screen.getByLabelText("Fail next request"), { target: { value: "unavailable" } });
    const url = "https://app.example.test/api/assets/v1/devices";
    expect((await preview.fetch(new Request(url))).status).toBe(503);
    expect((await preview.fetch(new Request(url))).status).toBe(200);
    await waitFor(() => expect((screen.getByLabelText("Fail next request") as HTMLSelectElement).value).toBe(""));
    fireEvent.click(screen.getByLabelText("Every request"));
    controls.set({ failNext: "internal" });
    expect((await preview.fetch(new Request(url))).status).toBe(500);
    expect((await preview.fetch(new Request(url))).status).toBe(500);
    controls.set({ failNext: "network", keepFailing: false });
    expect(preview.fetch(new Request(url))).rejects.toThrow(TypeError);
  });

  test("a request no fake answers is 404, and the longest prefix wins", async () => {
    const answered: string[] = [];
    const fake = (name: string, prefixes: string[]) => ({
      preview: {
        name,
        http: {
          prefixes,
          create: () => ({
            fetch: async (request: Request) => {
              answered.push(`${name} ${new URL(request.url).pathname}`);
              return new Response(null, { status: 204 });
            },
          }),
        },
      },
    });
    const preview = buildPreview(
      { fakes: { a: fake("short", ["/api/x/v1"]), b: fake("long", ["/api/x/v1/things"]) }, pages: {} },
      DEFAULT_SETTINGS,
      new PreviewControls(),
    );
    const at = (path: string) => preview.fetch(new Request(`https://app.example.test${path}`));
    await at("/api/x/v1/things/1");
    await at("/api/x/v1/thingsx");
    await at("/api/x/v1");
    expect(answered).toEqual(["long /api/x/v1/things/1", "short /api/x/v1/thingsx", "short /api/x/v1"]);
    const originalWarn = console.warn;
    console.warn = () => {};
    try {
      expect((await at("/api/y/v1/z")).status).toBe(404);
    } finally {
      console.warn = originalWarn;
    }
  });
});
