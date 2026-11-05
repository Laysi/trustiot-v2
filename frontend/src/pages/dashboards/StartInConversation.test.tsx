import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import { DASHBOARDS_PATH, dashboardPath } from "../../app/paths.ts";
import { SPECS } from "../../dashboard/fixtures.ts";
import { fakeEngine } from "../../test/fake-chart.ts";
import { FakeDashboardServer, type FakeRole } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

function open(server: FakeDashboardServer, path: string, role: FakeRole = "editor") {
  server.role = role;
  const conversations = new FakeQuestionServer();
  const rendered = renderAt(path, {
    dashboards: server.api(),
    conversations: conversations.api(),
    chartEngine: fakeEngine().engine,
    identity: signedInBrowser({ role }).browser,
  });
  return { ...rendered, conversations };
}

describe("「在對話中新增」 on the dashboard list", () => {
  test("opens the conversation home with a question to finish, and sends nothing by itself", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Energy" });
    const { router, conversations } = open(server, DASHBOARDS_PATH);
    const link = await screen.findByRole("link", { name: "Create in conversation" });
    act(() => {
      fireEvent.click(link);
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    const input = (await screen.findByRole("textbox", { name: "Ask TrustIoT" })) as HTMLTextAreaElement;
    expect(input.value).toBe("Create a dashboard: ");
    expect(conversations.requests.filter((request) => request.method === "POST")).toHaveLength(0);
  });

  test("a viewer does not see it", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Energy" });
    open(server, DASHBOARDS_PATH, "viewer");
    await screen.findByRole("table");
    expect(screen.queryByRole("link", { name: "Create in conversation" })).toBeNull();
  });
});

describe("「在對話中編輯」 on a dashboard's page", () => {
  test("opens the conversation home with a question naming the dashboard", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ title: "Energy overview", versions: [[SPECS.kpi]] });
    const { router } = open(server, dashboardPath(dashboard.id));
    const link = await screen.findByRole("link", { name: "Edit in conversation" });
    act(() => {
      fireEvent.click(link);
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    const input = (await screen.findByRole("textbox", { name: "Ask TrustIoT" })) as HTMLTextAreaElement;
    expect(input.value).toBe('Change the dashboard "Energy overview": ');
  });

  test("a viewer, an archived dashboard and a draft have no such entry", async () => {
    const server = new FakeDashboardServer();
    const saved = server.addDashboard({ title: "Saved", versions: [[SPECS.kpi]] });
    const archived = server.addDashboard({ title: "Old", versions: [[SPECS.kpi]], archivedAt: "2026-11-02T00:00:00Z" });
    const draft = server.addDashboard({ title: "Draft", kind: "draft", versions: [[SPECS.kpi]] });
    open(server, dashboardPath(saved.id), "viewer");
    await screen.findByRole("heading", { level: 1, name: "Saved" });
    expect(screen.queryByRole("link", { name: "Edit in conversation" })).toBeNull();
    cleanup();
    for (const other of [archived, draft]) {
      open(server, dashboardPath(other.id), "editor");
      await screen.findByRole("heading", { level: 1 });
      expect(screen.queryByRole("link", { name: "Edit in conversation" })).toBeNull();
      cleanup();
    }
  });
});
