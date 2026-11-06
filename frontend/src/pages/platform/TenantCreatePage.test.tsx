import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import { ApiError } from "../../api/errors.ts";
import { FakePlatform } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { PLATFORM_TENANT_NEW_PATH } from "./paths.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

async function open(api: FakePlatform) {
  renderPlatform(PLATFORM_TENANT_NEW_PATH, { api });
  await screen.findByRole("heading", { level: 1, name: "Create tenant" });
}

function fill(name: string, email: string) {
  fireEvent.change(screen.getByLabelText("Tenant name"), { target: { value: name } });
  fireEvent.change(screen.getByLabelText("First admin's email"), { target: { value: email } });
}

const calls = (api: FakePlatform, method: string) => api.calls.filter((call) => call.startsWith(method)).length;

describe("creating a tenant", () => {
  test("checks the name and the email before it goes to the review step", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    expect(screen.getByText("Enter a name.")).toBeDefined();
    expect(screen.getByText("Enter a valid email address.")).toBeDefined();
    expect(screen.queryByText("Review and create, step 2 of 2")).toBeNull();

    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    expect(screen.getByText("Review and create, step 2 of 2")).toBeDefined();
    expect(screen.getByText("Riverside Plant Demo")).toBeDefined();
    expect(screen.getByText(/Logged: the tenant's creation and the invitation/)).toBeDefined();
    // Going back keeps what was typed.
    fireEvent.click(screen.getByRole("button", { name: "Back to edit" }));
    expect((screen.getByLabelText("Tenant name") as HTMLInputElement).value).toBe("Riverside Plant Demo");
    expect(calls(api, "createTenant")).toBe(0);
  });

  test("creates a Starter tenant and invites the first admin, and the log shows both", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));

    expect(await screen.findByText("Created “Riverside Plant Demo”")).toBeDefined();
    expect(screen.getByText("The invitation was sent to first.admin@example.com.")).toBeDefined();
    expect(calls(api, "createTenant")).toBe(1);
    // A Starter tenant is what a new tenant is: no plan call.
    expect(calls(api, "setTenantPlan")).toBe(0);
    const tenant = [...api.tenants.values()][0];
    expect(tenant?.members.map((member) => member.role)).toEqual(["admin"]);
    expect(api.events.map((event) => event.type)).toEqual(["identity.tenant_created", "identity.member_role_changed"]);
    const link = screen.getByRole("link", { name: "Open the tenant" });
    expect(link.getAttribute("href")).toBe(`/platform/tenants/${tenant?.id}`);
  });

  test("an Enterprise tenant takes override values, blank ones stay the plan's", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("radio", { name: /Enterprise/ }));
    fireEvent.change(screen.getByLabelText("Number of gateways"), { target: { value: "40" } });
    fireEvent.change(screen.getByLabelText("Monthly AI allowance"), { target: { value: "-3" } });
    expect(screen.getByText("Enter an amount of 0 or more.")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    expect(screen.queryByText("Review and create, step 2 of 2")).toBeNull();

    fireEvent.change(screen.getByLabelText("Monthly AI allowance"), { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    expect(screen.getByText("gateways 40")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));
    await screen.findByText("Created “Riverside Plant Demo”");
    const tenant = [...api.tenants.values()][0];
    expect(tenant?.plan).toBe("enterprise");
    expect(tenant?.overrides).toEqual({ maxGateways: 40 });
  });

  test("when only the invitation fails, the tenant exists and the page never creates it a second time", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    api.failNext("inviteFirstAdmin", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));

    expect(
      await screen.findByText("Tenant “Riverside Plant Demo” was created, but the invitation was not sent"),
    ).toBeDefined();
    expect(screen.queryByRole("button", { name: "Create tenant and send the invitation" })).toBeNull();
    expect(screen.getByText(/don't create it again/)).toBeDefined();
    expect(calls(api, "createTenant")).toBe(1);
    const tenant = [...api.tenants.values()][0];
    expect(screen.getByRole("link", { name: "Open the tenant" }).getAttribute("href")).toBe(
      `/platform/tenants/${tenant?.id}`,
    );

    // Trying again sends only the invitation: no second tenant.
    fireEvent.click(screen.getByRole("button", { name: "Try the remaining steps again" }));
    await screen.findByText("Created “Riverside Plant Demo”");
    expect(calls(api, "createTenant")).toBe(1);
    expect(calls(api, "inviteFirstAdmin")).toBe(2);
  });

  test("when the plan fails, trying again sets the plan once, then invites, and never creates the tenant again", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("radio", { name: /Business/ }));
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    api.failNext("setTenantPlan", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));

    await screen.findByText("Tenant “Riverside Plant Demo” was created, but its plan was not set");
    expect(calls(api, "inviteFirstAdmin")).toBe(0);
    fireEvent.click(screen.getByRole("button", { name: "Try the remaining steps again" }));
    await screen.findByText("Created “Riverside Plant Demo”");
    expect(calls(api, "createTenant")).toBe(1);
    expect(calls(api, "setTenantPlan")).toBe(2);
    expect(calls(api, "inviteFirstAdmin")).toBe(1);
    expect([...api.tenants.values()][0]?.plan).toBe("business");
  });

  test("when the answer to the invitation was lost, trying again waits until the operation log was looked at", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    api.failNext("inviteFirstAdmin", new ApiError(504, { code: "timeout", message_key: "error.timeout" }));
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));

    await screen.findByText("Tenant “Riverside Plant Demo” was created, but the invitation was not sent");
    const retry = screen.getByRole("button", { name: "Try the remaining steps again" }) as HTMLButtonElement;
    expect(retry.disabled).toBe(true);
    expect(
      screen.getByRole("link", { name: "Open the tenant's operation log in a new tab" }).getAttribute("href"),
    ).toBe(`/platform/tenants/${[...api.tenants.values()][0]?.id}?tab=audit`);
    fireEvent.click(screen.getByRole("checkbox", { name: /the invitation did not go through/ }));
    expect(retry.disabled).toBe(false);
    fireEvent.click(retry);
    await screen.findByText("Created “Riverside Plant Demo”");
    expect(calls(api, "inviteFirstAdmin")).toBe(2);
  });

  test("while it is sending the button is disabled, so a second click cannot create a second tenant", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    api.hold("createTenant");
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));
    const sending = await screen.findByRole("button", { name: "Creating…" });
    expect((sending as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(sending);
    api.release("createTenant");
    await screen.findByText("Created “Riverside Plant Demo”");
    expect(calls(api, "createTenant")).toBe(1);
  });

  test("when it is not known whether the tenant was created, creating again waits until the person looked at the list", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("Riverside Plant Demo", "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    api.failNext("createTenant", new ApiError(504, { code: "timeout", message_key: "error.timeout" }));
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));

    await screen.findByText(/It is not known whether the tenant was created/);
    const submit = screen.getByRole("button", { name: "Create tenant and send the invitation" }) as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    fireEvent.click(submit);
    expect(calls(api, "createTenant")).toBe(1);
    expect(screen.getByRole("link", { name: "Open the tenant list in a new tab" }).getAttribute("href")).toBe(
      "/platform/tenants",
    );

    fireEvent.click(screen.getByRole("checkbox", { name: /is not there/ }));
    expect(submit.disabled).toBe(false);
    fireEvent.click(submit);
    await screen.findByText("Created “Riverside Plant Demo”");
    expect(calls(api, "createTenant")).toBe(2);
  });

  test("a refused name is shown and the creation can be sent again", async () => {
    const api = new FakePlatform("empty");
    await open(api);
    fill("x".repeat(101), "first.admin@example.com");
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    expect(screen.getByText("Enter a name of 1 to 100 characters.")).toBeDefined();
    fireEvent.change(screen.getByLabelText("Tenant name"), { target: { value: "Riverside Plant Demo" } });
    fireEvent.click(screen.getByRole("button", { name: "Next: review" }));
    api.failNext("createTenant");
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));
    expect(await screen.findByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Create tenant and send the invitation" }));
    await waitFor(() => expect(screen.getByText("Created “Riverside Plant Demo”")).toBeDefined());
    expect(calls(api, "createTenant")).toBe(2);
    expect(api.tenants.size).toBe(1);
  });
});
