import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../../api/errors.ts";
import { FakePromptServer } from "../../../test/fake-prompts.ts";
import { renderPlatformAi } from "../../../test/render-platform-ai.tsx";
import { promptPath, promptVersionsPath } from "./paths.ts";

afterEach(() => {
  cleanup();
});

function open(server: FakePromptServer, role = "main") {
  return renderPlatformAi(promptVersionsPath("assistant", role), { prompts: server.api() });
}

async function row(version: number): Promise<HTMLElement> {
  const checkbox = await screen.findByRole("checkbox", { name: `Pick v${version} to compare` });
  return checkbox.closest("tr") as HTMLElement;
}

describe("the versions page", () => {
  test("lists the versions newest first with their source, who and when, and which one is in use", async () => {
    open(new FakePromptServer());
    expect(await screen.findByRole("heading", { level: 1, name: "Versions of main" })).toBeDefined();
    const v12 = await row(12);
    expect(within(v12).getByText("In use")).toBeDefined();
    expect(within(v12).getByText("platform-admin@example.com")).toBeDefined();
    expect(within(v12).getByText("Numbers come with their periods")).toBeDefined();
    // The version in use has nothing to revert to; the older ones do.
    expect(within(v12).queryByRole("button", { name: "Revert to this version…" })).toBeNull();
    expect(within(await row(11)).getByText("Revert to v9")).toBeDefined();
    expect(within(await row(10)).getByText("Edit, run on the question bank")).toBeDefined();
    expect(within(await row(10)).getByText("—")).toBeDefined();
    // A page of four: the earliest version is behind "load earlier versions".
    expect(screen.queryByRole("checkbox", { name: "Pick v9 to compare" })).not.toBeNull();
  });

  test("the first page holds the latest versions and the rest load on request", async () => {
    const server = new FakePromptServer();
    server.add("assistant", "long", {
      versions: Array.from({ length: 6 }, (_, index) => ({
        version: index + 1,
        origin: "edit" as const,
        source: null,
        content: `text ${index + 1}`,
        note: null,
        at: "2026-10-2" + index + "T03:00:00Z",
        by: "ops@example.com",
      })),
      defaultLabel: "2026.10",
    });
    open(server, "long");
    await row(6);
    expect(screen.queryByRole("checkbox", { name: "Pick v2 to compare" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Load earlier versions" }));
    await row(1);
    expect(screen.queryByRole("button", { name: "Load earlier versions" })).toBeNull();
  });

  test("two picked versions are compared, merged on a narrow screen and side by side on request", async () => {
    open(new FakePromptServer());
    fireEvent.click(within(await row(11)).getByRole("checkbox"));
    fireEvent.click(within(await row(12)).getByRole("checkbox"));
    expect(screen.getByText("v11 and v12 selected")).toBeDefined();
    // Narrow by default in the test (no media query matches): merged, with the signs and words.
    const merged = (await screen.findAllByRole("region", { name: "v11 → v12" })).at(-1) as HTMLElement;
    expect(within(merged).getAllByText("Added").length).toBeGreaterThan(0);
    expect(within(merged).getAllByText("Deleted").length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("button", { name: "Side by side" }));
    expect(screen.getByRole("button", { name: "Side by side" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "Merged" }).getAttribute("aria-pressed")).toBe("false");
    // A third pick replaces the oldest one: at most two are compared.
    fireEvent.click(within(await row(10)).getByRole("checkbox"));
    expect(screen.getByText("v10 and v12 selected")).toBeDefined();
  });

  test("a version with no history shows that the prompt still uses the default", async () => {
    open(new FakePromptServer(), "worker");
    expect(await screen.findByText("worker still uses the default")).toBeDefined();
    expect(screen.getByRole("link", { name: "Edit worker" }).getAttribute("href")).toBe(
      promptPath("assistant", "worker"),
    );
    expect(screen.queryByRole("table")).toBeNull();
  });

  test("reverting shows the difference, needs a reason, and publishes the old content as a new version", async () => {
    const server = new FakePromptServer();
    open(server);
    fireEvent.click(within(await row(9)).getByRole("button", { name: "Revert to this version…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Revert main to v9?" });
    expect(within(dialog).getByText(/Publishes the content of v9 as v13; every older version stays/)).toBeDefined();
    const diff = await within(dialog).findByRole("region", {
      name: "Difference between the version in use and the one after the revert",
    });
    expect(within(diff).getAllByText("Deleted").length).toBeGreaterThan(0);

    // No reason, no revert: the field says so and nothing is sent.
    fireEvent.click(within(dialog).getByRole("button", { name: "Revert to v9 (publish v13)" }));
    expect(await within(dialog).findByText("Write why you are reverting.")).toBeDefined();
    expect(server.calls.filter((call) => call.startsWith("revert"))).toEqual([]);

    fireEvent.change(within(dialog).getByLabelText("Reason for reverting (required)"), {
      target: { value: "v12 gives some users the wrong language" },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "Revert to v9 (publish v13)" }));
    expect(await screen.findByText("Reverted main to v9; it is now v13.")).toBeDefined();
    const newest = await row(13);
    expect(within(newest).getByText("In use")).toBeDefined();
    expect(within(newest).getByText("Revert to v9")).toBeDefined();
    expect(within(newest).getByText("v12 gives some users the wrong language")).toBeDefined();
    // The old version is still there, and v13 has v9's content.
    expect(await row(12)).toBeDefined();
    expect((await server.api().getVersion("assistant", "main", 13)).content).toBe(
      (await server.api().getVersion("assistant", "main", 9)).content,
    );
  });

  test("a revert that meets a newer version says nothing was reverted and reads the versions again", async () => {
    const server = new FakePromptServer();
    open(server);
    fireEvent.click(within(await row(9)).getByRole("button", { name: "Revert to this version…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Revert main to v9?" });
    await within(dialog).findByRole("region", { name: /Difference between/ });
    fireEvent.change(within(dialog).getByLabelText("Reason for reverting (required)"), {
      target: { value: "back out" },
    });
    server.publishBehindBack("assistant", "main", "Theirs");
    fireEvent.click(within(dialog).getByRole("button", { name: "Revert to v9 (publish v13)" }));
    expect(
      await within(dialog).findByText(/Someone published a newer version first, so nothing was reverted/),
    ).toBeDefined();
    await waitFor(() =>
      expect(server.calls.filter((call) => call.startsWith("listVersions")).length).toBeGreaterThan(1),
    );
  });

  test("the default's change is shown with its three choices", async () => {
    const server = new FakePromptServer();
    const { router } = open(server);
    const card = (await screen.findByText("New default 2026.11")).closest("section") as HTMLElement;
    expect(
      within(card).getByText(/You are on v12\. Below is what the default itself changed from 2026\.10 to 2026\.11/),
    ).toBeDefined();
    const diff = within(card).getByRole("region", { name: "Difference between the defaults" });
    expect(within(diff).getByText("For queries over 31 days use the aggregated data, not raw readings.")).toBeDefined();
    expect(within(diff).getByText("+1 −0")).toBeDefined();

    // "Add this to my version" opens the editor with the default's change applied to the published text.
    fireEvent.click(within(card).getByRole("button", { name: "Add this to my version…" }));
    await waitFor(() => expect(router.state.location.pathname).toBe(promptPath("assistant", "main")));
    const field = (await screen.findByRole("textbox", { name: "Editable text" })) as HTMLTextAreaElement;
    await waitFor(() => expect(field.value).toContain("For queries over 31 days use the aggregated data"));
    expect(field.value).toContain("Answer in {{user_language}}");
    // It is a draft, not a version: nothing was published.
    expect(server.calls.filter((call) => call.startsWith("publish"))).toEqual([]);
    expect(screen.getByText(/Draft, not published/)).toBeDefined();
  });

  test("using the new default shows the full difference and publishes it as the next version", async () => {
    const server = new FakePromptServer();
    open(server);
    const card = (await screen.findByText("New default 2026.11")).closest("section") as HTMLElement;
    fireEvent.click(within(card).getByRole("button", { name: "Use the new default…" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Use the new default 2026.11?" });
    expect(
      within(dialog).getByRole("region", { name: "Difference between the version in use and the new default" }),
    ).toBeDefined();
    fireEvent.click(within(dialog).getByRole("button", { name: "Use the new default (publish v13)" }));
    expect(await screen.findByText("Now on the default 2026.11, as v13.")).toBeDefined();
    const newest = await row(13);
    expect(within(newest).getByText("Applied default 2026.11")).toBeDefined();
    await waitFor(() => expect(screen.queryByText("New default 2026.11")).toBeNull());
  });

  test("setting the default aside keeps the notice on the prompt page", async () => {
    const server = new FakePromptServer();
    open(server);
    const card = (await screen.findByText("New default 2026.11")).closest("section") as HTMLElement;
    fireEvent.click(within(card).getByRole("button", { name: "Not now (the notice stays)" }));
    expect(screen.queryByText("New default 2026.11")).toBeNull();
    expect(server.calls.filter((call) => call.startsWith("adoptDefault"))).toEqual([]);
  });

  test("versions that cannot be read say so and can be retried", async () => {
    const server = new FakePromptServer();
    server.failNext("listVersions", new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    open(server);
    expect(await screen.findByText("The versions cannot be read right now")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await row(12)).toBeDefined();
  });
});
