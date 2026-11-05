import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../api/errors.ts";
import { FakePlatform } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { PLATFORM_GATEWAYS_PATH } from "./paths.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

async function open(api: FakePlatform) {
  const rendered = renderPlatform(PLATFORM_GATEWAYS_PATH, { api });
  await screen.findByRole("heading", { level: 1, name: "Health and device pool" });
  await screen.findByRole("table");
  return rendered;
}

function row(id: string): HTMLElement {
  return within(screen.getByRole("table")).getByRole("row", { name: new RegExp(id) });
}

async function menuOf(id: string) {
  fireEvent.keyDown(screen.getByRole("button", { name: `Actions for ${id}` }), { key: "Enter" });
  return screen.findByRole("menu");
}

describe("the health overview and the device pool", () => {
  test("search and filters say they only see the loaded pages, until the rest is loaded", async () => {
    const api = new FakePlatform("normal");
    api.pageSize = 2;
    await open(api);
    expect(screen.queryByText(/Search and filters look only at the/)).toBeNull();
    fireEvent.change(screen.getByLabelText("Hardware ID or tenant"), { target: { value: "TIG-24A0-0031" } });
    expect(
      screen.getByText("Search and filters look only at the 2 gateways loaded so far. Load more to include the rest."),
    ).toBeDefined();
    expect(screen.getByText("No gateway matches")).toBeDefined();
    for (let page = 0; page < 5 && screen.queryByRole("button", { name: "Load more" }); page++) {
      fireEvent.click(screen.getByRole("button", { name: "Load more" }));
      await waitFor(() => expect(screen.queryByRole("button", { name: "Loading…" }) === null).toBe(true));
    }
    await screen.findByRole("table");
    expect(within(screen.getByRole("table")).getByText("TIG-24A0-0031")).toBeDefined();
    expect(screen.queryByText(/Search and filters look only at the/)).toBeNull();
  });

  test("counts the gateways by health, and a tile applies its filter", async () => {
    await open(new FakePlatform("normal"));
    expect(screen.getByText("4 bound")).toBeDefined();
    expect(screen.getByText("1 in the pool not counted")).toBeDefined();

    fireEvent.click(screen.getByRole("button", { name: /^Offline/ }));
    const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(2);
    fireEvent.click(screen.getByRole("button", { name: /^All/ }));
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(7);
  });

  test("shows where each gateway is: its tenant, the pool, or repair, with health and backlog", async () => {
    await open(new FakePlatform("normal"));
    const degraded = row("TIG-24A0-0011");
    expect(within(degraded).getByText("Degraded")).toBeDefined();
    expect(within(degraded).getByText("4,210 rows buffered")).toBeDefined();
    expect(within(degraded).getByRole("link", { name: "Demo Tenant A" })).toBeDefined();
    const processing = row("TIG-24A0-0023");
    expect(within(processing).getByText("Tenant is being suspended")).toBeDefined();
    const pooled = row("TIG-24A0-0031");
    expect(within(pooled).getByText("Device pool")).toBeDefined();
    expect(within(pooled).getByText("— (unbound gateways don't report)")).toBeDefined();
    expect(within(pooled).getByRole("button", { name: "Bind TIG-24A0-0031 to a tenant…" })).toBeDefined();
    expect(within(row("TIG-24A0-0002")).getAllByText("In repair").length).toBeGreaterThan(0);
  });

  test("says 'Unknown', never 'Offline', when the health cannot be read", async () => {
    const api = new FakePlatform("normal");
    api.healthUnavailable = true;
    await open(api);
    expect(screen.getByText("Health data can't be read right now")).toBeDefined();
    expect(within(row("TIG-24A0-0007")).getByText("Unknown")).toBeDefined();
    expect(within(row("TIG-24A0-0007")).queryByText("Offline")).toBeNull();
  });

  test("an empty pool points to the provisioning events", async () => {
    renderPlatform(PLATFORM_GATEWAYS_PATH, { api: new FakePlatform("empty") });
    expect(await screen.findByRole("heading", { name: "The device pool has no gateways waiting" })).toBeDefined();
    expect(screen.getByRole("link", { name: "See the provisioning events" }).getAttribute("href")).toBe(
      "/platform/provisioning",
    );
  });

  test("a failure has a retry, and a suspended current tenant gets its own explanation", async () => {
    const api = new FakePlatform("normal");
    api.failNext("listGateways");
    renderPlatform(PLATFORM_GATEWAYS_PATH, { api });
    expect(await screen.findByText("The gateways can't be loaded right now")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("table");
    cleanup();

    const refused = new FakePlatform("normal");
    refused.failNext(
      "listGateways",
      new ApiError(403, { code: "tenant_suspended", message_key: "error.tenant_suspended" }),
    );
    renderPlatform(PLATFORM_GATEWAYS_PATH, { api: refused });
    expect(await screen.findByText("Your current tenant is suspended")).toBeDefined();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
  });

  test("binding shows the tenants' quota, refuses a full tenant, and the gateway then belongs to the tenant", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    fireEvent.click(within(row("TIG-24A0-0031")).getByRole("button", { name: "Bind TIG-24A0-0031 to a tenant…" }));
    const dialog = await screen.findByRole("alertdialog");
    const select = await within(dialog).findByRole("combobox", { name: "Tenant" });
    const options = within(select)
      .getAllByRole("option")
      .map((option) => option.textContent);
    // A suspended tenant is not offered; the others show how many gateways they have of the limit.
    expect(options).toContain("Demo Tenant A (3 / 5 gateways)");
    expect(options.some((text) => text?.includes("Nangang"))).toBe(false);

    const full = [...api.tenants.values()].find((tenant) => tenant.name === "Acme Labs Test");
    fireEvent.change(select, { target: { value: full?.id } });
    expect(within(dialog).getByText("The gateway quota of “Acme Labs Test” is full (1 / 1)")).toBeDefined();
    expect((within(dialog).getByRole("button", { name: "Bind to Acme Labs Test" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(within(dialog).getByRole("link", { name: "Open the tenant to adjust the quota" })).toBeDefined();

    const demo = [...api.tenants.values()].find((tenant) => tenant.name === "Demo Tenant A");
    fireEvent.change(select, { target: { value: demo?.id } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Bind to Demo Tenant A" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    expect(screen.getByText("TIG-24A0-0031 was bound to Demo Tenant A.")).toBeDefined();
    await waitFor(() =>
      expect(within(row("TIG-24A0-0031")).getByRole("link", { name: "Demo Tenant A" })).toBeDefined(),
    );
    expect(
      api.events.some((event) => event.type === "assets.gateway_bound" && event.aggregateId === "TIG-24A0-0031"),
    ).toBe(true);
  });

  test("a gateway another tenant holds is refused in the dialog (409)", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    fireEvent.click(within(row("TIG-24A0-0031")).getByRole("button", { name: "Bind TIG-24A0-0031 to a tenant…" }));
    const dialog = await screen.findByRole("alertdialog");
    await within(dialog).findByRole("combobox", { name: "Tenant" });
    api.failNext(
      "bindGateway",
      new ApiError(409, { code: "conflict", message_key: "gateway_binding.bound_to_another_tenant" }),
    );
    fireEvent.click(within(dialog).getByRole("button", { name: /^Bind to / }));
    expect(await within(dialog).findByText("This gateway is bound to another tenant. Unbind it first.")).toBeDefined();
    expect(screen.getByRole("alertdialog")).toBeDefined();
  });

  test("unbinding asks for the last 4 characters of the hardware ID and says what it costs", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    fireEvent.click(within(await menuOf("TIG-24A0-0011")).getByRole("menuitem", { name: "Unbind…" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(
      within(dialog).getByText(/the 4,210 buffered rows that were not uploaded yet may never reach the old tenant/),
    ).toBeDefined();
    expect(within(dialog).getByText(/Always fit a new SD card/)).toBeDefined();
    const confirm = within(dialog).getByRole("button", { name: "Unbind" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText(/last 4 characters/), { target: { value: "0011" } });
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(within(row("TIG-24A0-0011")).getByText("Device pool")).toBeDefined());
  });

  test("sending in for repair needs a note and the last 4 characters", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    fireEvent.click(within(await menuOf("TIG-24A0-0007")).getByRole("menuitem", { name: "Send for repair…" }));
    const dialog = await screen.findByRole("alertdialog");
    const confirm = within(dialog).getByRole("button", { name: "Unbind and send for repair" }) as HTMLButtonElement;
    fireEvent.change(within(dialog).getByLabelText(/last 4 characters/), { target: { value: "0007" } });
    expect(confirm.disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText("Repair note"), { target: { value: "Power module fault" } });
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(within(row("TIG-24A0-0007")).getAllByText("In repair").length).toBeGreaterThan(0));
  });

  test("revoking the certificates needs the whole hardware ID and warns about key compromise", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    fireEvent.click(within(await menuOf("TIG-24A0-0011")).getByRole("menuitem", { name: "Revoke certificates…" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.change(within(dialog).getByLabelText("Reason"), { target: { value: "key_compromise" } });
    expect(within(dialog).getByText(/can never get a certificate again/)).toBeDefined();
    const confirm = within(dialog).getByRole("button", { name: "Revoke all certificates" }) as HTMLButtonElement;
    fireEvent.change(within(dialog).getByLabelText(/whole hardware ID/), { target: { value: "0011" } });
    expect(confirm.disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText(/whole hardware ID/), { target: { value: "TIG-24A0-0011" } });
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    const event = api.events.find((entry) => entry.type === "certs.gateway_certificates_revoked");
    expect(event).toBeDefined();
    expect((event as { payload: { reason: string } }).payload.reason).toBe("key_compromise");
  });

  test("decommissioning needs the whole ID and a ticked box, and the gateway leaves the list", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    fireEvent.click(within(await menuOf("TIG-24A0-0002")).getByRole("menuitem", { name: "Decommission…" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("This can't be undone")).toBeDefined();
    const confirm = within(dialog).getByRole("button", { name: "Decommission" }) as HTMLButtonElement;
    fireEvent.change(within(dialog).getByLabelText(/whole hardware ID/), { target: { value: "TIG-24A0-0002" } });
    expect(confirm.disabled).toBe(true);
    fireEvent.click(
      within(dialog).getByRole("checkbox", { name: "I understand that decommissioning can't be undone" }),
    );
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(within(screen.getByRole("table")).queryByText("TIG-24A0-0002") === null).toBe(true));
  });

  test("the row menu keeps the irreversible actions last, and a pooled gateway has the bind button and the irreversible ones", async () => {
    await open(new FakePlatform("normal"));
    const menu = await menuOf("TIG-24A0-0011");
    expect(
      within(menu)
        .getAllByRole("menuitem")
        .map((item) => item.textContent),
    ).toEqual(["Unbind…", "Send for repair…", "Revoke certificates…", "Decommission…"]);
    expect(within(row("TIG-24A0-0031")).getByRole("button", { name: /^Bind/ })).toBeDefined();
    const pooled = await menuOf("TIG-24A0-0031");
    expect(
      within(pooled)
        .getAllByRole("menuitem")
        .map((item) => item.textContent),
    ).toEqual(["Revoke certificates…", "Decommission…"]);
  });
});
