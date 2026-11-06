import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Outlet, type RouteObject } from "react-router";

import { ApiError } from "../../api/errors.ts";
import { GatewaysApiProvider } from "../../api/pending/GatewaysApiProvider.tsx";
import { MENU_ITEMS, MenuItemsContext } from "../../app/menu.ts";
import { GATEWAYS_PATH, gatewayBindPath } from "../../app/paths.ts";
import { ROUTES } from "../../app/routes.tsx";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { demoGatewayServer, FakeGatewayServer, PREVIEW_CLAIM } from "../../test/fake-gateways.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { withRoutes } from "../../ui-preview/collect.ts";
import { previewMenu, previewRoutes } from "./preview.ts";

afterEach(() => {
  cleanup();
});

/** The app's routes with the gateway page added as the preview does, and the fake as its API. */
function routesFor(server: FakeGatewayServer): RouteObject[] {
  return [
    {
      element: (
        <MenuItemsContext.Provider value={[...MENU_ITEMS, ...previewMenu]}>
          <GatewaysApiProvider api={server.api()}>
            <Outlet />
          </GatewaysApiProvider>
        </MenuItemsContext.Provider>
      ),
      children: withRoutes(ROUTES, previewRoutes),
    },
  ];
}

/** The design's four gateways on a Business plan; bindings answer at once. */
function fleet(): FakeGatewayServer {
  const server = demoGatewayServer("normal");
  server.bindDelayMs = 0;
  return server;
}

async function open(server: FakeGatewayServer, path = GATEWAYS_PATH, role: "admin" | "editor" | "viewer" = "admin") {
  const { browser } = signedInBrowser({ role });
  return renderAt(path, { routes: routesFor(server), identity: browser });
}

function table(): HTMLElement {
  return screen.getByRole("table");
}

function row(id: string): HTMLElement {
  return within(table()).getByRole("row", { name: new RegExp(id) });
}

async function loaded(server: FakeGatewayServer, path = GATEWAYS_PATH, role: "admin" | "editor" | "viewer" = "admin") {
  const rendered = await open(server, path, role);
  await within(await screen.findByRole("table")).findByRole("button", { name: "tg-0001" });
  return rendered;
}

/**
 * Waits until `query` finds nothing. It compares a boolean: a failed `expect` on a DOM element
 * formats the whole element into the message, which takes seconds on a page this size.
 */
function gone(query: () => HTMLElement | null) {
  return waitFor(() => expect(query() === null).toBe(true));
}

function click(element: HTMLElement) {
  return act(async () => {
    fireEvent.click(element);
  });
}

function type(label: string, value: string) {
  return act(async () => {
    fireEvent.change(screen.getByLabelText(label), { target: { value } });
  });
}

describe("the gateway list", () => {
  test("shows each gateway's presence and why it is offline, last report, configuration, devices, flags and agent", async () => {
    await loaded(fleet());
    expect(screen.getByRole("heading", { level: 1, name: "Gateways" })).toBeDefined();
    expect(
      within(table())
        .getAllByRole("columnheader")
        .map((cell) => cell.textContent),
    ).toEqual(["Gateway", "Status", "Last report", "Applied configuration", "Devices and health", "Agent version"]);

    const online = row("tg-0001");
    expect(within(online).getByText("Online")).toBeDefined();
    expect(within(online).getByText("12 seconds ago")).toBeDefined();
    expect(within(online).getByText("Up to date")).toBeDefined();
    expect(
      within(online).getByRole("link", { name: "3 devices: 1 normal, 1 partly failing, 1 offline" }),
    ).toBeDefined();
    expect(within(online).getByText("1.4.0")).toBeDefined();

    const offline = row("tg-0002");
    expect(within(offline).getByText("Offline: heartbeat timed out")).toBeDefined();
    expect(within(offline).getByText("4 hours ago")).toBeDefined();
    expect(within(offline).getByRole("link", { name: "1 devices: 1 unknown" })).toBeDefined();
    expect(within(offline).getByText("Low power")).toBeDefined();
    expect(within(offline).getByText("Offline buffer 1,240 rows")).toBeDefined();

    const applying = row("tg-0003");
    expect(within(applying).getByText("Applying: version 3 → 4")).toBeDefined();
    expect(within(applying).getByText("Clock not synced")).toBeDefined();
    expect(within(applying).getByText("1.3.2")).toBeDefined();

    const waiting = row("tg-0004");
    expect(within(waiting).getByText("Waiting for first connection")).toBeDefined();
    expect(within(waiting).getByText("No devices")).toBeDefined();
    expect(within(waiting).getAllByText("—")).toHaveLength(3);
  });

  test("a gateway's devices link to the device list filtered by it", async () => {
    await loaded(fleet());
    const link = within(row("tg-0003")).getByRole("link", { name: "6 devices: 6 normal" });
    expect(link.getAttribute("href")).toBe("/devices?gateway=tg-0003");
  });

  test("the summary counts online, offline and waiting gateways, and each is a filter kept in the address", async () => {
    const { router } = await loaded(fleet());
    const summary = screen.getByRole("navigation", { name: "Status summary" });
    expect(within(summary).getByRole("link", { name: "Online 2" })).toBeDefined();
    expect(within(summary).getByRole("link", { name: "Offline 1" })).toBeDefined();

    await click(within(summary).getByRole("link", { name: "Offline 1" }));
    await waitFor(() => expect(router.state.location.search).toBe("?status=offline"));
    expect(within(table()).getAllByRole("row")).toHaveLength(2);
    expect(within(table()).getByRole("button", { name: "tg-0002" })).toBeDefined();

    // The active filter is a toggle.
    await click(within(summary).getByRole("link", { name: "Offline 1" }));
    await waitFor(() => expect(router.state.location.search).toBe(""));
    expect(within(table()).getAllByRole("row")).toHaveLength(5);
  });

  test("a filter that matches nothing says so and offers to show everything", async () => {
    const server = fleet();
    await loaded(server);
    cleanup();
    const only = new FakeGatewayServer();
    only.addGateway("tg-0001");
    await open(only, `${GATEWAYS_PATH}?status=offline`);
    expect(await screen.findByText("No gateway has this status.")).toBeDefined();
    await click(screen.getByRole("button", { name: "Show all gateways" }));
    expect(await screen.findByRole("table")).toBeDefined();
  });

  test("the hardware ID opens the gateway's operating metrics in place, and a #hardware-ID link opens it from the start", async () => {
    await loaded(fleet());
    const toggle = within(table()).getByRole("button", { name: "tg-0002" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    await click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    const details = document.getElementById("gw-tg-0002");
    expect(details).not.toBeNull();
    const panel = within(details as HTMLElement);
    expect(panel.getByText("CPU")).toBeDefined();
    expect(panel.getByRole("img", { name: "CPU 38%" })).toBeDefined();
    expect(panel.getByText("71 °C")).toBeDefined();
    expect(panel.getByText("3 d 6 h")).toBeDefined();
    expect(panel.getByText(/^1,240 rows, oldest/)).toBeDefined();
    expect(panel.getByText("Apr 30, 2027")).toBeDefined();
    await click(toggle);
    expect(document.getElementById("gw-tg-0002")).toBeNull();

    cleanup();
    await open(fleet(), `${GATEWAYS_PATH}#tg-0003`);
    await waitFor(() => expect(document.getElementById("gw-tg-0003")).not.toBeNull());
    expect(document.getElementById("gw-tg-0001")).toBeNull();
    // The table and the phone's list both hold the gateway; the one that is not hidden takes the focus.
    await waitFor(() => expect(["gateway-tg-0003", "gateway-m-tg-0003"]).toContain(document.activeElement?.id ?? ""));
    expect(document.getElementById("gateway-tg-0003")?.getAttribute("data-target")).toBe("true");
  });

  test("a gateway that has not reported has no metrics to show", async () => {
    await loaded(fleet());
    await click(within(table()).getByRole("button", { name: "tg-0004" }));
    expect(
      within(document.getElementById("gw-tg-0004") as HTMLElement).getByText(
        "This gateway has not reported operating metrics yet.",
      ),
    ).toBeDefined();
  });

  test("Refresh now reads the gateways and their health again", async () => {
    const server = fleet();
    await loaded(server);
    const before = server.requests.length;
    await click(screen.getByRole("button", { name: "Refresh now" }));
    await waitFor(() => expect(server.requests.length).toBe(before + 1));
  });
});

describe("loading, failure and a read that fails with data on the screen", () => {
  test("shows one status message while loading", async () => {
    const server = fleet();
    await open(server);
    expect((await screen.findByText("Loading gateways…")).getAttribute("role")).toBe("status");
    await screen.findByRole("table");
  });

  test("a failed first read shows the error with a retry that works", async () => {
    const server = fleet();
    server.failNext("gateways", new Response(null, { status: 503 }));
    await open(server);
    expect(await screen.findByText(/Couldn't load the gateways/)).toBeDefined();
    expect(screen.queryByRole("table")).toBeNull();
    await click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("table")).toBeDefined();
  });

  test("when a later read fails the old data stays, with the time it is from, and retrying clears the notice", async () => {
    const server = fleet();
    await loaded(server);
    server.failNext("gateways", new Response(null, { status: 503 }));
    await click(screen.getByRole("button", { name: "Refresh now" }));
    const notice = await screen.findByText(/Can't refresh right now\. This is the state at \d\d:\d\d\./);
    expect(notice).toBeDefined();
    expect(within(table()).getByRole("button", { name: "tg-0001" })).toBeDefined();
    await click(screen.getByRole("button", { name: "Retry now" }));
    await gone(() => screen.queryByText(/Can't refresh right now/));
  });

  test("when only the health fails the gateways still show, with a retry for the health", async () => {
    const server = fleet();
    server.failHealth(new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    await loaded(server);
    expect(await screen.findByText(/Couldn't load the devices and health/)).toBeDefined();
    expect(within(row("tg-0001")).getAllByText("—").length).toBeGreaterThan(0);
    server.failHealth(null);
    await click(screen.getByRole("button", { name: "Try again" }));
    await gone(() => screen.queryByText(/Couldn't load the devices and health/));
    expect(within(row("tg-0001")).getByText(/3 devices/)).toBeDefined();
  });
});

describe("with no gateway", () => {
  test("an administrator is told what to do and can bind the first gateway", async () => {
    const server = demoGatewayServer("empty");
    server.bindDelayMs = 0;
    await open(server);
    expect(await screen.findByText("No gateway is bound yet")).toBeDefined();
    expect(screen.getByText("You need the hardware ID and the claim code on the gateway's label.")).toBeDefined();
    expect(screen.queryByRole("navigation", { name: "Status summary" })).toBeNull();
    await click(screen.getByRole("button", { name: "Bind your first gateway" }));
    expect(await screen.findByRole("dialog", { name: "Bind a gateway" })).toBeDefined();
  });

  test("a viewer is asked to have an administrator bind one, and has no way to bind", async () => {
    await open(demoGatewayServer("empty"), GATEWAYS_PATH, "viewer");
    expect(
      await screen.findByText(
        "Binding a gateway needs an administrator. Ask an administrator of Acme Factory to bind one, and it will appear here.",
      ),
    ).toBeDefined();
    expect(within(screen.getByRole("main")).queryByRole("button")).toBeNull();
  });
});

describe("roles", () => {
  test("a viewer and an editor see the list and the health, with no bind button, and ?bind opens nothing", async () => {
    for (const role of ["viewer", "editor"] as const) {
      await loaded(fleet(), gatewayBindPath(), role);
      expect(screen.getByRole("heading", { level: 1, name: "Gateways" })).toBeDefined();
      expect(screen.queryByRole("button", { name: "Bind a gateway" })).toBeNull();
      expect(screen.queryByRole("dialog")).toBeNull();
      expect(within(row("tg-0002")).getByText("Low power")).toBeDefined();
      cleanup();
    }
  });

  test("the menu has a Gateways entry that is marked on this page", async () => {
    await loaded(fleet());
    await act(async () => {
      fireEvent.keyDown(screen.getByRole("button", { name: "Menu" }), { key: "Enter" });
    });
    const menu = await screen.findByRole("menu");
    const marked = within(menu)
      .getAllByRole("menuitem")
      .filter((entry) => entry.getAttribute("aria-current") === "page");
    expect(marked.map((entry) => entry.textContent)).toEqual(["Gateways"]);
  });
});

describe("binding a gateway", () => {
  async function openDialog(server: FakeGatewayServer) {
    await loaded(server);
    await click(screen.getByRole("button", { name: "Bind a gateway" }));
    return screen.findByRole("dialog", { name: "Bind a gateway" });
  }

  async function submit() {
    await click(screen.getByRole("button", { name: /^(Bind|Retry binding|Try again in .*)$/ }));
  }

  test("?bind opens the form with the hardware ID field focused, and closing it removes the parameter", async () => {
    const { router } = await open(fleet(), gatewayBindPath());
    const dialog = await screen.findByRole("dialog", { name: "Bind a gateway" });
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByLabelText("Hardware ID")));
    expect(within(dialog).getByText(/belongs only to Acme Factory/)).toBeDefined();
    await click(within(dialog).getByRole("button", { name: "Cancel" }));
    await gone(() => screen.queryByRole("dialog"));
    expect(router.state.location.search).toBe("");
  });

  test("an empty or malformed form is refused before anything is sent, naming the field", async () => {
    const server = fleet();
    const dialog = await openDialog(server);
    await submit();
    expect(within(dialog).getByText("Enter the hardware ID.")).toBeDefined();
    expect(within(dialog).getByLabelText("Hardware ID").getAttribute("aria-invalid")).toBe("true");
    await type("Hardware ID", "not valid!");
    await submit();
    expect(within(dialog).getByText(/A hardware ID has letters, digits/)).toBeDefined();
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await submit();
    expect(within(dialog).getByText("Enter the claim code.")).toBeDefined();
    expect(server.bindings).toHaveLength(0);
  });

  test("a wrong claim code is one rejection that keeps the input, and a correction binds the gateway", async () => {
    const server = fleet();
    const dialog = await openDialog(server);
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await type("Claim code", "WRONG-CODE-0000");
    await submit();
    expect(await within(dialog).findByText("Couldn't bind this gateway")).toBeDefined();
    expect((within(dialog).getByLabelText("Hardware ID") as HTMLInputElement).value).toBe(PREVIEW_CLAIM.hardwareId);
    expect((within(dialog).getByLabelText("Claim code") as HTMLInputElement).value).toBe("WRONG-CODE-0000");
    // A hardware ID that is not in the pool gets the same words.
    await type("Hardware ID", "tg-9999");
    await submit();
    expect(await within(dialog).findByText(/don't match, or this gateway can't be bound right now/)).toBeDefined();

    await type("Hardware ID", ` ${PREVIEW_CLAIM.hardwareId} `);
    await type("Claim code", PREVIEW_CLAIM.claimCode);
    await submit();
    await gone(() => screen.queryByRole("dialog"));
    expect(server.bindings.at(-1)).toEqual({ hardwareId: "tg-0005", claimCode: PREVIEW_CLAIM.claimCode });
  });

  test("after binding, the gateway is first in the list waiting for its first connection, with the next step", async () => {
    const server = fleet();
    await openDialog(server);
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await type("Claim code", PREVIEW_CLAIM.claimCode);
    await submit();
    const notice = await screen.findByText("Bound tg-0005");
    expect(notice).toBeDefined();
    // No link to a page for adding devices: it does not exist yet.
    expect(within(screen.getByRole("main")).queryByRole("link", { name: "Add a device" })).toBeNull();
    await waitFor(() => expect(within(table()).getAllByRole("row")).toHaveLength(6));
    const first = within(table()).getAllByRole("row")[1] as HTMLElement;
    expect(within(first).getByRole("button", { name: "tg-0005" })).toBeDefined();
    expect(within(first).getByText("Waiting for first connection")).toBeDefined();
    // Five of five: the plan's limit is now reached.
    expect(await screen.findByText("The Business plan's gateway limit is reached (5 / 5)")).toBeDefined();
    expect((screen.getByRole("button", { name: "Bind a gateway" }) as HTMLButtonElement).disabled).toBe(true);
    await click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText("Bound tg-0005")).toBeNull();
  });

  test("with no answer the failure says the binding did not finish, keeps the input, and retrying binds once", async () => {
    const server = fleet();
    const dialog = await openDialog(server);
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await type("Claim code", PREVIEW_CLAIM.claimCode);
    server.failNextBind(new ApiError(0, null));
    await submit();
    expect(await within(dialog).findByText("The binding did not finish")).toBeDefined();
    expect(within(dialog).getByText(/Just try again; the gateway is never bound twice/)).toBeDefined();
    expect((within(dialog).getByLabelText("Claim code") as HTMLInputElement).value).toBe(PREVIEW_CLAIM.claimCode);
    await click(within(dialog).getByRole("button", { name: "Retry binding" }));
    await gone(() => screen.queryByRole("dialog"));
    expect(server.bindings).toHaveLength(2);
    expect([...server.gateways.keys()].filter((id) => id === "tg-0005")).toHaveLength(1);
  });

  test("a 503 is treated like no answer", async () => {
    const server = fleet();
    const dialog = await openDialog(server);
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await type("Claim code", PREVIEW_CLAIM.claimCode);
    server.failNextBind(new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    await submit();
    expect(await within(dialog).findByText("The binding did not finish")).toBeDefined();
  });

  test("a rate limit pauses binding with a countdown and says why", async () => {
    const server = fleet();
    const dialog = await openDialog(server);
    await type("Hardware ID", "tg-limited");
    await type("Claim code", "AAAA-BBBB-CCCC");
    await submit();
    expect(await within(dialog).findByText("Too many attempts")).toBeDefined();
    expect(
      within(dialog).getByText(/To stop anyone from guessing hardware IDs, binding is paused\. Try again in 30 s\./),
    ).toBeDefined();
    const wait = within(dialog).getByRole("button", { name: /^Try again in \d+ s$/ }) as HTMLButtonElement;
    expect(wait.disabled).toBe(true);
    // The form cannot be sent meanwhile.
    expect(server.bindings).toHaveLength(1);
    fireEvent.submit(dialog.querySelector("form") as HTMLFormElement);
    expect(server.bindings).toHaveLength(1);
  });

  test("when the plan's limit is reached between loading and binding, the dialog says so", async () => {
    const server = fleet();
    const dialog = await openDialog(server);
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await type("Claim code", PREVIEW_CLAIM.claimCode);
    server.failNextBind(new ApiError(403, { code: "quota_exceeded", message_key: "gateway.quota_exceeded" }));
    await submit();
    expect(await within(dialog).findByText("The gateway limit is reached")).toBeDefined();
  });

  test("Escape closes the form, but not while a binding is on its way", async () => {
    const server = fleet();
    const release = server.holdBinds();
    const dialog = await openDialog(server);
    await type("Hardware ID", PREVIEW_CLAIM.hardwareId);
    await type("Claim code", PREVIEW_CLAIM.claimCode);
    await submit();
    await within(dialog).findByRole("button", { name: "Binding…" });
    await act(async () => {
      fireEvent.keyDown(dialog, { key: "Escape" });
    });
    expect(screen.queryByRole("dialog")).not.toBeNull();
    expect((within(dialog).getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
    await act(async () => {
      release();
    });
    await gone(() => screen.queryByRole("dialog"));
    cleanup();
    const another = await openDialog(fleet());
    await act(async () => {
      fireEvent.keyDown(another, { key: "Escape" });
    });
    await gone(() => screen.queryByRole("dialog"));
  });
});

describe("the plan's gateway limit", () => {
  function fullServer(): FakeGatewayServer {
    const server = fleet();
    server.addGateway("tg-0005", { presence: "online", agent: "1.3.2", devices: { normal: 6 } });
    return server;
  }

  test("a full plan shows the notice, disables the bind button and ties it to the reason; ?bind opens nothing", async () => {
    await open(fullServer(), gatewayBindPath());
    // Not even for a moment while the limit is still being read.
    expect(screen.queryByRole("dialog")).toBeNull();
    await within(await screen.findByRole("table")).findByRole("button", { name: "tg-0001" });
    const notice = await screen.findByText("The Business plan's gateway limit is reached (5 / 5)");
    expect(notice).toBeDefined();
    expect(
      screen.getByText(
        /Existing gateways keep working; you just can't add more\. Enterprise allows a gateway count set by contract\. To upgrade, contact us\./,
      ),
    ).toBeDefined();
    const button = screen.getByRole("button", { name: "Bind a gateway" }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    const reason = document.getElementById(button.getAttribute("aria-describedby") ?? "");
    expect(reason?.textContent).toContain("gateway limit is reached");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("link", { name: "View plan and usage" })).toBeNull();
  });

  test("the Starter plan points to Business, and a limit that is not reached shows no notice", async () => {
    const server = fleet();
    server.setPlan("starter");
    await loaded(server);
    expect(await screen.findByText("The Starter plan's gateway limit is reached (4 / 1)")).toBeDefined();
    expect(screen.getByText(/Business allows up to 5 gateways/)).toBeDefined();
    cleanup();
    const roomy = fleet();
    roomy.setPlan("enterprise", "unlimited");
    await loaded(roomy);
    await gone(() => screen.queryByText(/gateway limit is reached/));
    expect((screen.getByRole("button", { name: "Bind a gateway" }) as HTMLButtonElement).disabled).toBe(false);
  });

  test("a viewer sees the same facts, with no way to bind", async () => {
    await loaded(fullServer(), GATEWAYS_PATH, "viewer");
    expect(await screen.findByText("The Business plan's gateway limit is reached (5 / 5)")).toBeDefined();
    expect(screen.queryByRole("button", { name: "Bind a gateway" })).toBeNull();
  });
});

describe("the empty device list's first step", () => {
  test("links to the binding form when the build has the gateway page, and stays text when it does not", async () => {
    const devices = new FakeDeviceServer();
    const { browser } = signedInBrowser();
    renderAt("/devices", {
      devices: devices.api(),
      identity: browser,
      routes: routesFor(fleet()),
    });
    const steps = await screen.findByRole("list", { name: "Steps to set up devices" });
    const link = within(steps).getByRole("link", { name: "Bind a gateway" });
    expect(link.getAttribute("href")).toBe("/gateways?bind");
    cleanup();

    renderAt("/devices", { devices: devices.api() });
    const without = await screen.findByRole("list", { name: "Steps to set up devices" });
    expect(within(without).queryByRole("link", { name: "Bind a gateway" })).toBeNull();
  });

  test("following the link opens the binding form on the gateway page", async () => {
    const devices = new FakeDeviceServer();
    const { browser } = signedInBrowser();
    const { router } = renderAt("/devices", {
      devices: devices.api(),
      identity: browser,
      routes: routesFor(fleet()),
    });
    const steps = await screen.findByRole("list", { name: "Steps to set up devices" });
    await click(within(steps).getByRole("link", { name: "Bind a gateway" }));
    await waitFor(() => expect(router.state.location.pathname).toBe(GATEWAYS_PATH));
    expect(await screen.findByRole("dialog", { name: "Bind a gateway" })).toBeDefined();
  });
});
