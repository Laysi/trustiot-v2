import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { FakePlatform } from "../../test/fake-platform.ts";
import { allowGenerousTimeouts } from "../../test/platform-timing.ts";
import { renderPlatform } from "../../test/render-platform.tsx";
import { PLATFORM_PROVISIONING_PATH } from "./paths.ts";

allowGenerousTimeouts();

afterEach(() => {
  cleanup();
});

describe("the provisioning events", () => {
  test("lists the factory registrations newest first, with the operator's email and where the gateway is now", async () => {
    renderPlatform(PLATFORM_PROVISIONING_PATH, { api: new FakePlatform("normal") });
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(6);
    expect(within(rows[0] as HTMLElement).getByText("TIG-24A0-0031")).toBeDefined();
    expect(within(rows[0] as HTMLElement).getByText("Device pool")).toBeDefined();
    expect(await within(table).findAllByText("platform-admin@example.test")).toHaveLength(6);
    expect(within(table).getAllByText("Demo Tenant A")).toHaveLength(3);
  });

  test("filters by workstation", async () => {
    renderPlatform(PLATFORM_PROVISIONING_PATH, { api: new FakePlatform("normal") });
    await screen.findByRole("table");
    fireEvent.change(screen.getByRole("combobox", { name: "Workstation" }), { target: { value: "WS-02" } });
    await screen.findByRole("table");
    const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);
    expect(rows.length).toBe(3);
    for (const entry of rows) expect(within(entry).getByText("WS-02")).toBeDefined();
  });

  test("a range whose start is not before its end is stopped before it is sent", async () => {
    const api = new FakePlatform("normal");
    renderPlatform(PLATFORM_PROVISIONING_PATH, { api });
    await screen.findByRole("table");
    // A start alone is a valid range and is sent; the end that makes the range backwards is not.
    fireEvent.change(screen.getByLabelText("From"), { target: { value: "2026-10-01T10:00" } });
    await waitFor(() => expect(screen.queryByRole("table") !== null).toBe(true));
    await new Promise((resolve) => setTimeout(resolve, 0));
    const before = api.calls.length;
    fireEvent.change(screen.getByLabelText("To (exclusive)"), { target: { value: "2026-09-01T10:00" } });
    expect(screen.getByRole("alert").textContent).toBe("“From” must be earlier than “To”.");
    expect(screen.queryByRole("table")).toBeNull();
    expect(api.calls.length).toBe(before);
  });

  test("says when there is no registration, and when the events cannot be read, with a retry", async () => {
    renderPlatform(PLATFORM_PROVISIONING_PATH, { api: new FakePlatform("empty") });
    expect(await screen.findByText("No factory registration in this period")).toBeDefined();
    cleanup();

    const api = new FakePlatform("normal");
    api.failNext("listProvisioning");
    renderPlatform(PLATFORM_PROVISIONING_PATH, { api });
    expect(await screen.findByText("The provisioning events can't be loaded right now")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("table");
  });

  test("loads the next page on request", async () => {
    const api = new FakePlatform("normal");
    api.pageSize = 4;
    renderPlatform(PLATFORM_PROVISIONING_PATH, { api });
    await screen.findByRole("table");
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(5);
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await screen.findAllByText("TIG-24A0-0002");
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(7);
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
  });
});
