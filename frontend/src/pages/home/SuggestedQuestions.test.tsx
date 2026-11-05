import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { RouteObject } from "react-router";

import { type SuggestionsApi, SuggestionsApiProvider } from "../../api/pending/suggestions.ts";
import { Layout } from "../../app/Layout.tsx";
import { TenantGate } from "../../session/TenantGate.tsx";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { FakeSuggestionsServer } from "../../test/fake-suggestions.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { ConversationPage } from "../conversations/ConversationPage.tsx";
import { HomePage } from "../pages.tsx";
import { SuggestedQuestions } from "./SuggestedQuestions.tsx";
import { HomeSuggestionsSlotProvider } from "./suggestions-slot.tsx";

afterEach(() => {
  cleanup();
});

/** The home page with the suggestions provided as the UI preview provides them. */
function homeWith(api: SuggestionsApi): RouteObject[] {
  return [
    {
      path: "/",
      element: <Layout />,
      children: [
        {
          element: <TenantGate />,
          children: [
            {
              index: true,
              element: (
                <SuggestionsApiProvider api={api}>
                  <HomeSuggestionsSlotProvider slot={<SuggestedQuestions />}>
                    <HomePage />
                  </HomeSuggestionsSlotProvider>
                </SuggestionsApiProvider>
              ),
            },
            { path: "conversations/:conversationId", element: <ConversationPage /> },
          ],
        },
      ],
    },
  ];
}

const device = (id: string, name: string) => ({ id, name });
const ACME = [device("a-1", "Acme Compressor"), device("a-2", "Acme Boiler"), device("a-3", "Acme Meter")];
const BETA = [device("b-1", "Beta Chiller"), device("b-2", "Beta Pump"), device("b-3", "Beta Fan")];

const suggested = async () => within(await screen.findByRole("region", { name: "Try these questions" }));

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

async function switchTo(name: RegExp) {
  const trigger = await screen.findByRole("button", { name: /^Switch organization/ });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  const menu = await screen.findByRole("menu");
  await act(async () => {
    fireEvent.click(within(menu).getByRole("menuitemradio", { name }));
  });
}

describe("the suggested questions on the empty conversation home", () => {
  test("are the server's templates filled with this tenant's devices, under the composer", async () => {
    const { browser, tenants } = signedInBrowser();
    const server = new FakeSuggestionsServer();
    server.addTenant(tenants[0]?.id ?? "", { hasData: true, devices: ACME });
    renderAt("/", { routes: homeWith(server.api()), identity: browser });
    const region = await suggested();
    expect(region.getAllByRole("button").map((button) => button.textContent)).toEqual([
      "Which device used the most energy today?",
      "Which devices are offline?",
      "Acme Compressor this week compared with last week",
      "When was Acme Boiler abnormal yesterday?",
      "Acme Meter's daily energy this month",
    ]);
    // The composer is there and comes first.
    const composer = screen.getByRole("textbox", { name: "Ask TrustIoT" });
    expect(composer.compareDocumentPosition(region.getAllByRole("button")[0] as HTMLElement)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
  });

  test("pressing one sends it as it is and opens the conversation", async () => {
    const { browser, tenants } = signedInBrowser();
    const suggestions = new FakeSuggestionsServer();
    suggestions.addTenant(tenants[0]?.id ?? "", { hasData: true, devices: ACME });
    const conversations = new FakeQuestionServer();
    const { router } = renderAt("/", {
      routes: homeWith(suggestions.api()),
      identity: browser,
      conversations: conversations.api(),
    });
    click((await suggested()).getByRole("button", { name: "Which devices are offline?" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    const asks = conversations.requests.filter(
      (request) => request.method === "POST" && request.url.pathname.endsWith("/questions"),
    );
    expect(asks.map((request) => request.body)).toEqual([{ question: "Which devices are offline?" }]);
    expect(
      within(await screen.findByRole("list", { name: "Question being answered" })).getByText(
        "Which devices are offline?",
      ),
    ).toBeTruthy();
  });

  test("a question that cannot be sent is explained under its own pill, and the others stay", async () => {
    const { browser, tenants } = signedInBrowser();
    const suggestions = new FakeSuggestionsServer();
    suggestions.addTenant(tenants[0]?.id ?? "", { hasData: true, devices: ACME });
    const conversations = new FakeQuestionServer();
    renderAt("/", { routes: homeWith(suggestions.api()), identity: browser, conversations: conversations.api() });
    conversations.failNextOn("ask", fakeError(403, "quota_exceeded", "error.quota_exceeded"));
    const region = await suggested();
    const pill = region.getByRole("button", { name: "Which devices are offline?" });
    click(pill);
    const alert = await region.findByRole("alert");
    expect(alert.textContent).toBe("The question wasn't sent: Your plan's limit has been reached.");
    expect(pill.getAttribute("aria-describedby")).toBe(alert.id);
    expect(region.getAllByRole("alert")).toHaveLength(1);
  });

  test("use only this tenant's devices, and a switch to another tenant shows only that one's", async () => {
    const { browser, tenants } = signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] });
    const [acme, beta] = tenants;
    const server = new FakeSuggestionsServer();
    server.addTenant(acme?.id ?? "", { hasData: true, devices: ACME });
    server.addTenant(beta?.id ?? "", { hasData: true, devices: BETA });
    renderAt("/", { routes: homeWith(server.api()), identity: browser });

    const texts = () => screen.getAllByRole("button").map((button) => button.textContent ?? "");
    await screen.findByRole("button", { name: "Acme Compressor this week compared with last week" });
    expect(texts().filter((text) => text.includes("Beta"))).toEqual([]);
    // Every ask so far was for the first tenant (the page may ask twice while the session settles).
    expect([...new Set(server.calls)]).toEqual([acme?.id ?? ""]);

    await switchTo(/Beta Plant/);
    await screen.findByRole("button", { name: "Beta Chiller this week compared with last week" });
    // Not one of the first tenant's devices is left, and the server was asked for the second tenant.
    expect(texts().filter((text) => text.includes("Acme"))).toEqual([]);
    expect(texts().filter((text) => text.includes("Beta")).length).toBeGreaterThan(0);
    expect([...new Set(server.calls)]).toEqual([acme?.id ?? "", beta?.id ?? ""]);
    expect(server.calls.at(-1)).toBe(beta?.id ?? "");

    // And back: the first tenant's own, never the second's.
    await switchTo(/Acme Factory/);
    await screen.findByRole("button", { name: "When was Acme Boiler abnormal yesterday?" });
    expect(texts().filter((text) => text.includes("Beta"))).toEqual([]);
  });

  test("for a tenant the server holds nothing about, no other tenant's questions are shown", async () => {
    const { browser, tenants } = signedInBrowser({ tenants: ["Acme Factory", "Gamma Works"] });
    const server = new FakeSuggestionsServer();
    server.addTenant(tenants[0]?.id ?? "", { hasData: true, devices: ACME });
    renderAt("/", { routes: homeWith(server.api()), identity: browser });
    await screen.findByRole("button", { name: "Acme Compressor this week compared with last week" });

    await switchTo(/Gamma Works/);
    expect(await screen.findByText("Suggested questions can't be loaded right now")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Acme/ })).toBeNull();
  });

  test("a tenant without a reading is told what will appear, and has no pills", async () => {
    const { browser, tenants } = signedInBrowser();
    const server = new FakeSuggestionsServer();
    server.addTenant(tenants[0]?.id ?? "", { hasData: false, devices: [] });
    renderAt("/", { routes: homeWith(server.api()), identity: browser });
    const region = within(
      await screen.findByRole("region", {
        name: "Once the first readings arrive, questions based on your devices will appear here.",
      }),
    );
    expect(region.queryAllByRole("button")).toHaveLength(0);
    expect(screen.getByRole("textbox", { name: "Ask TrustIoT" })).toBeTruthy();
  });

  test("a failure takes one calm line with a retry, and the composer works", async () => {
    const { browser, tenants } = signedInBrowser();
    const server = new FakeSuggestionsServer();
    server.addTenant(tenants[0]?.id ?? "", { hasData: true, devices: ACME });
    server.failNext();
    renderAt("/", { routes: homeWith(server.api()), identity: browser });
    expect(await screen.findByText("Suggested questions can't be loaded right now")).toBeTruthy();
    // Not an alert: the composer above does not depend on it.
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByRole("textbox", { name: "Ask TrustIoT" })).toBeTruthy();

    click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("button", { name: "Which devices are offline?" });
    expect(screen.queryByText("Suggested questions can't be loaded right now")).toBeNull();
  });

  test("shows at most six", async () => {
    const { browser } = signedInBrowser();
    const many: SuggestionsApi = {
      listSuggestions: async () => ({
        has_data: true,
        suggestions: Array.from({ length: 9 }, (_, index) => ({
          id: `s-${index}`,
          text: `Question ${index}?`,
          category: "trend" as const,
          template: "t",
        })),
      }),
    };
    renderAt("/", { routes: homeWith(many), identity: browser });
    const region = await suggested();
    expect(region.getAllByRole("button").map((button) => button.textContent)).toEqual([
      "Question 0?",
      "Question 1?",
      "Question 2?",
      "Question 3?",
      "Question 4?",
      "Question 5?",
    ]);
  });
});

describe("the production home page", () => {
  test("shows no suggested questions: their endpoint is not in this build", async () => {
    renderAt("/");
    await screen.findByRole("textbox", { name: "Ask TrustIoT" });
    expect(screen.queryByRole("heading", { name: "Try these questions" })).toBeNull();
    expect(document.body.textContent).not.toContain("Suggested questions");
  });
});
