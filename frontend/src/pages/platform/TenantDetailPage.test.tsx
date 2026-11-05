import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { FakePlatform } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { platformTenantPath } from "./paths.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

function idOf(api: FakePlatform, name: string): string {
  const tenant = [...api.tenants.values()].find((record) => record.name === name);
  if (!tenant) throw new Error(`no tenant ${name}`);
  return tenant.id;
}

async function open(api: FakePlatform, name: string, tab?: "members" | "gateways" | "audit") {
  const rendered = renderPlatform(platformTenantPath(idOf(api, name), tab), { api });
  await screen.findByRole("heading", { level: 1, name });
  return rendered;
}

describe("a tenant's page", () => {
  test("shows a normal tenant's state, quota with the temporary raise, and its latest operations", async () => {
    const api = new FakePlatform("normal");
    await open(api, "Demo Tenant A");
    expect(screen.getAllByText(/Business/).length).toBeGreaterThan(0);
    const state = screen.getByRole("heading", { name: /^State/ }).closest("section") as HTMLElement;
    expect(within(state).getByText("Active")).toBeDefined();
    expect(within(state).getByText("State version")).toBeDefined();
    expect(within(state).getByText("— (not changed since it was created)")).toBeDefined();

    const quota = screen.getByRole("heading", { name: "Quota" }).closest("section") as HTMLElement;
    expect(within(quota).getByText("3 / 5")).toBeDefined();
    expect(within(quota).getByText("6 / 10")).toBeDefined();
    expect(within(quota).getByText("12 / 20")).toBeDefined();
    expect(within(quota).getByText(/A temporary raise of \$20\.00 until/)).toBeDefined();

    // The latest three operations of the tenant, with the actor's email, not an ID.
    expect(await screen.findByText("AI raise 0 → 20; reason “More headroom before a customer demo”")).toBeDefined();
    expect(screen.getAllByText("platform-admin@example.test").length).toBeGreaterThan(0);
    // The header offers the quota and the suspension.
    expect(screen.getByRole("button", { name: "Adjust quota" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Suspend…" })).toBeDefined();
  });

  test("a suspension in progress is a timeline, and past ten minutes it says certs may be stuck", async () => {
    const api = new FakePlatform("normal");
    await open(api, "North Plant Demo");
    expect(screen.getAllByText("Processing").length).toBeGreaterThan(0);
    expect(screen.getByText("Suspension recorded (version 4)")).toBeDefined();
    expect(screen.getByText("API and AI blocked")).toBeDefined();
    expect(screen.getByText("Waiting for certs to confirm")).toBeDefined();
    expect(screen.queryByText("certs has not confirmed for over 10 minutes")).toBeNull();
    // Resume is on the state card, and the header has no suspend button.
    expect(screen.getByRole("button", { name: "Resume…" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "Suspend…" })).toBeNull();
    cleanup();

    const old = new FakePlatform("normal");
    const record = [...old.tenants.values()].find((tenant) => tenant.name === "North Plant Demo");
    if (record) record.stateChangedAt = new Date(Date.now() - 23 * 60_000);
    await open(old, "North Plant Demo");
    expect(await screen.findByText("certs has not confirmed for over 10 minutes")).toBeDefined();
    expect(screen.getAllByText("Processing").length).toBeGreaterThan(0);
  });

  test("a suspended tenant can have its reason changed or be resumed; resuming makes it active again", async () => {
    const api = new FakePlatform("normal");
    await open(api, "Nangang Test Tenant");
    const state = screen.getByRole("heading", { name: /^State/ }).closest("section") as HTMLElement;
    expect(within(state).getByText("Suspended")).toBeDefined();
    expect(within(state).getByText("Non-payment")).toBeDefined();
    expect(within(state).getByText("ops@example.test")).toBeDefined();
    expect(within(state).getByRole("button", { name: "Change reason…" })).toBeDefined();

    fireEvent.click(within(state).getByRole("button", { name: "Resume…" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Resume Nangang Test Tenant" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(screen.getByRole("button", { name: "Suspend…" })).toBeDefined());
    expect(screen.getByText("Nangang Test Tenant was resumed.")).toBeDefined();
  });

  test("changing the reason of a suspension asks for the name and keeps the tenant suspended", async () => {
    const api = new FakePlatform("normal");
    await open(api, "Nangang Test Tenant");
    fireEvent.click(screen.getByRole("button", { name: "Change reason…" }));
    const dialog = await screen.findByRole("alertdialog");
    const confirm = within(dialog).getByRole("button", { name: "Change the reason" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.click(within(dialog).getByRole("radio", { name: /Security or abuse/ }));
    fireEvent.change(within(dialog).getByLabelText(/Type the tenant name/), {
      target: { value: "Nangang Test Tenant" },
    });
    fireEvent.click(confirm);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    const record = [...api.tenants.values()].find((tenant) => tenant.name === "Nangang Test Tenant");
    expect(record?.reason).toBe("security_or_abuse");
    expect(record?.state).toBe("suspended");
  });

  test("suspending moves the page from Processing to Suspended once certs confirms, and the log shows the operation, then resuming brings it back", async () => {
    const api = new FakePlatform("normal");
    const { queryClient, router } = await open(api, "Demo Tenant A");
    fireEvent.click(screen.getByRole("button", { name: "Suspend…" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("radio", { name: /Security or abuse/ }));
    fireEvent.change(within(dialog).getByLabelText(/Type the tenant name/), { target: { value: "Demo Tenant A" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Suspend Demo Tenant A" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));

    // Processing: the timeline, and nothing says "Suspended" yet.
    await screen.findByText("Waiting for certs to confirm");
    expect(screen.getAllByText("Processing").length).toBeGreaterThan(0);
    expect(screen.queryByText("Suspended")).toBeNull();

    // certs confirms; the 5-second refresh reads the tenant again.
    api.confirmApplied(idOf(api, "Demo Tenant A"));
    await queryClient.invalidateQueries();
    await waitFor(() => expect(screen.getAllByText("Suspended").length).toBeGreaterThan(0));
    expect(screen.queryByText("Waiting for certs to confirm")).toBeNull();

    // The operation log shows what was just done.
    await router.navigate(platformTenantPath(idOf(api, "Demo Tenant A"), "audit"));
    expect(await screen.findByText("Suspended (Security or abuse), version 2")).toBeDefined();
    expect(screen.getByText("Confirmed state version 2")).toBeDefined();
    await router.navigate(platformTenantPath(idOf(api, "Demo Tenant A")));

    // Resuming brings it back to normal.
    fireEvent.click(await screen.findByRole("button", { name: "Resume…" }));
    fireEvent.click(
      within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Resume Demo Tenant A" }),
    );
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    await waitFor(() => expect(screen.getByRole("button", { name: "Suspend…" })).toBeDefined());
    expect(screen.queryByText("Suspended")).toBeNull();
  });

  test("a tenant whose contract ended has no suspend, resume or quota buttons and says why", async () => {
    await open(new FakePlatform("normal"), "Old Contract Demo");
    expect(screen.getAllByText("Suspended, final").length).toBeGreaterThan(0);
    expect(screen.getByText(/can't be resumed or given another reason/)).toBeDefined();
    for (const name of ["Adjust quota", "Suspend…", "Resume…", "Change reason…", "Adjust"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
  });

  test("adjusting the quota needs a reason, shows before and after, and warns about a limit below the use", async () => {
    const api = new FakePlatform("normal");
    await open(api, "Demo Tenant A");
    fireEvent.click(screen.getByRole("button", { name: "Adjust quota" }));
    const dialog = await screen.findByRole("alertdialog");
    const apply = within(dialog).getByRole("button", { name: /^Apply/ }) as HTMLButtonElement;
    expect(apply.disabled).toBe(true);

    fireEvent.change(within(dialog).getByLabelText("Gateway limit"), { target: { value: "2" } });
    expect(within(dialog).getByText("The new limit 2 is below the 3 in use")).toBeDefined();
    fireEvent.change(within(dialog).getByLabelText("Gateway limit"), { target: { value: "8" } });
    expect(within(dialog).queryByText(/is below the/)).toBeNull();
    fireEvent.change(within(dialog).getByLabelText("AI raise this month"), { target: { value: "40" } });
    expect(within(dialog).getByText("Apply 2 changes")).toBeDefined();
    expect(within(dialog).getByText("$70.00")).toBeDefined();
    expect(within(dialog).getByText("$90.00")).toBeDefined();
    // The reason is required.
    expect(apply.disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText("Reason"), { target: { value: "Pilot expansion" } });
    expect(apply.disabled).toBe(false);
    fireEvent.click(apply);
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));

    const quota = screen.getByRole("heading", { name: "Quota" }).closest("section") as HTMLElement;
    await waitFor(() => expect(within(quota).getByText("3 / 8")).toBeDefined());
    const event = api.events.find(
      (entry) =>
        entry.type === "identity.quota_changed" &&
        entry.payload &&
        (entry.payload as { reason?: string }).reason === "Pilot expansion",
    );
    expect(event).toBeDefined();
  });

  test("a gateway limit override can be taken off, which sends null and restores the plan's limit", async () => {
    const api = new FakePlatform("normal");
    const record = [...api.tenants.values()].find((tenant) => tenant.name === "Demo Tenant A");
    if (record) record.overrides.maxGateways = 8;
    await open(api, "Demo Tenant A");
    fireEvent.click(screen.getByRole("button", { name: "Adjust quota" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("checkbox", { name: /Use the plan default for the gateway limit/ }));
    expect((within(dialog).getByLabelText("Gateway limit") as HTMLInputElement).disabled).toBe(true);
    expect(within(dialog).getByText("Apply 1 change")).toBeDefined();
    fireEvent.change(within(dialog).getByLabelText("Reason"), { target: { value: "Back to the plan" } });
    fireEvent.click(within(dialog).getByRole("button", { name: /^Apply/ }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    expect(record?.overrides.maxGateways).toBeUndefined();
    const quota = screen.getByRole("heading", { name: "Quota" }).closest("section") as HTMLElement;
    await waitFor(() => expect(within(quota).getByText("3 / 5")).toBeDefined());
  });

  test("the members tab lists the members and sets an existing user's role after a confirmation", async () => {
    const api = new FakePlatform("normal");
    await open(api, "Demo Tenant A", "members");
    const table = await screen.findByRole("table");
    expect(within(table).getByText("pm@example.test")).toBeDefined();

    fireEvent.change(screen.getByLabelText("Email"), { target: { value: "pm@example.test" } });
    fireEvent.change(screen.getByLabelText("Role"), { target: { value: "admin" } });
    fireEvent.click(screen.getByRole("button", { name: "Check and confirm…" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("viewer")).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Set admin" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog") === null).toBe(true));
    expect(screen.getByText("pm@example.test is now admin.")).toBeDefined();
    await waitFor(() => expect(within(screen.getByRole("table")).getAllByText("admin").length).toBe(2));
    expect(api.events.some((event) => event.type === "identity.member_role_changed")).toBe(true);
  });

  test("an email nobody has is refused in the dialog, which says so and stays open", async () => {
    const api = new FakePlatform("normal");
    await open(api, "Demo Tenant A", "members");
    await screen.findByRole("table");
    fireEvent.change(screen.getByLabelText("Email"), { target: { value: "nobody@example.com" } });
    fireEvent.click(screen.getByRole("button", { name: "Check and confirm…" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Add nobody@example.com as admin?")).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Add as admin" }));
    expect(await within(dialog).findByText(/No user has this email address/)).toBeDefined();
    expect(screen.getByRole("alertdialog")).toBeDefined();
  });

  test("the gateways tab lists only this tenant's gateways", async () => {
    await open(new FakePlatform("normal"), "Demo Tenant A", "gateways");
    const table = await screen.findByRole("table");
    expect(within(table).getByText("TIG-24A0-0007")).toBeDefined();
    expect(within(table).getByText("TIG-24A0-0019")).toBeDefined();
    expect(within(table).queryByText("TIG-24A0-0023")).toBeNull();
    expect(within(table).queryByText("TIG-24A0-0031")).toBeNull();
  });

  test("the operation log tab shows the tenant's events", async () => {
    await open(new FakePlatform("normal"), "Nangang Test Tenant", "audit");
    expect(await screen.findByText("Suspended (Non-payment), version 2")).toBeDefined();
    expect(screen.getByText("Confirmed state version 2")).toBeDefined();
  });

  test("says when the tenant doesn't exist", async () => {
    renderPlatform(platformTenantPath("00000000-0000-4000-8000-00000000ffff"), { api: new FakePlatform("normal") });
    expect(await screen.findByText("This tenant doesn't exist")).toBeDefined();
    expect(screen.getByRole("link", { name: "Back to the tenants" })).toBeDefined();
  });
});
