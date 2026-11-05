import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../../api/errors.ts";
import { FakePromptServer, FIXED_SECTIONS } from "../../../test/fake-prompts.ts";
import { renderPlatformAi } from "../../../test/render-platform-ai.tsx";
import { PROMPTS_PATH, promptPath } from "./paths.ts";

afterEach(() => {
  cleanup();
});

function open(server: FakePromptServer, path = PROMPTS_PATH, options: { platformAdmin?: boolean } = {}) {
  return renderPlatformAi(path, { prompts: server.api(), ...options });
}

async function editor(): Promise<HTMLTextAreaElement> {
  return (await screen.findByRole("textbox", { name: "Editable text" })) as HTMLTextAreaElement;
}

function type(field: HTMLTextAreaElement, text: string) {
  fireEvent.change(field, { target: { value: text } });
}

describe("the system prompt page", () => {
  test("lists the prompts by service with their state and opens the first one", async () => {
    open(new FakePromptServer());
    const list = await screen.findByRole("navigation", { name: "Prompts" });
    expect(within(list).getByText("AI assistant")).toBeDefined();
    const main = within(list).getByRole("link", { name: /main/ });
    expect(main.getAttribute("aria-current")).toBe("page");
    expect(within(main).getByText("Modified")).toBeDefined();
    expect(within(main).getByText("Default updated")).toBeDefined();
    expect(within(main).getByText("v12 · Oct 31, 2026, 9:20 AM")).toBeDefined();
    expect(within(list).getByRole("link", { name: /router/ }).textContent).toContain("On hold");
    expect(within(list).getByRole("link", { name: /^worker\b/ }).textContent).toContain("Default");
    expect(within(list).getByText(/appear here once they are connected/)).toBeDefined();

    expect((await editor()).value).toContain("Answer in {{user_language}}");
    expect(screen.getByText(/Published v12 · /)).toBeDefined();
    expect(screen.getByRole("link", { name: "Versions and differences" }).getAttribute("href")).toBe(
      "/platform/ai/prompts/assistant/main/versions",
    );
  });

  test("a user who is not a platform administrator gets the no-permission page", async () => {
    const server = new FakePromptServer();
    open(server, PROMPTS_PATH, { platformAdmin: false });
    expect(await screen.findByRole("heading", { name: "No permission" })).toBeDefined();
    expect(server.calls).toEqual([]);
  });

  test("the fixed sections are shown as text that cannot be edited, and the prompt's text is never markup", async () => {
    const server = new FakePromptServer();
    server.add("assistant", "tricky", {
      versions: [
        {
          version: 1,
          origin: "default",
          source: "2026.10",
          content: "Say <b>hello</b> <img src=x onerror=alert(1)>",
          note: null,
          at: "2026-10-22T03:00:00Z",
          by: "system",
        },
      ],
      defaultLabel: "2026.10",
    });
    open(server, promptPath("assistant", "tricky"));
    const field = await editor();
    expect(field.value).toBe("Say <b>hello</b> <img src=x onerror=alert(1)>");
    expect(document.querySelector("main img")).toBeNull();
    expect(document.querySelector("main b")).toBeNull();

    const section = screen.getByRole("region", { name: "Fixed section: Safety guardrails (J5)" });
    expect(within(section).getByText("Part of the code: visible, not editable")).toBeDefined();
    const text = section.querySelector("pre");
    expect(text?.textContent).toBe(FIXED_SECTIONS[0]?.content);
    // Not a field: there is no text box, text area or editable element in it.
    expect(within(section).queryByRole("textbox")).toBeNull();
    expect(section.querySelector("textarea, input, [contenteditable]")).toBeNull();
    // The only text box on the page is the editable part.
    expect(screen.getAllByRole("textbox")).toHaveLength(1);
  });

  test("an unknown variable is named with its line, and both buttons stay off until it is gone", async () => {
    open(new FakePromptServer());
    const field = await editor();
    expect(screen.getByRole("button", { name: "Publish…" }).hasAttribute("disabled")).toBe(true);
    type(field, `${field.value}\nCall the user {{customer_name}}.`);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toBe(
      "Unknown variable {{customer_name}} (line 6). It cannot be saved: only the variables listed below can be used.",
    );
    expect(field.getAttribute("aria-invalid")).toBe("true");
    expect(field.getAttribute("aria-describedby")).toBe(alert.id);
    expect(screen.getByText("Draft, not published · 1 problems")).toBeDefined();
    expect(screen.getByRole("button", { name: "Publish…" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Run the question bank first…" }).hasAttribute("disabled")).toBe(true);
    type(field, field.value.replace("{{customer_name}}", "{{tenant_name}}"));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(screen.getByRole("button", { name: "Publish…" }).hasAttribute("disabled")).toBe(false);
  });

  test("a variable button inserts it at the cursor", async () => {
    open(new FakePromptServer());
    const field = await editor();
    field.focus();
    field.setSelectionRange(0, 0);
    fireEvent.click(screen.getByRole("button", { name: "{{today}}" }));
    await waitFor(() => expect(field.value.startsWith("{{today}}You are")).toBe(true));
  });

  test("publishing shows the difference first, then sends the draft with the version it was edited from", async () => {
    const server = new FakePromptServer();
    open(server);
    const field = await editor();
    type(
      field,
      field.value.replace("Make every chart a component.", "Make every chart a component.\nKeep answers short."),
    );
    fireEvent.click(screen.getByRole("button", { name: "Publish…" }));

    const dialog = await screen.findByRole("alertdialog", { name: "Publish v13 of main?" });
    const diff = within(dialog).getByRole("region", { name: "Difference between v12 and the draft" });
    expect(within(diff).getByText("+1 −0")).toBeDefined();
    expect(within(diff).getByText("Keep answers short.")).toBeDefined();
    expect(within(diff).getByText("Added")).toBeDefined();
    expect(within(dialog).getByText("This draft has not been run on the question bank")).toBeDefined();
    fireEvent.change(within(dialog).getByLabelText("Version note (optional)"), {
      target: { value: "shorter answers" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "Publish v13" }));

    expect(await screen.findByText("Published v13 of main.")).toBeDefined();
    await waitFor(() => expect(screen.getByText(/Published v13 · /)).toBeDefined());
    const versions = await server.api().listVersions("assistant", "main");
    expect(versions.versions[0]).toMatchObject({ version: 13, origin: "edit", note: "shorter answers" });
    // The draft is gone: the text box shows the published text and nothing is pending.
    expect((await editor()).value).toContain("Keep answers short.");
    expect(screen.getByRole("button", { name: "Publish…" }).hasAttribute("disabled")).toBe(true);
  });

  test("the note is optional, and a refusal from the service is shown in the dialog", async () => {
    const server = new FakePromptServer();
    open(server);
    const field = await editor();
    type(field, `${field.value}\nMore.`);
    fireEvent.click(screen.getByRole("button", { name: "Publish…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Publish v13 of main?" });
    server.failNext("publish", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Publish v13" }));
    expect(await within(dialog).findByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Publish v13" }));
    expect(await screen.findByText("Published v13 of main.")).toBeDefined();
  });

  test("someone publishing first stops the publish, keeps the draft and offers a comparison", async () => {
    const server = new FakePromptServer();
    open(server);
    const field = await editor();
    type(field, `${field.value}\nMine.`);
    fireEvent.click(screen.getByRole("button", { name: "Publish…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Publish v13 of main?" });
    server.publishBehindBack("assistant", "main", "Theirs.\nAnd more.");
    fireEvent.click(within(dialog).getByRole("button", { name: "Publish v13" }));

    const stale = await screen.findByText("ops@example.com published v13");
    expect(stale).toBeDefined();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(field.value).toContain("Mine.");
    expect(screen.getByRole("button", { name: "Publish…" }).hasAttribute("disabled")).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Compare v13 with my draft" }));
    const compare = await screen.findByRole("alertdialog", { name: "v13 and my draft" });
    expect(within(compare).getByRole("region", { name: "v13 and my draft" })).toBeDefined();
    fireEvent.click(within(compare).getByRole("button", { name: "Keep editing on top of v13" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Publish…" }).hasAttribute("disabled")).toBe(false));
    expect(screen.queryByText("ops@example.com published v13")).toBeNull();
  });

  test("discarding a draft and leaving with one both ask first, and a draft never leaves the tab", async () => {
    const server = new FakePromptServer();
    const { router } = open(server);
    const field = await editor();
    type(field, "A new draft");
    fireEvent.click(screen.getByRole("button", { name: "Discard the draft" }));
    const discard = await screen.findByRole("alertdialog", { name: "Discard the draft of main?" });
    fireEvent.click(within(discard).getByRole("button", { name: "Stay here" }));
    expect(field.value).toBe("A new draft");

    // Opening another prompt with a draft asks; staying keeps the draft.
    fireEvent.click(screen.getByRole("link", { name: /worker_sql/ }));
    const leave = await screen.findByRole("alertdialog", { name: "Discard the draft of main?" });
    expect(within(leave).getByText(/only exists in this browser tab/)).toBeDefined();
    fireEvent.click(within(leave).getByRole("button", { name: "Stay here" }));
    expect(router.state.location.pathname).toBe(PROMPTS_PATH);
    expect(field.value).toBe("A new draft");
    fireEvent.click(screen.getByRole("link", { name: /worker_sql/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Discard and leave" }));
    await waitFor(() => expect(router.state.location.pathname).toBe(promptPath("assistant", "worker_sql")));
    expect((await editor()).value).toContain("Instructions of worker_sql");
    // The service never saw the draft.
    expect(server.calls.filter((call) => call.startsWith("publish"))).toEqual([]);
  });

  test("the notice that the default moved links to the difference and can be set aside", async () => {
    open(new FakePromptServer());
    expect(await screen.findByText("Default updated (2026.11)")).toBeDefined();
    expect(screen.getByRole("link", { name: "See the default's changes" }).getAttribute("href")).toBe(
      "/platform/ai/prompts/assistant/main/versions#default",
    );
    fireEvent.click(screen.getByRole("button", { name: "Not now" }));
    expect(screen.queryByText("Default updated (2026.11)")).toBeNull();
  });

  test("a prompt that cannot be read says so without hiding the others, and can be retried", async () => {
    const server = new FakePromptServer();
    server.failNext("getPrompt", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    open(server);
    expect(await screen.findByText("The prompt of main cannot be read right now")).toBeDefined();
    expect(screen.getByRole("navigation", { name: "Prompts" })).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await editor()).toBeDefined();
  });

  test("with no prompt the page says so; a list that cannot be read can be retried", async () => {
    const empty = new FakePromptServer({ seed: "empty" });
    empty.failNext("listPrompts", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    open(empty);
    expect(await screen.findByText("The prompt list cannot be read right now")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("There is no prompt yet")).toBeDefined();
  });
});

describe("running the question bank on a draft", () => {
  async function draftReady(server: FakePromptServer) {
    open(server);
    const field = await editor();
    type(field, `${field.value}\nA line.`);
    return field;
  }

  test("picks a part of the bank, shows progress while it runs and the report when it is done, and publishes from it", async () => {
    const server = new FakePromptServer();
    await draftReady(server);
    fireEvent.click(screen.getByRole("button", { name: "Run the question bank first…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Run the analysis question bank on the draft" });
    expect(
      await within(dialog).findByText(
        "84 questions selected, each run 3 times. About US$14 (by the last cost per question).",
      ),
    ).toBeDefined();
    fireEvent.click(within(dialog).getByRole("checkbox", { name: "Dashboards (22 questions)" }));
    expect(within(dialog).getByText(/^106 questions selected/)).toBeDefined();
    fireEvent.click(within(dialog).getByRole("checkbox", { name: "Dashboards (22 questions)" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Start the run" }));

    expect(await screen.findByText("Running: 0 of 252")).toBeDefined();
    act(() => server.tick());
    await waitFor(() => expect(screen.getByText("Running: 63 of 252")).toBeDefined(), { timeout: 4000 });
    // Only every ten percent is announced: 63 of 252 is 25 percent, announced as 20.
    expect(screen.getByText("Run progress 20%")).toBeDefined();
    act(() => {
      server.tick();
      server.tick();
      server.tick();
    });

    const report = await screen.findByText("Run report: draft vs v12", {}, { timeout: 4000 });
    const card = report.closest("section") as HTMLElement;
    const table = within(card).getByRole("table");
    const rows = within(table)
      .getAllByRole("row")
      .slice(1)
      .map((row) => row.textContent);
    expect(rows[0]).toContain("Accuracy");
    expect(rows[0]).toContain("88.1%");
    expect(rows[0]).toContain("90.5%");
    expect(rows[0]).toContain("+2.4（better）");
    expect(rows[2]).toContain("+0.005（worse）");
    expect(rows[3]).toContain("+0.2 / +0.6（worse）");
    expect(within(card).getByText("2 questions are newly wrong:")).toBeDefined();
    expect(within(card).getByText("Which air compressor goes offline most often?")).toBeDefined();

    fireEvent.click(within(card).getByRole("button", { name: "Publish v13…" }));
    const publish = await screen.findByRole("alertdialog", { name: "Publish v13 of main?" });
    // A report exists for this draft, so the dialog does not offer to run the bank again.
    expect(within(publish).queryByText("This draft has not been run on the question bank")).toBeNull();
    expect(within(publish).queryByRole("button", { name: "Run the question bank first…" })).toBeNull();
    fireEvent.click(within(publish).getByRole("button", { name: "Publish v13" }));
    await screen.findByText("Published v13 of main.");
    const first = (await server.api().listVersions("assistant", "main")).versions[0];
    expect(first?.evaluated).toBe(true);
  });

  test("a run in progress can be cancelled, and only one run per prompt can start", async () => {
    const server = new FakePromptServer();
    await draftReady(server);
    fireEvent.click(screen.getByRole("button", { name: "Run the question bank first…" }));
    fireEvent.click(await screen.findByRole("button", { name: "Start the run" }));
    await screen.findByText(/^Running:/);
    // The button that starts a run is off while this page shows one.
    expect(screen.getByRole("button", { name: "Run the question bank first…" }).hasAttribute("disabled")).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Cancel the run" }));
    expect(await screen.findByText("The run was cancelled.")).toBeDefined();
  });

  test("a second run of the same prompt is refused by the service and says so", async () => {
    const server = new FakePromptServer();
    await draftReady(server);
    // Another administrator's run is on the prompt already.
    await server.api().startEval("assistant", "main", { content: "x", subsets: ["usage"] });
    fireEvent.click(screen.getByRole("button", { name: "Run the question bank first…" }));
    fireEvent.click(await screen.findByRole("button", { name: "Start the run" }));
    expect(await screen.findByText("This prompt already has a run in progress. Wait for it to finish.")).toBeDefined();
  });

  test("a run that was started before the page opened keeps showing its progress", async () => {
    const server = new FakePromptServer();
    await server.api().startEval("assistant", "main", { content: "x", subsets: ["usage"] });
    open(server);
    expect(await screen.findByText("Running: 0 of 144")).toBeDefined();
    expect(screen.getByText("You can leave this page; the progress shows again when you return.")).toBeDefined();
  });
});
