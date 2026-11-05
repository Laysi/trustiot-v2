import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../../api/errors.ts";
import { FakeAiUsageServer } from "../../../test/fake-ai-usage.ts";
import { renderPlatformAi } from "../../../test/render-platform-ai.tsx";
import { USAGE_PATH } from "./paths.ts";

afterEach(() => {
  cleanup();
});

function open(server: FakeAiUsageServer, options: { platformAdmin?: boolean } = {}) {
  return renderPlatformAi(USAGE_PATH, { usage: server.api(), ...options });
}

function tenantRow(name: RegExp | string): HTMLElement {
  return within(screen.getByRole("table")).getByRole("row", {
    name: typeof name === "string" ? new RegExp(name) : name,
  });
}

describe("the AI usage page", () => {
  test("shows the month's four figures and each tenant's use of its quota", async () => {
    open(new FakeAiUsageServer());
    expect(await screen.findByRole("heading", { level: 1, name: "AI usage and reconciliation" })).toBeDefined();
    await screen.findByText("Amount");
    const tiles = (label: string) => screen.getByText(label).closest("div") as HTMLElement;
    expect(within(tiles("Amount")).getByText("US$166.61")).toBeDefined();
    expect(within(tiles("Model calls")).getByText("18,240")).toBeDefined();
    expect(within(tiles("Model calls")).getByText("212 failed, 9 refused")).toBeDefined();
    expect(within(tiles("Overdrawn tenants")).getByText("1")).toBeDefined();
    expect(within(tiles("Estimated entries")).getByText("37")).toBeDefined();
    expect(within(tiles("Estimated entries")).getByText("4 of them are thinking lower bounds")).toBeDefined();

    const over = tenantRow("Acme Labs Test");
    expect(within(over).getByText("Overdrawn")).toBeDefined();
    expect(within(over).getByText(/US\$0\.31 over, refused since /)).toBeDefined();
    expect(within(over).getByText("US$10.31 / US$10.00")).toBeDefined();
    expect(within(over).getByText("103%")).toBeDefined();
    expect(within(tenantRow("Demo Tenant A")).getByText("Includes a temporary raise of US$20.00")).toBeDefined();
    expect(within(tenantRow("Demo Tenant A")).getByText("US$12.40 / US$70.00")).toBeDefined();
    const north = tenantRow("North Demo Factory");
    expect(within(north).getByText("31")).toBeDefined();
    expect(within(north).getByText("lower bound 4")).toBeDefined();
    const platform = within(screen.getByRole("table")).getAllByRole("row").at(-1) as HTMLElement;
    expect(within(platform).getByText("Question bank runs and other platform use, not any tenant's")).toBeDefined();
    expect(within(platform).getByText("US$14.08 / —")).toBeDefined();
  });

  test("only an overdrawn tenant has the shortcut to raise its quota", async () => {
    open(new FakeAiUsageServer());
    await screen.findByRole("table");
    const link = within(tenantRow("Acme Labs Test")).getByRole("link", {
      name: "Raise the quota of Acme Labs Test temporarily",
    });
    expect(link.getAttribute("href")).toBe("/platform/tenants/t-acme-labs");
    expect(screen.getAllByRole("link", { name: /Raise the quota/ })).toHaveLength(1);
  });

  test("tenants are ordered by quota used and can be sorted by amount or searched", async () => {
    open(new FakeAiUsageServer());
    await screen.findByRole("table");
    const names = () =>
      within(screen.getByRole("table"))
        .getAllByRole("row")
        .slice(1)
        .map((row) => (row.querySelector("strong")?.textContent ?? "").slice(0, 12).trim());
    expect(names()).toEqual(["Acme Labs Te", "Second Plant", "North Demo F", "Demo Tenant", "Platform"]);
    fireEvent.change(screen.getByLabelText("Sort"), { target: { value: "amount" } });
    expect(names()).toEqual(["North Demo F", "Second Plant", "Demo Tenant", "Acme Labs Te", "Platform"]);
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "demo tenant" } });
    expect(names()).toEqual(["Demo Tenant"]);
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "no such tenant" } });
    expect(screen.getByText("No tenant matches.")).toBeDefined();
  });

  test("the category tab breaks usage down by what the provider reported, with the estimated part marked", async () => {
    open(new FakeAiUsageServer());
    await screen.findByRole("table");
    fireEvent.click(screen.getByRole("tab", { name: "By category" }));
    const table = await screen.findByRole("table");
    const rows = within(table).getAllByRole("row").slice(1);
    // Each cell prints its column's name first (for a narrow screen), then the value.
    const value = (cell: HTMLElement | undefined) => cell?.lastElementChild?.textContent;
    expect(rows.map((row) => value(within(row).getAllByRole("cell")[1]))).toEqual([
      "Input",
      "Cache write (5 minutes)",
      "Cache write (1 hour)",
      "Cache read",
      "Output (thinking included)",
      "Input (Decisions charges input only)",
    ]);
    expect(within(rows[4] as HTMLElement).getByText("includes 8,410 estimated")).toBeDefined();
    expect(within(rows[4] as HTMLElement).getByText("US$142.66")).toBeDefined();
  });

  test("a month that is not over cannot be reconciled", async () => {
    const server = new FakeAiUsageServer();
    open(server);
    await screen.findByRole("table");
    fireEvent.click(screen.getByRole("tab", { name: "Reconciliation" }));
    expect(await screen.findByText("November 2026 is not over yet")).toBeDefined();
    expect(server.calls.some((call) => call.startsWith("getReconciliation"))).toBe(false);
    expect(screen.getByRole("option", { name: "November 2026 (in progress)" })).toBeDefined();
  });

  test("a month that is over shows the ledger beside each invoice, with the difference and a status in words", async () => {
    open(new FakeAiUsageServer());
    await screen.findByRole("table");
    fireEvent.change(screen.getByLabelText("Month (UTC)"), { target: { value: "2026-10" } });
    fireEvent.click(await screen.findByRole("tab", { name: "Reconciliation" }));
    const table = await screen.findByRole("table", { name: "Reconciliation for October 2026" });
    const rowFor = (provider: string) => within(table).getByRole("row", { name: new RegExp(provider) });
    expect(within(rowFor("Anthropic")).getByText("US$1,204.55")).toBeDefined();
    expect(within(rowFor("Anthropic")).getByText("+US$1.47（0.12%）")).toBeDefined();
    expect(within(rowFor("Anthropic")).getByText("Within tolerance")).toBeDefined();
    expect(within(rowFor("OpenAI")).getByText("+US$13.70（22.39%）")).toBeDefined();
    expect(within(rowFor("OpenAI")).getByText("To check")).toBeDefined();
    expect(within(rowFor("Google Vertex AI")).getByText("Awaiting invoice")).toBeDefined();
    expect(within(rowFor("Google Vertex AI")).getByRole("button", { name: "Enter the invoice amount…" })).toBeDefined();
    expect(screen.getByRole("button", { name: "Download the report (CSV)" })).toBeDefined();
  });

  test("an invoice amount is typed in, checked, and the reconciliation shows it", async () => {
    const server = new FakeAiUsageServer();
    open(server);
    await screen.findByRole("table");
    fireEvent.change(screen.getByLabelText("Month (UTC)"), { target: { value: "2026-10" } });
    fireEvent.click(await screen.findByRole("tab", { name: "Reconciliation" }));
    const table = await screen.findByRole("table", { name: "Reconciliation for October 2026" });
    fireEvent.click(within(table).getByRole("button", { name: "Enter the invoice amount…" }));
    const dialog = await screen.findByRole("alertdialog", {
      name: "Enter the invoice amount of Google Vertex AI for October 2026",
    });
    fireEvent.change(within(dialog).getByLabelText("Invoice amount (US$)"), { target: { value: "abc" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Save the amount" }));
    expect(await within(dialog).findByText("Enter a number greater than 0.")).toBeDefined();
    expect(server.calls.some((call) => call.startsWith("setInvoice"))).toBe(false);
    fireEvent.change(within(dialog).getByLabelText("Invoice amount (US$)"), { target: { value: "12.5" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Save the amount" }));
    // Not `expect(element).toBeNull()`: while it fails, bun prints the whole dialog element each time, which is slow.
    await waitFor(() => {
      if (screen.queryByRole("alertdialog") !== null) throw new Error("the invoice dialog is still open");
    });
    expect(server.calls.at(-1)).toBe("setInvoice 2026-10 Google Vertex AI");
    expect(await screen.findByText("US$12.50")).toBeDefined();
  });

  test("a month without usage and a ledger that cannot be read each say so", async () => {
    const empty = new FakeAiUsageServer({ seed: "empty" });
    open(empty);
    expect(await screen.findByText("No AI usage this month yet")).toBeDefined();
    cleanup();

    const broken = new FakeAiUsageServer();
    broken.failNext("getUsage", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    open(broken);
    expect(await screen.findByText("The ledger cannot be read right now")).toBeDefined();
    expect(screen.getByText(/the AI gateway refuses calls too/)).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("table")).toBeDefined();
  });

  test("a user who is not a platform administrator gets the no-permission page and nothing is read", async () => {
    const server = new FakeAiUsageServer();
    open(server, { platformAdmin: false });
    expect(
      await screen.findByRole("heading", { name: "You don't have permission to open the platform admin" }),
    ).toBeDefined();
    expect(server.calls).toEqual([]);
  });

  test("the sections of the platform AI group are all there in the preview", async () => {
    open(new FakeAiUsageServer());
    const nav = await screen.findByRole("navigation", { name: "Platform admin sections" });
    expect(
      within(nav)
        .getAllByRole("link")
        .map((link) => link.textContent),
    ).toEqual(["Model roles", "System prompt", "Usage and reconciliation"]);
    expect(within(nav).getByRole("link", { name: "Usage and reconciliation" }).getAttribute("aria-current")).toBe(
      "page",
    );
  });
});
