import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { FakePlatform } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { PLATFORM_AUDIT_PATH, platformAuditPath } from "./paths.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

async function open(api: FakePlatform, path = PLATFORM_AUDIT_PATH) {
  const rendered = renderPlatform(path, { api });
  await screen.findByRole("heading", { level: 1, name: "Operation log" });
  return rendered;
}

function rows(): HTMLElement[] {
  return within(screen.getByRole("table")).getAllByRole("row").slice(1);
}

describe("the operation log", () => {
  test("the platform-wide log lists model roles and registrations, and an event type this version doesn't know", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");
    expect(screen.getByText("Model role changes are ordered by version")).toBeDefined();
    expect(screen.getByText("main: claude-sonnet-5-5, claude-opus-5-5 (version 14)")).toBeDefined();
    expect(screen.getAllByText("Factory registration").length).toBe(6);

    // An unknown type is still listed, by its code, and says to open the payload.
    const unknown = rows().find((entry) => within(entry).queryByText("assets.something_new"));
    expect(unknown).toBeDefined();
    expect(within(unknown as HTMLElement).getByText("An event this version doesn't know")).toBeDefined();
    expect(within(unknown as HTMLElement).getByText("Open it to read the payload as written.")).toBeDefined();
    expect(within(unknown as HTMLElement).getByText("The producer gave none")).toBeDefined();
  });

  test("a row opens to the whole event with its payload as written", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");
    const entry = rows().find((candidate) => within(candidate).queryByText("Model role changed")) as HTMLElement;
    const toggle = within(entry).getByRole("button", { name: "Show the details of Model role changed" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    const detail = screen.getByText("Event ID").closest("dl") as HTMLElement;
    expect(within(detail).getByText("identity.ai_model_role_changed v1")).toBeDefined();
    expect(within(detail).getByText("identity")).toBeDefined();
    expect(screen.getByText(/"action": "set"/)).toBeDefined();
    fireEvent.click(within(entry).getByRole("button", { name: "Hide the details of Model role changed" }));
    expect(screen.queryByText("Event ID")).toBeNull();
  });

  test("one tenant's log shows its state changes with the actor's email, and a link opens it directly", async () => {
    const api = new FakePlatform("normal");
    const id = [...api.tenants.values()].find((tenant) => tenant.name === "Nangang Test Tenant")?.id ?? "";
    await open(api, platformAuditPath(id));
    await screen.findByRole("table");
    expect(screen.getByRole("combobox", { name: "Tenant" })).toBeDefined();
    expect((screen.getByRole("combobox", { name: "Tenant" }) as HTMLSelectElement).value).toBe(id);
    expect(screen.queryByText("Model role changes are ordered by version")).toBeNull();
    expect(screen.getByText("Suspended (Non-payment), version 2")).toBeDefined();
    expect(await screen.findByText("ops@example.test")).toBeDefined();
    expect(screen.getByText("Confirmed state version 2")).toBeDefined();
    expect(screen.getByText("certs")).toBeDefined();
  });

  test("the scope control switches between the platform and a tenant", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");
    fireEvent.click(screen.getByRole("button", { name: "One tenant" }));
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Tenant" })).toBeDefined());
    await screen.findByRole("table");
    expect(screen.queryByText("Factory registration")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Platform-wide" }));
    await waitFor(() => expect(screen.getAllByText("Factory registration").length).toBeGreaterThan(0));
  });

  test("the type filter only covers the loaded page, and says so when nothing is left", async () => {
    await open(new FakePlatform("normal"));
    await screen.findByRole("table");
    expect(screen.getByRole("combobox", { name: "Type (this page only)" })).toBeDefined();
    fireEvent.change(screen.getByRole("combobox", { name: "Type (this page only)" }), { target: { value: "gateway" } });
    expect(rows()).toHaveLength(6);
    fireEvent.change(screen.getByRole("combobox", { name: "Type (this page only)" }), { target: { value: "members" } });
    expect(screen.getByText("No event of this type on the loaded page")).toBeDefined();
  });

  test("a range whose start is not before its end is stopped before it is sent", async () => {
    const api = new FakePlatform("normal");
    await open(api);
    await screen.findByRole("table");
    const before = api.calls.length;
    fireEvent.change(screen.getByLabelText("From"), { target: { value: "2026-11-02T10:00" } });
    fireEvent.change(screen.getByLabelText("To (exclusive)"), { target: { value: "2026-11-01T10:00" } });
    expect(screen.getByRole("alert").textContent).toBe("“From” must be earlier than “To”.");
    expect(screen.queryByRole("table")).toBeNull();
    expect(api.calls.length).toBe(before);
    // The events of the previous range are no longer on screen, so they cannot be exported either.
    expect((screen.getByRole("button", { name: "Export this page (JSON)" }) as HTMLButtonElement).disabled).toBe(true);
  });

  test("loads the next page by cursor, and says there is nothing in an empty period", async () => {
    const api = new FakePlatform("normal");
    api.pageSize = 3;
    await open(api);
    await screen.findByRole("table");
    expect(rows()).toHaveLength(3);
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(rows()).toHaveLength(6));
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
    cleanup();

    await open(new FakePlatform("empty"));
    expect(await screen.findByText("No events in this period")).toBeDefined();
  });

  test("an unreadable log says nothing is lost, and a retry reads it again", async () => {
    const api = new FakePlatform("normal");
    api.failNext("listEvents");
    await open(api);
    expect(await screen.findByText("The operation log can't be read right now")).toBeDefined();
    expect(screen.getByText(/none is lost/)).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("table");
  });
});
