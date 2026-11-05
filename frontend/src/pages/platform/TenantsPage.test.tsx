import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { FakePlatform, unavailable } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { PLATFORM_TENANTS_PATH } from "./paths.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

async function open(api: FakePlatform, path = PLATFORM_TENANTS_PATH) {
  const rendered = renderPlatform(path, { api });
  await screen.findByRole("heading", { level: 1, name: "Tenants" });
  return rendered;
}

function row(name: string): HTMLElement {
  return within(screen.getByRole("table")).getByRole("row", { name: new RegExp(name) });
}

async function menuOf(name: string) {
  fireEvent.keyDown(screen.getByRole("button", { name: `Actions for ${name}` }), { key: "Enter" });
  return screen.findByRole("menu");
}

describe("the tenant list", () => {
  test("lists every tenant with plan, status, gateways, members and the month's AI spend", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(7);

    const demo = row("Demo Tenant A");
    expect(within(demo).getByText("Business")).toBeDefined();
    expect(within(demo).getByText("Active")).toBeDefined();
    expect(within(demo).getByText("3 / 5")).toBeDefined();
    expect(within(demo).getByText("$12.40 / $70.00")).toBeDefined();

    // A suspension certs has not confirmed is "Processing" with the time it has waited, never "Suspended".
    const processing = row("North Plant Demo");
    expect(within(processing).getByText("Processing")).toBeDefined();
    expect(within(processing).getAllByText(/Waited 2 min/).length).toBeGreaterThan(0);

    const suspended = row("Nangang Test Tenant");
    expect(within(suspended).getByText("Suspended")).toBeDefined();
    expect(within(suspended).getByText("Non-payment")).toBeDefined();
    const final = row("Old Contract Demo");
    expect(within(final).getByText("Suspended, final")).toBeDefined();
    expect(within(final).getByText("Contract terminated, can't be resumed")).toBeDefined();

    expect(within(row("Acme Labs Test")).getByText("Overdrawn")).toBeDefined();
  });

  test("filters by search, status and plan, says when nothing matches and clears the filters", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");
    fireEvent.change(screen.getByRole("searchbox", { name: "Search" }), { target: { value: "acme" } });
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2);

    fireEvent.change(screen.getByRole("searchbox", { name: "Search" }), { target: { value: "" } });
    fireEvent.change(screen.getByRole("combobox", { name: "Status" }), { target: { value: "suspended" } });
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(3);
    fireEvent.change(screen.getByRole("combobox", { name: "Plan" }), { target: { value: "enterprise" } });
    expect(screen.getByRole("heading", { name: "No tenant matches" })).toBeDefined();

    fireEvent.click(screen.getByRole("button", { name: "Clear filters" }));
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(7);
  });

  test("an empty platform offers to create the first tenant", async () => {
    await open(new FakePlatform("empty"));
    expect(await screen.findByRole("heading", { name: "There are no tenants yet" })).toBeDefined();
    const link = screen.getByRole("link", { name: "Create the first tenant" });
    expect(link.getAttribute("href")).toBe("/platform/tenants/new");
  });

  test("shows rows while loading, and an error with a retry that reads the list again", async () => {
    const api = new FakePlatform("normal");
    api.hold("listTenants");
    renderPlatform(PLATFORM_TENANTS_PATH, { api });
    expect(await screen.findByText("Loading tenants…")).toBeDefined();
    api.release("listTenants");
    await screen.findByRole("table");
    cleanup();

    const failing = new FakePlatform("normal");
    failing.failNext("listTenants", unavailable());
    renderPlatform(PLATFORM_TENANTS_PATH, { api: failing });
    expect(await screen.findByText("The tenant list can't be loaded right now")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("table");
  });

  test("the row menu offers suspend for an active tenant, resume for a suspended one and neither once the contract ended", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");

    let menu = await menuOf("Demo Tenant A");
    expect(within(menu).getByRole("menuitem", { name: "Suspend…" })).toBeDefined();
    expect(within(menu).queryByRole("menuitem", { name: "Resume…" })).toBeNull();
    fireEvent.keyDown(menu, { key: "Escape" });

    menu = await menuOf("Nangang Test Tenant");
    expect(within(menu).getByRole("menuitem", { name: "Resume…" })).toBeDefined();
    expect(within(menu).queryByRole("menuitem", { name: "Suspend…" })).toBeNull();
    fireEvent.keyDown(menu, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("menu") === null).toBe(true));

    menu = await menuOf("Old Contract Demo");
    expect(within(menu).queryByRole("menuitem", { name: "Suspend…" })).toBeNull();
    expect(within(menu).queryByRole("menuitem", { name: "Resume…" })).toBeNull();
    expect(within(menu).queryByRole("menuitem", { name: "Adjust quota…" })).toBeNull();
    expect(within(menu).getByRole("menuitem", { name: "Contract terminated, can't be resumed" })).toBeDefined();
  });

  test("suspending asks for the reason and the tenant's name, then shows 'Processing' until certs confirms", async () => {
    const api = new FakePlatform("normal");
    const { queryClient } = await open(api);
    await screen.findByRole("table");

    fireEvent.click(within(await menuOf("Demo Tenant A")).getByRole("menuitem", { name: "Suspend…" }));
    const dialog = await screen.findByRole("alertdialog");
    // The focus starts on Cancel, and nothing is sent until the name is typed.
    await waitFor(() =>
      expect(document.activeElement === within(dialog).getByRole("button", { name: "Cancel" })).toBe(true),
    );
    const confirm = within(dialog).getByRole("button", { name: "Suspend Demo Tenant A" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    expect(within(dialog).getByText(/6 members are blocked/)).toBeDefined();
    expect(within(dialog).getByText(/3 gateways have their certificates revoked/)).toBeDefined();

    fireEvent.click(within(dialog).getByRole("radio", { name: /Security or abuse/ }));
    fireEvent.change(within(dialog).getByLabelText(/Type the tenant name/), { target: { value: "Demo Tenant" } });
    expect(within(dialog).getByText("This doesn't match.")).toBeDefined();
    expect(confirm.disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText(/Type the tenant name/), { target: { value: "Demo Tenant A" } });
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);

    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    expect(screen.getByText(/The suspension of Demo Tenant A was sent and is processing/)).toBeDefined();
    await waitFor(() => expect(within(row("Demo Tenant A")).getByText("Processing")).toBeDefined());

    // certs confirms; the 5-second refresh reads the list again.
    const id = [...api.tenants.values()].find((tenant) => tenant.name === "Demo Tenant A")?.id ?? "";
    api.confirmApplied(id);
    await queryClient.invalidateQueries();
    await waitFor(() => expect(within(row("Demo Tenant A")).getByText("Suspended")).toBeDefined());
    expect(within(row("Demo Tenant A")).getByText("Security or abuse")).toBeDefined();
  });

  test("ending the contract also needs the box ticked, and says it is final", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    await screen.findByRole("table");
    fireEvent.click(within(await menuOf("Acme Labs Test")).getByRole("menuitem", { name: "Suspend…" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("radio", { name: /Contract terminated/ }));
    expect(within(dialog).getByText("This is a final state")).toBeDefined();
    fireEvent.change(within(dialog).getByLabelText(/Type the tenant name/), { target: { value: "Acme Labs Test" } });
    const confirm = within(dialog).getByRole("button", { name: "End the contract and suspend" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.click(
      within(dialog).getByRole("checkbox", { name: "I understand that ending the contract can't be undone" }),
    );
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    const record = [...api.tenants.values()].find((tenant) => tenant.name === "Acme Labs Test");
    expect(record?.reason).toBe("contract_terminated");
  });

  test("stops without writing when the tenant's state changed after the dialog opened", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    await screen.findByRole("table");
    fireEvent.click(within(await menuOf("Demo Tenant A")).getByRole("menuitem", { name: "Suspend…" }));
    const dialog = await screen.findByRole("alertdialog");

    // Another administrator suspends it meanwhile.
    const record = [...api.tenants.values()].find((tenant) => tenant.name === "Demo Tenant A");
    if (record) {
      record.state = "suspended";
      record.reason = "non_payment";
      record.stateVersion += 1;
    }
    fireEvent.change(within(dialog).getByLabelText(/Type the tenant name/), { target: { value: "Demo Tenant A" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Suspend Demo Tenant A" }));
    expect(await within(dialog).findByText("The state changed after you opened this")).toBeDefined();
    expect(api.calls.filter((call) => call.startsWith("setTenantState"))).toEqual([]);
    expect((within(dialog).getByRole("button", { name: "Suspend Demo Tenant A" }) as HTMLButtonElement).disabled).toBe(
      true,
    );

    fireEvent.click(within(dialog).getByRole("button", { name: "Look at the current state" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(within(row("Demo Tenant A")).getByText("Processing")).toBeDefined());
  });

  test("a refused resume stays in the dialog and can be sent again", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    await screen.findByRole("table");
    fireEvent.click(within(await menuOf("Nangang Test Tenant")).getByRole("menuitem", { name: "Resume…" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText(/Now: suspended, reason “Non-payment”/)).toBeDefined();
    expect(within(dialog).getByText(/2 members can use it/)).toBeDefined();

    api.failNext("setTenantState", unavailable());
    fireEvent.click(within(dialog).getByRole("button", { name: "Resume Nangang Test Tenant" }));
    expect(await within(dialog).findByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    expect(within(dialog).getByText("Nothing was changed.")).toBeDefined();

    fireEvent.click(within(dialog).getByRole("button", { name: "Resume Nangang Test Tenant" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(within(row("Nangang Test Tenant")).getByText("Active")).toBeDefined());
  });

  test("a platform administrator whose current tenant is suspended is told, with a way to resume it", async () => {
    const api = new FakePlatform("empty");
    api.addTenant("Harbor Warehouse", {
      state: "suspended",
      reason: "non_payment",
      stateVersion: 2,
      appliedVersion: 2,
    });
    renderPlatform(PLATFORM_TENANTS_PATH, {
      api,
      tenants: ["Harbor Warehouse"],
      prepare: ({ tenants }) => {
        for (const tenant of tenants) tenant.state = "suspended";
      },
    });
    await screen.findByRole("heading", { level: 1, name: "Tenants" });
    expect(await screen.findByText("Your current tenant “Harbor Warehouse” is suspended")).toBeDefined();
  });

  test("everyone but a platform administrator sees that they have no permission", async () => {
    renderPlatform(PLATFORM_TENANTS_PATH, { api: new FakePlatform("normal"), platformAdmin: false });
    expect(
      await screen.findByRole("heading", { name: "You don't have permission to open the platform admin" }),
    ).toBeDefined();
    expect(screen.queryByRole("table")).toBeNull();
  });
});
