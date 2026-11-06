import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { fakeError } from "../../../test/fake-dashboards.ts";
import { FakeModelRoleServer } from "../../../test/fake-model-roles.ts";
import { renderAt, signedInBrowser } from "../../../test/render.tsx";
import { MODEL_ROLES_PATH } from "./paths.ts";

afterEach(() => {
  cleanup();
});

function open(server: FakeModelRoleServer, options: { platformAdmin?: boolean } = {}) {
  const { browser } = signedInBrowser({ platformAdmin: options.platformAdmin ?? true });
  return renderAt(MODEL_ROLES_PATH, { identity: browser, modelRoles: server.api() });
}

async function card(name: string): Promise<HTMLElement> {
  const heading = await screen.findByRole("heading", { name });
  const section = heading.closest("section");
  if (section === null) throw new Error(`no card for ${name}`);
  return section;
}

describe("the model roles page", () => {
  test("lists each role with its models in order, where they come from and what it can do", async () => {
    open(new FakeModelRoleServer());
    expect(await screen.findByRole("heading", { level: 1, name: "Model roles" })).toBeDefined();

    const main = await card("main");
    const models = within(within(main).getByRole("list", { name: "Models of main, in the order tried" }));
    expect(models.getAllByRole("listitem").map((item) => item.textContent)).toEqual([
      "1anthropic/claude-opus-5-5Primary",
    ]);
    expect(within(main).getByText("Changed")).toBeDefined();
    expect(within(main).getByText("No fallback")).toBeDefined();
    expect(within(main).getByText(/Default: anthropic\/claude-opus-5-5 → anthropic\/claude-sonnet-5-5/)).toBeDefined();
    expect(within(main).getByText(/Version 14 · /)).toBeDefined();
    expect(within(main).getByRole("button", { name: "Reset main to its default" })).toBeDefined();

    const sql = await card("worker_sql");
    const sqlModels = within(sql)
      .getAllByRole("listitem")
      .map((item) => item.textContent);
    expect(sqlModels).toEqual(["1anthropic/claude-sonnet-5-5Primary", "2anthropic/claude-opus-5-5Fallback"]);
    expect(within(sql).getByText("Default")).toBeDefined();
    expect(within(sql).getByText("Version 0 · never changed (the configured default)")).toBeDefined();
    expect(within(sql).queryByRole("button", { name: /^Reset/ })).toBeNull();

    const complete = await card("complete");
    expect(within(complete).getByText("The stored list is not in use")).toBeDefined();
    expect(within(complete).getByText(/anthropic-old\/claude-haiku-4-5/)).toBeDefined();
    expect(within(complete).getByText("Primary (default)")).toBeDefined();
    expect(within(complete).getByRole("button", { name: "Reset complete to its default" })).toBeDefined();

    expect(within(await card("router")).getByText("OpenAI Decisions")).toBeDefined();
  });

  test("a user who is not a platform administrator gets the no-permission page and nothing is read", async () => {
    const server = new FakeModelRoleServer();
    open(server, { platformAdmin: false });
    expect(
      await screen.findByRole("heading", { name: "You don't have permission to open the platform admin" }),
    ).toBeDefined();
    expect(screen.queryByRole("heading", { name: "Model roles" })).toBeNull();
    expect(server.requests).toEqual([]);
  });

  test("shows the sections of the platform AI group and marks the current one", async () => {
    open(new FakeModelRoleServer());
    const nav = await screen.findByRole("navigation", { name: "Platform admin sections" });
    expect(within(nav).getByRole("link", { name: "Model roles" }).getAttribute("aria-current")).toBe("page");
    // The pages whose API is pending are not in the product's navigation (the product's routes).
    expect(
      within(nav)
        .getAllByRole("link")
        .map((link) => link.textContent),
    ).toEqual(["Model roles"]);
  });

  test("/platform in the product goes to the model roles, the only section it has", async () => {
    const { router } = renderAt("/platform", {
      identity: signedInBrowser({ platformAdmin: true }).browser,
      modelRoles: new FakeModelRoleServer().api(),
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Model roles" })).toBeDefined();
    expect(router.state.location.pathname).toBe(MODEL_ROLES_PATH);
  });

  test("the back office's pages whose API is pending are not in the product's routes", async () => {
    for (const path of ["/platform/tenants", "/platform/audit-log", "/platform/ai/prompts", "/platform/ai/usage"]) {
      renderAt(path, {
        identity: signedInBrowser({ platformAdmin: true }).browser,
        modelRoles: new FakeModelRoleServer().api(),
      });
      expect(await screen.findByRole("heading", { level: 1, name: "Page not found" })).toBeDefined();
      cleanup();
    }
  });

  test("an empty gateway and a read failure each say so, and the failure can be retried", async () => {
    const server = new FakeModelRoleServer({ seed: "empty" });
    server.failNext(fakeError(503, "unavailable", "error.unavailable"));
    open(server);
    expect(await screen.findByText("The model roles cannot be read right now")).toBeDefined();
    expect(screen.getByText(/Nothing was changed\./)).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("The AI gateway has no roles configured.")).toBeDefined();
  });
});

async function startChange(role = "main") {
  const target = await card(role);
  fireEvent.click(within(target).getByRole("button", { name: `Change the models of ${role}` }));
  return screen.findByRole("alertdialog");
}

function addModel(dialog: HTMLElement, id: string) {
  fireEvent.change(within(dialog).getByLabelText("Model ID"), { target: { value: id } });
  fireEvent.click(within(dialog).getByRole("button", { name: "Add" }));
}

describe("changing a role's models", () => {
  test("edits the list, shows the before and after, then sends the change with the version that was read", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    // Focus starts on Cancel, and the provider list holds only providers of the role's protocol.
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(within(dialog).getByRole("option", { name: "anthropic" })).toBeDefined();
    expect(within(dialog).queryByRole("option", { name: "openai" })).toBeNull();
    expect(within(dialog).getByRole("button", { name: "Next: review the change" }).hasAttribute("disabled")).toBe(true);

    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Move claude-sonnet-5-5 up" }));
    expect(within(dialog).getByRole("status").textContent).toBe("claude-sonnet-5-5 moved to position 1.");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));

    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    expect(within(confirm).getByText("Now (version 14)")).toBeDefined();
    const added = within(confirm)
      .getAllByRole("listitem")
      .filter((item) => /added/.test(item.textContent ?? ""));
    expect(added.map((item) => item.textContent)).toEqual(["1anthropic/claude-sonnet-5-5Primary · added"]);
    expect(within(confirm).getByText(/prompt cache is kept per model/)).toBeDefined();
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));

    expect(await screen.findByText("Changed the models of main (version 15).")).toBeDefined();
    expect(server.requests.at(-1)?.method).toBe("PUT");
    expect(server.requests.at(-1)?.body).toEqual({
      models: ["anthropic/claude-sonnet-5-5", "anthropic/claude-opus-5-5"],
      expected_version: 14,
    });
    const main = await card("main");
    await waitFor(() => expect(within(main).getAllByRole("listitem")).toHaveLength(2));
    expect(within(main).queryByText("No fallback")).toBeNull();
    expect(within(main).getByText(/Version 15 · /)).toBeDefined();
  });

  test("checks a model ID before it is added: no spaces or commas, no repeats, at most eight", async () => {
    open(new FakeModelRoleServer());
    const dialog = await startChange("worker_sql");
    const add = () => within(dialog).getByRole("button", { name: "Add" });
    fireEvent.change(within(dialog).getByLabelText("Model ID"), { target: { value: "claude haiku" } });
    expect(within(dialog).getByText("A model ID cannot contain spaces or commas.")).toBeDefined();
    expect(add().hasAttribute("disabled")).toBe(true);
    fireEvent.change(within(dialog).getByLabelText("Model ID"), { target: { value: "claude-opus-5-5" } });
    expect(within(dialog).getByText("anthropic/claude-opus-5-5 is already in the list.")).toBeDefined();
    expect(add().hasAttribute("disabled")).toBe(true);
    for (let index = 0; index < 6; index += 1) addModel(dialog, `m${index}`);
    fireEvent.change(within(dialog).getByLabelText("Model ID"), { target: { value: "one-more" } });
    expect(within(dialog).getByText("At most 8 models.")).toBeDefined();
    expect(add().hasAttribute("disabled")).toBe(true);
  });

  test("a list can lose its last model only on the way to adding another: it needs one to go on", async () => {
    open(new FakeModelRoleServer());
    const dialog = await startChange();
    fireEvent.click(within(dialog).getByRole("button", { name: "Remove claude-opus-5-5" }));
    expect(within(dialog).getByText("At least one model is needed.")).toBeDefined();
    expect(within(dialog).getByRole("button", { name: "Next: review the change" }).hasAttribute("disabled")).toBe(true);
  });

  test("a refused list comes back to the editor with the server's reason, and nothing is changed", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    server.failNext(
      new Response(
        JSON.stringify({
          code: "validation_failed",
          message_key: "error.validation_failed",
          field_errors: [{ field: "/models", message_key: "ai_model_role.protocol_mismatch" }],
        }),
        { status: 422, headers: { "content-type": "application/json" } },
      ),
    );
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));
    // The editor is back with the reason beside the list, and the draft is still there.
    expect(await screen.findByText(/speaks another protocol than this role/)).toBeDefined();
    expect(screen.getByRole("list", { name: "Order tried" }).children).toHaveLength(2);
  });

  test("a change made by someone else meanwhile keeps the draft and offers the latest version", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    server.changeBehindBack("main", ["anthropic/claude-haiku-5-5"]);
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));

    expect(
      await within(confirm).findByText("Someone else changed main while you were editing (now version 15)"),
    ).toBeDefined();
    expect(within(confirm).getByText(/Your change was not written/)).toBeDefined();
    expect(within(confirm).getByRole("button", { name: "Change main" }).hasAttribute("disabled")).toBe(true);
    fireEvent.click(within(confirm).getByRole("button", { name: "Review my change on version 15" }));

    // The before side is now the latest version; sending it names that version.
    expect(await within(confirm).findByText("Now (version 15)")).toBeDefined();
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));
    expect(await screen.findByText("Changed the models of main (version 16).")).toBeDefined();
    expect(server.requests.at(-1)?.body).toEqual({
      models: ["anthropic/claude-opus-5-5", "anthropic/claude-sonnet-5-5"],
      expected_version: 15,
    });
  });

  test("a failed re-read after a conflict keeps the dialog and the draft", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    // The change is refused as a conflict, and the re-read that follows it fails.
    server.failNext(fakeError(409, "conflict", "ai_model_role.version_conflict", { current_version: 15 }));
    server.failNext(fakeError(503, "unavailable", "error.unavailable"));
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));
    await within(confirm).findByText(/Someone else changed main/);
    // The button reads again, and that fails too: the draft stays and it can be pressed again.
    server.failNext(fakeError(503, "unavailable", "error.unavailable"));
    fireEvent.click(within(confirm).getByRole("button", { name: "Review my change on version 15" }));
    expect(await within(confirm).findByText(/The latest version could not be read/)).toBeDefined();
    expect(screen.getByRole("alertdialog", { name: "Confirm the change to main" })).toBeDefined();
  });

  test("an unknown outcome is not sent again: the roles are read and the change is reported as taken", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    server.outcomeUnknownNext();
    const before = server.requests.filter((request) => request.method === "PUT").length;
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));

    expect(
      await within(confirm).findByText("Confirmed by reading again: the change took effect (version 15)"),
    ).toBeDefined();
    expect(server.requests.filter((request) => request.method === "PUT")).toHaveLength(before + 1);
    expect(within(confirm).getByText(/operation log may lack this entry/)).toBeDefined();
    fireEvent.click(within(confirm).getByRole("button", { name: "Close" }));
    const main = await card("main");
    await waitFor(() => expect(within(main).getAllByRole("listitem")).toHaveLength(2));
  });

  test("an unknown outcome that did not take effect says so and offers to send it again", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    server.failNext(fakeError(504, "timeout", "ai_model_role.outcome_unknown"));
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));
    expect(
      await within(confirm).findByText("Confirmed by reading again: the change did not take effect"),
    ).toBeDefined();
    expect(within(confirm).getByText("main is still at version 14. You can send it again.")).toBeDefined();
    fireEvent.click(within(confirm).getByRole("button", { name: "Send again" }));
    expect(await screen.findByText("Changed the models of main (version 15).")).toBeDefined();
  });

  test("another failure is shown in the dialog, which stays open for another try", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const dialog = await startChange();
    addModel(dialog, "claude-sonnet-5-5");
    fireEvent.click(within(dialog).getByRole("button", { name: "Next: review the change" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Confirm the change to main" });
    server.failNext(fakeError(503, "unavailable", "error.unavailable"));
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));
    expect(await within(confirm).findByText("The service is temporarily unavailable. Try again later.")).toBeDefined();
    fireEvent.click(within(confirm).getByRole("button", { name: "Change main" }));
    expect(await screen.findByText("Changed the models of main (version 15).")).toBeDefined();
  });
});

describe("resetting a role", () => {
  test("shows the current and the default models, then resets with the version that was read", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const main = await card("main");
    fireEvent.click(within(main).getByRole("button", { name: "Reset main to its default" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Reset main to its default?" });
    expect(within(dialog).getByText("claude-opus-5-5 → claude-sonnet-5-5")).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Reset main" }));

    expect(await screen.findByText("Reset main to its default (version 15).")).toBeDefined();
    expect(server.requests.at(-1)?.method).toBe("DELETE");
    expect(server.requests.at(-1)?.url.searchParams.get("expected_version")).toBe("14");
    await waitFor(() => expect(within(main).getByText("Default")).toBeDefined());
    expect(within(main).queryByRole("button", { name: /^Reset/ })).toBeNull();
  });

  test("clears a stored list that is not in use", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const complete = await card("complete");
    fireEvent.click(within(complete).getByRole("button", { name: "Reset complete to its default" }));
    fireEvent.click(await screen.findByRole("button", { name: "Reset complete" }));
    expect(await screen.findByText("Reset complete to its default (version 10).")).toBeDefined();
    await waitFor(() => expect(within(complete).queryByText("The stored list is not in use")).toBeNull());
  });

  test("a conflict is reported in the dialog and the reset waits for the latest version", async () => {
    const server = new FakeModelRoleServer();
    open(server);
    const main = await card("main");
    fireEvent.click(within(main).getByRole("button", { name: "Reset main to its default" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Reset main to its default?" });
    server.changeBehindBack("main", ["anthropic/claude-haiku-5-5"]);
    fireEvent.click(within(dialog).getByRole("button", { name: "Reset main" }));
    expect(await within(dialog).findByText(/Someone else changed main/)).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Review my change on version 15" }));
    await waitFor(() => expect(within(dialog).queryByText(/Someone else changed main/)).toBeNull());
    fireEvent.click(within(dialog).getByRole("button", { name: "Reset main" }));
    expect(await screen.findByText("Reset main to its default (version 16).")).toBeDefined();
  });
});
