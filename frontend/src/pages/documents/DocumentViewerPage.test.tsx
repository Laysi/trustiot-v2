import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { act } from "react";

import { documentLibrary, openDocuments } from "../../test/documents-render.tsx";
import { PAGE_HEIGHT, PAGE_WIDTH, refusal } from "../../test/fake-documents.ts";
import { documentPath } from "./paths.ts";

afterEach(() => {
  cleanup();
});

/** A library with the demo documents; `manual` is the two-version one (v2, 120 pages, is current). */
function setup() {
  const library = documentLibrary();
  const manual = [...library.documents.documents.values()].find((document) => document.name === "XG-200 使用手冊");
  if (manual === undefined) throw new Error("the demo library has no manual");
  return { library, manual };
}

async function openViewer(
  path: string,
  library: ReturnType<typeof documentLibrary>,
  role?: "viewer" | "editor" | "admin",
) {
  const rendered = openDocuments(path, library, role ? { role } : {});
  await screen.findByRole("heading", { level: 1 });
  return rendered;
}

describe("the page view a citation opens", () => {
  test("shows the cited page with the cited area drawn over it in the image's pixels", async () => {
    const { library, manual } = setup();
    await openViewer(
      documentPath(manual.id, { version: 2, page: 5, box: { x: 60, y: 190, width: 674, height: 160 } }),
      library,
    );
    expect(await screen.findByRole("heading", { level: 1, name: "XG-200 使用手冊" })).toBeTruthy();
    const image = await screen.findByRole("img", {
      name: /^The page image of page 5\. The marked area is the part the AI cited, in the upper part of the page\.$/,
    });
    expect(image.getAttribute("width")).toBe(String(PAGE_WIDTH));
    expect(image.getAttribute("height")).toBe(String(PAGE_HEIGHT));
    const overlay = screen.getByTestId("cited-area");
    expect(overlay.getAttribute("viewBox")).toBe(`0 0 ${PAGE_WIDTH} ${PAGE_HEIGHT}`);
    const frame = overlay.querySelector("rect[stroke-width='3']");
    expect(frame?.getAttribute("x")).toBe("60");
    expect(frame?.getAttribute("y")).toBe("190");
    expect(frame?.getAttribute("width")).toBe("674");
    expect(frame?.getAttribute("height")).toBe("160");
    expect(overlay.querySelector("path")?.getAttribute("d")).toContain("M60 190");
    expect(overlay.textContent).toBe("Area the AI cited");
    // The overlay is drawn with attributes and classes only: no style attribute (the page's CSP).
    expect(overlay.querySelectorAll("[style]")).toHaveLength(0);
    expect(screen.getByText("Opened from a citation")).toBeTruthy();
    expect(screen.getByText(/A page image has no readable text/)).toBeTruthy();
  });

  test("a box that does not fit, a page past the end and an unknown version fall back to the defaults", async () => {
    const { library, manual } = setup();
    await openViewer(`/documents/${manual.id}?version=9&page=999&box=1,2,3`, library);
    expect(await screen.findByRole("img", { name: "The page image of page 1" })).toBeTruthy();
    expect(screen.queryByTestId("cited-area")).toBeNull();
    // The unknown version is the current one, with no "old version" notice.
    expect(screen.queryByText(/You're looking at v/)).toBeNull();
    expect((screen.getByLabelText("Page number") as HTMLInputElement).value).toBe("1");
  });

  test("a box on a page that is not the version's is dropped with the page", async () => {
    const { library, manual } = setup();
    await openViewer(documentPath(manual.id, { page: 999, box: { x: 10, y: 10, width: 100, height: 100 } }), library);
    expect(await screen.findByRole("img", { name: "The page image of page 1" })).toBeTruthy();
    expect(screen.queryByTestId("cited-area")).toBeNull();
    expect(screen.queryByText("Opened from a citation")).toBeNull();
  });

  test("a box outside the page image is not drawn, and the page is not called a citation", async () => {
    const { library, manual } = setup();
    await openViewer(documentPath(manual.id, { page: 3, box: { x: 5000, y: 5000, width: 10, height: 10 } }), library);
    expect(await screen.findByRole("img", { name: "The page image of page 3" })).toBeTruthy();
    expect(screen.queryByTestId("cited-area")).toBeNull();
    expect(screen.queryByText("Opened from a citation")).toBeNull();
  });

  test("without parameters it is the current version, page 1", async () => {
    const { library, manual } = setup();
    await openViewer(`/documents/${manual.id}`, library);
    expect(await screen.findByRole("img", { name: "The page image of page 1" })).toBeTruthy();
    expect(screen.getByRole("combobox", { name: "Version" })).toBeTruthy();
    expect(screen.getByText("Done, the AI can cite it")).toBeTruthy();
  });

  test("an older version than the current one says so and offers the current one", async () => {
    const { library, manual } = setup();
    await openViewer(documentPath(manual.id, { version: 1, page: 3 }), library);
    const note = await screen.findByText("You're looking at v1");
    expect(note.closest("[role=status]")?.textContent).toContain("The current version is v2");
    fireEvent.click(screen.getByRole("button", { name: "See v2" }));
    await waitFor(() => expect(screen.queryByText("You're looking at v1")).toBeNull());
    expect((screen.getByRole("combobox", { name: "Version" }) as HTMLSelectElement).value).toBe("2");
  });

  test("moving to another page keeps the version, replaces the address and drops the cited area", async () => {
    const { library, manual } = setup();
    const { router } = await openViewer(
      documentPath(manual.id, { version: 2, page: 5, box: { x: 10, y: 10, width: 100, height: 100 } }),
      library,
    );
    await screen.findByTestId("cited-area");
    fireEvent.click(screen.getByRole("button", { name: "Next page" }));
    await screen.findByRole("img", { name: "The page image of page 6" });
    expect(screen.queryByTestId("cited-area")).toBeNull();
    expect(router.state.location.search).toBe("?version=2&page=6");
    // Replaced, not pushed: the first entry is still the only one.
    expect(router.state.historyAction).toBe("REPLACE");

    fireEvent.change(screen.getByLabelText("Page number"), { target: { value: "9" } });
    fireEvent.keyDown(screen.getByLabelText("Page number"), { key: "Enter" });
    await screen.findByRole("img", { name: "The page image of page 9" });
    // An entry that is not a page of the document puts the page back.
    fireEvent.change(screen.getByLabelText("Page number"), { target: { value: "500" } });
    fireEvent.blur(screen.getByLabelText("Page number"));
    expect((screen.getByLabelText("Page number") as HTMLInputElement).value).toBe("9");

    fireEvent.click(
      within(screen.getByRole("navigation", { name: "Pages" })).getByRole("link", { name: "Go to page 2" }),
    );
    await screen.findByRole("img", { name: "The page image of page 2" });
    expect(screen.getByRole("button", { name: "Previous page" }).hasAttribute("disabled")).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Previous page" }));
    await screen.findByRole("img", { name: "The page image of page 1" });
    expect(screen.getByRole("button", { name: "Previous page" }).hasAttribute("disabled")).toBe(true);
  });

  test("zooming is a toggle that is announced", async () => {
    const { library, manual } = setup();
    await openViewer(`/documents/${manual.id}`, library);
    await screen.findByRole("img", { name: "The page image of page 1" });
    const zoom = screen.getByRole("button", { name: "Zoom in" });
    expect(zoom.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(zoom);
    expect(screen.getByRole("button", { name: "Zoom out" }).getAttribute("aria-pressed")).toBe("true");
  });

  test("a page that cannot be loaded says so and can be tried again", async () => {
    const { library, manual } = setup();
    library.documents.failNext("getPage", refusal(503, "unavailable", "error.unavailable"));
    await openViewer(`/documents/${manual.id}?page=4`, library);
    expect(await screen.findByText(/Couldn't load page 4/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("img", { name: "The page image of page 4" })).toBeTruthy();
  });

  test("a page image that fails to load is reported, not left blank", async () => {
    const { library, manual } = setup();
    await openViewer(`/documents/${manual.id}?page=4`, library);
    fireEvent.error(await screen.findByRole("img", { name: "The page image of page 4" }));
    expect(await screen.findByText("The image of page 4 couldn't be loaded.")).toBeTruthy();
  });

  test("a deleted document shows that it was deleted and none of its content", async () => {
    const { library, manual } = setup();
    await library.documents.deleteDocument(manual.id);
    await openViewer(documentPath(manual.id, { page: 3 }), library);
    expect(await screen.findByText("This document was deleted")).toBeTruthy();
    expect(screen.getByText(/This document was deleted on .+, and the AI no longer cites it\./)).toBeTruthy();
    expect(screen.getByRole("heading", { level: 1, name: "XG-200 使用手冊" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Back to the library" }).getAttribute("href")).toBe("/documents");
    expect(screen.queryByRole("img")).toBeNull();
    expect(library.documents.calls.some((call) => call.method === "getPage")).toBe(false);
  });

  test("a document that does not exist is not found, with the way back", async () => {
    const { library } = setup();
    openDocuments("/documents/00000000-0000-4000-8000-00000000ffff", library);
    expect(await screen.findByText("This document wasn't found")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Back to the library" })).toBeTruthy();
  });

  test("says the pages are not ready while a version has not been counted", async () => {
    const { library } = setup();
    const queued = [...library.documents.documents.values()].find(
      (document) => document.name === "AC-75 空壓機保養手冊",
    );
    await openViewer(`/documents/${queued?.id}`, library);
    expect(await screen.findByText("The pages aren't ready yet")).toBeTruthy();
    expect(screen.queryByRole("img")).toBeNull();
  });

  test("a plan without document RAG sees the upgrade wall", async () => {
    const { library, manual } = setup();
    library.documents.plan = "starter";
    openDocuments(`/documents/${manual.id}`, library);
    expect(await screen.findByText("The document library is part of Business plans and above")).toBeTruthy();
  });

  test("downloads the original of the version shown", async () => {
    const click = spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    try {
      const { library, manual } = setup();
      await openViewer(documentPath(manual.id, { version: 1 }), library);
      fireEvent.click(await screen.findByRole("button", { name: "Original PDF" }));
      await waitFor(() => expect(click).toHaveBeenCalled());
      expect(library.documents.calls.find((call) => call.method === "getDownloadUrl")?.args[0]).toEqual({
        documentId: manual.id,
        version: 1,
      });
    } finally {
      click.mockRestore();
    }
  });
});

describe("deleting from the page view", () => {
  async function openMenu() {
    await act(async () => {
      fireEvent.keyDown(screen.getByRole("button", { name: /^Actions for/ }), { key: "Enter" });
    });
    return screen.findByRole("menu");
  }

  test("a viewer has no delete", async () => {
    const { library, manual } = setup();
    await openViewer(`/documents/${manual.id}`, library, "viewer");
    await screen.findByRole("img", { name: "The page image of page 1" });
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
    expect(screen.getByRole("button", { name: "Original PDF" })).toBeTruthy();
  });

  test("an editor deletes after confirming and lands on the library with the message", async () => {
    const { library, manual } = setup();
    const { router, queryClient } = await openViewer(`/documents/${manual.id}`, library, "editor");
    await screen.findByRole("img", { name: "The page image of page 1" });
    fireEvent.click(within(await openMenu()).getByRole("menuitem", { name: "Delete document" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete “XG-200 使用手冊”?" });
    expect(within(dialog).getByText(/up to v2/)).toBeTruthy();
    fireEvent.click(within(dialog).getByRole("button", { name: "Delete document" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/documents"));
    expect(await screen.findByText("Deleted “XG-200 使用手冊”. The AI no longer cites it.")).toBeTruthy();
    expect(screen.queryByRole("link", { name: "XG-200 使用手冊" })).toBeNull();
    // What was cached about the document, its details and its page images, went with it.
    expect(queryClient.getQueryCache().findAll({ predicate: (query) => query.queryKey[2] === manual.id })).toHaveLength(
      0,
    );
  });
});
