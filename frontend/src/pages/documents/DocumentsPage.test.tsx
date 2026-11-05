import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { ApiError } from "../../api/errors.ts";
import { documentLibrary, openDocuments } from "../../test/documents-render.tsx";
import { type FakeDocumentServer, refusal } from "../../test/fake-documents.ts";
import { DOCUMENTS_PATH } from "./paths.ts";

afterEach(() => {
  cleanup();
});

function pdf(name: string, size = 1_000_000): File {
  const file = new File(["%PDF-1.4"], name, { type: "application/pdf" });
  Object.defineProperty(file, "size", { value: size });
  return file;
}

async function openLibrary(library = documentLibrary(), role?: "viewer" | "editor" | "admin") {
  const rendered = openDocuments(DOCUMENTS_PATH, library, role ? { role } : {});
  await screen.findByRole("heading", { level: 1, name: "Documents" });
  return { ...rendered, library };
}

/** The row of the document called `name`. */
function row(name: string): HTMLElement {
  return screen.getByRole("link", { name }).closest("tr") as HTMLElement;
}

function pick(files: File[]) {
  fireEvent.change(screen.getByLabelText("Choose PDF files to upload"), { target: { files } });
}

async function openUploadDialog(): Promise<HTMLElement> {
  fireEvent.click((await screen.findAllByRole("button", { name: "Upload PDF" }))[0] as HTMLElement);
  return screen.findByRole("dialog");
}

/** Opens the "…" menu of a row with the keyboard, as a user without a pointer would. */
async function openMenu(name: string): Promise<HTMLElement> {
  await act(async () => {
    fireEvent.keyDown(screen.getByRole("button", { name }), { key: "Enter" });
  });
  return screen.findByRole("menu");
}

/** Closes the upload dialog, which hides the page behind it from a screen reader (and a test). */
async function closeDialog(dialog: HTMLElement) {
  fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
}

/** The upload the fake server holds for the file `name`. */
function uploadOf(server: FakeDocumentServer, name: string) {
  const upload = server.uploads.find((item) => item.file.name === name);
  if (upload === undefined) throw new Error(`no upload of ${name} reached the server`);
  return upload;
}

describe("the document library", () => {
  test("lists the documents with what the AI can do with each, the model and the capacity", async () => {
    await openLibrary();
    const table = await screen.findByRole("table", { name: "Documents, 4 shown" });
    const ready = row("XG-200 使用手冊");
    expect(within(ready).getByText("Done, the AI can cite it")).toBeTruthy();
    expect(within(ready).getByText("v2")).toBeTruthy();
    expect(within(ready).getByText("120 pages")).toBeTruthy();
    expect(within(ready).getByText("XG-200 變頻器")).toBeTruthy();

    const processing = row("XG-200 故障碼表");
    expect(within(processing).getByText("Processing 5 / 8 pages")).toBeTruthy();
    const progress = within(processing).getByRole("progressbar", { name: "Processing progress of XG-200 故障碼表" });
    expect(progress.getAttribute("value")).toBe("5");
    expect(progress.getAttribute("max")).toBe("8");
    expect(within(processing).getByText("The AI cites it only when this is done")).toBeTruthy();
    expect(within(processing).getByText("Another member")).toBeTruthy();

    const failed = row("PM-3200 電表手冊");
    expect(within(failed).getByText("Processing failed")).toBeTruthy();
    expect(
      within(failed).getByText(
        "Page 34 couldn't be turned into an image (the file may be damaged). The other 63 pages can already be cited.",
      ),
    ).toBeTruthy();
    expect(within(failed).getByRole("button", { name: "Retry processing PM-3200 電表手冊" })).toBeTruthy();
    expect(within(failed).getByRole("button", { name: "Upload a new version of PM-3200 電表手冊" })).toBeTruthy();

    const queued = row("AC-75 空壓機保養手冊");
    expect(within(queued).getByText("Queued")).toBeTruthy();
    expect(within(queued).getByText("Counting pages")).toBeTruthy();
    expect(within(table).getAllByRole("row")).toHaveLength(5);

    const usage = screen.getByRole("progressbar", { name: "Document library capacity" });
    expect(usage.getAttribute("aria-valuetext")).toMatch(/^\d[\d.,]* \w+ used of 5 GB$/);
  });

  test("a document's name is shown as text, never as markup", async () => {
    const library = documentLibrary({ seeded: false });
    library.documents.addDocument({ name: "<img src=x onerror=alert(1)>", modelId: library.models[0]?.id ?? "" });
    const { container } = await openLibrary(library);
    expect(await screen.findByRole("link", { name: "<img src=x onerror=alert(1)>" })).toBeTruthy();
    expect(container.querySelector("img[src='x']")).toBeNull();
  });

  test("filters by model, by status and by name, each kept in the address", async () => {
    const { router } = await openLibrary();
    await screen.findByRole("table");
    fireEvent.change(screen.getByLabelText("Model"), {
      target: { value: screen.getByRole("option", { name: "PM-3200 電表" }).getAttribute("value") },
    });
    expect(
      screen
        .getAllByRole("link")
        .filter((link) => link.closest("tr"))
        .map((link) => link.textContent),
    ).toEqual(["PM-3200 電表手冊"]);
    expect(router.state.location.search).toContain("model=");

    fireEvent.change(screen.getByLabelText("Model"), { target: { value: "" } });
    fireEvent.change(screen.getByLabelText("Status"), { target: { value: "ready" } });
    expect(
      screen
        .getAllByRole("link")
        .filter((link) => link.closest("tr"))
        .map((link) => link.textContent),
    ).toEqual(["XG-200 使用手冊"]);

    fireEvent.change(screen.getByLabelText("Status"), { target: { value: "" } });
    fireEvent.change(screen.getByLabelText("Search document names"), { target: { value: "故障碼" } });
    expect(router.state.location.search).toContain("q=");
    expect(
      screen
        .getAllByRole("link")
        .filter((link) => link.closest("tr"))
        .map((link) => link.textContent),
    ).toEqual(["XG-200 故障碼表"]);
    fireEvent.change(screen.getByLabelText("Search document names"), { target: { value: "nothing like it" } });
    expect(await screen.findByText("No document matches")).toBeTruthy();
  });

  test("says what the page is for and the next step when there are no documents", async () => {
    await openLibrary(documentLibrary({ seeded: false }));
    expect(await screen.findByText("Upload your first manual")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Add a device first" }).getAttribute("href")).toBe("/devices");
    // The header's button and the empty state's: the same action.
    expect(screen.getAllByRole("button", { name: "Upload PDF" }).length).toBe(2);
  });

  test("shows a skeleton while the list loads", async () => {
    const library = documentLibrary();
    library.documents.listDocuments = () => new Promise(() => {});
    openDocuments(DOCUMENTS_PATH, library);
    expect((await screen.findByText("Loading documents…")).getAttribute("role")).toBe("status");
  });

  test("says the library is unavailable, that nothing was lost, and retries", async () => {
    const library = documentLibrary();
    library.documents.failNext(
      "listDocuments",
      new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }),
    );
    await openLibrary(library).catch(() => {});
    expect(await screen.findByText("The document library is temporarily unavailable")).toBeTruthy();
    expect(screen.getByText(/aren't lost/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("table")).toBeTruthy();
  });

  test("shows another error as it is, with a retry", async () => {
    const library = documentLibrary();
    library.documents.failNext("listDocuments", refusal(403, "permission_denied", "error.permission_denied"));
    openDocuments(DOCUMENTS_PATH, library);
    expect(await screen.findByText(/Couldn't load the documents: You don't have permission/)).toBeTruthy();
  });

  test("a plan without document RAG sees the upgrade wall and nothing else", async () => {
    const library = documentLibrary();
    library.documents.plan = "starter";
    openDocuments(DOCUMENTS_PATH, library);
    expect(await screen.findByText("The document library is part of Business plans and above")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Upload PDF" })).toBeNull();
    expect(screen.queryByRole("table")).toBeNull();
  });

  test("polls while a version is waiting or processing, and shows it when it is done", async () => {
    const { library, queryClient } = await openLibrary();
    expect(within(row("AC-75 空壓機保養手冊")).getByText("Queued")).toBeTruthy();
    act(() => library.documents.advance());
    await act(() => queryClient.invalidateQueries({ queryKey: ["documents"] }));
    await waitFor(() =>
      expect(within(row("AC-75 空壓機保養手冊")).getByText(/^Processing 0 \/ 12 pages$/)).toBeTruthy(),
    );
    for (let step = 0; step < 12; step += 1) act(() => library.documents.advance());
    await act(() => queryClient.invalidateQueries({ queryKey: ["documents"] }));
    await waitFor(() => expect(within(row("AC-75 空壓機保養手冊")).getByText("Done, the AI can cite it")).toBeTruthy());
  });
});

describe("what a viewer can do", () => {
  test("reads the list and has no upload, no row actions and no retry", async () => {
    await openLibrary(documentLibrary(), "viewer");
    await screen.findByRole("table");
    expect(screen.getByText("You're a viewer, so this page is read-only")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Upload PDF" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Retry processing/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Upload a new version/ })).toBeNull();
    expect(screen.queryByRole("columnheader", { name: "Actions" })).toBeNull();
    expect(screen.getByRole("link", { name: "XG-200 使用手冊" })).toBeTruthy();
  });

  test("an empty library tells a viewer to ask an editor, with no upload button", async () => {
    await openLibrary(documentLibrary({ seeded: false }), "viewer");
    expect(await screen.findByText(/Ask an editor or admin to upload/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Upload PDF" })).toBeNull();
  });

  test("an editor has the upload and the row actions", async () => {
    await openLibrary(documentLibrary(), "editor");
    await screen.findByRole("table");
    expect(screen.getByRole("button", { name: "Upload PDF" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Actions for XG-200 使用手冊" })).toBeTruthy();
  });
});

describe("uploading", () => {
  test("needs a model, then sends each file with its progress and queues the document", async () => {
    const { library } = await openLibrary();
    const dialog = await openUploadDialog();
    pick([pdf("XG-200_manual_rev3.pdf", 82_000_000), pdf("brand-new.pdf")]);
    const start = within(dialog).getByRole("button", { name: "Upload" });
    expect(start.hasAttribute("disabled")).toBe(true);
    expect(within(dialog).getByText("Choose a model before uploading.")).toBeTruthy();

    fireEvent.change(within(dialog).getByLabelText("Model"), {
      target: { value: library.models[0]?.id },
    });
    expect(start.hasAttribute("disabled")).toBe(false);
    fireEvent.click(start);

    await waitFor(() => expect(library.documents.uploads).toHaveLength(2));
    const first = uploadOf(library.documents, "XG-200_manual_rev3.pdf");
    expect(first.request).toEqual({ modelId: library.models[0]?.id ?? "" });
    act(() => first.progress(0.64));
    const bar = await within(dialog).findByRole("progressbar", { name: "Upload progress of XG-200_manual_rev3.pdf" });
    expect(bar.getAttribute("value")).toBe("64");
    expect(within(dialog).getByText("64%")).toBeTruthy();

    act(() => uploadOf(library.documents, "brand-new.pdf").finish());
    expect(await within(dialog).findByText("Uploaded and queued for processing")).toBeTruthy();
    act(() => first.finish());
    await within(dialog).findAllByText("Uploaded and queued for processing");
    await waitFor(() => expect(within(dialog).queryByRole("progressbar")).toBeNull());
    await closeDialog(dialog);
    expect(await screen.findByRole("link", { name: "brand-new" })).toBeTruthy();
    expect(within(row("brand-new")).getByText("You")).toBeTruthy();
    expect(within(row("brand-new")).getByText("Queued")).toBeTruthy();
  });

  test("refuses a file that is not a PDF, and one that does not fit, in its own row", async () => {
    const library = documentLibrary();
    library.documents.limitBytes = 70_000_000;
    await openLibrary(library);
    const dialog = await openUploadDialog();
    pick([new File(["x"], "panel-photo.jpg", { type: "image/jpeg" }), pdf("huge.pdf", 900_000_000)]);
    expect(await within(dialog).findByText(/Not a PDF\. The library only takes PDF files/)).toBeTruthy();
    expect(within(dialog).getByText(/Over the remaining capacity of/)).toBeTruthy();
    // Nothing is left to upload, so there is no upload button, and nothing reached the server.
    expect(within(dialog).queryByRole("button", { name: "Upload" })).toBeNull();
    expect(library.documents.uploads).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole("button", { name: "Remove panel-photo.jpg" }));
    expect(within(dialog).queryByText(/Not a PDF/)).toBeNull();
  });

  test("a file named like a document of the same model asks: new version (the default) or new document", async () => {
    const { library } = await openLibrary();
    const dialog = await openUploadDialog();
    fireEvent.change(within(dialog).getByLabelText("Model"), { target: { value: library.models[0]?.id } });
    pick([pdf("XG-200 使用手冊.pdf")]);
    const group = await within(dialog).findByRole("group", { name: "“XG-200 使用手冊” already exists" });
    const asVersion = within(group).getByRole("radio", { name: /Upload as new version v3/ });
    expect((asVersion as HTMLInputElement).checked).toBe(true);
    fireEvent.click(within(dialog).getByRole("button", { name: "Upload" }));
    await waitFor(() => expect(library.documents.uploads).toHaveLength(1));
    expect(library.documents.uploads[0]?.request.documentId).toBe(
      [...library.documents.documents.values()].find((document) => document.name === "XG-200 使用手冊")?.id,
    );
    act(() => library.documents.uploads[0]?.finish());
    await within(dialog).findByText("Uploaded and queued for processing");
    await closeDialog(dialog);
    await waitFor(() => expect(within(row("XG-200 使用手冊")).getByText("v3")).toBeTruthy());
  });

  test("saving under an existing name as a new document is a choice", async () => {
    const { library } = await openLibrary();
    const dialog = await openUploadDialog();
    fireEvent.change(within(dialog).getByLabelText("Model"), { target: { value: library.models[0]?.id } });
    pick([pdf("XG-200 使用手冊.pdf")]);
    fireEvent.click(await within(dialog).findByRole("radio", { name: "Save as a new document" }));
    fireEvent.click(within(dialog).getByRole("button", { name: "Upload" }));
    await waitFor(() => expect(library.documents.uploads).toHaveLength(1));
    expect(library.documents.uploads[0]?.request.documentId).toBeUndefined();
  });

  test("closing the dialog keeps the upload going and the list shows it; leaving the choice cancels it", async () => {
    const { library } = await openLibrary();
    const dialog = await openUploadDialog();
    fireEvent.change(within(dialog).getByLabelText("Model"), { target: { value: library.models[0]?.id } });
    pick([pdf("wiring.pdf", 240_000_000)]);
    fireEvent.click(within(dialog).getByRole("button", { name: "Upload" }));
    await waitFor(() => expect(library.documents.uploads).toHaveLength(1));
    act(() => library.documents.uploads[0]?.progress(0.12));
    fireEvent.click(within(dialog).getByRole("button", { name: "Continue in the background" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const section = screen.getByRole("region", { name: "Uploads in progress" });
    expect(within(section).getByText("wiring.pdf", { exact: false })).toBeTruthy();
    expect(
      within(section).getByRole("progressbar", { name: "Upload progress of wiring.pdf" }).getAttribute("value"),
    ).toBe("12");

    fireEvent.click(within(section).getByRole("button", { name: "Cancel uploading wiring.pdf" }));
    await waitFor(() => expect(screen.queryByRole("region", { name: "Uploads in progress" })).toBeNull());
    expect(library.documents.uploads[0]?.settled).toBe(true);
  });

  test("a refused upload says why, and a failed one can be tried again", async () => {
    const { library } = await openLibrary();
    const dialog = await openUploadDialog();
    fireEvent.change(within(dialog).getByLabelText("Model"), { target: { value: library.models[0]?.id } });
    library.documents.failNext("upload", refusal(403, "quota_exceeded", "entitlement.limit_reached", { max: 5 }));
    pick([pdf("one.pdf")]);
    fireEvent.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect(await within(dialog).findByText(/Over your plan's document capacity, so it wasn't uploaded/)).toBeTruthy();

    // Another file fails for another reason; trying again sends it.
    fireEvent.click(within(dialog).getByRole("button", { name: "Remove one.pdf" }));
    library.documents.failNext("upload", refusal(503, "unavailable", "error.unavailable"));
    pick([pdf("two.pdf")]);
    fireEvent.click(within(dialog).getByRole("button", { name: "Upload" }));
    expect(await within(dialog).findByText(/The upload failed: The service is temporarily unavailable/)).toBeTruthy();
    fireEvent.click(within(dialog).getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(library.documents.uploads).toHaveLength(1));
    act(() => library.documents.uploads[0]?.finish());
    expect(await within(dialog).findByText("Uploaded and queued for processing")).toBeTruthy();
  });

  test("a new version of one document has its model fixed and takes one file", async () => {
    const { library } = await openLibrary();
    fireEvent.click(screen.getByRole("button", { name: "Upload a new version of PM-3200 電表手冊" }));
    const dialog = await screen.findByRole("dialog", { name: "Upload a new version of PM-3200 電表手冊" });
    expect(within(dialog).getByText("Model: PM-3200 電表")).toBeTruthy();
    expect(within(dialog).queryByLabelText("Model")).toBeNull();
    pick([pdf("a.pdf"), pdf("b.pdf")]);
    expect(
      await within(dialog).findByText("A new version is one file, so only the first file was taken."),
    ).toBeTruthy();
    expect(within(dialog).queryByText(/b\.pdf/)).toBeNull();
    fireEvent.click(within(dialog).getByRole("button", { name: "Upload" }));
    await waitFor(() => expect(library.documents.uploads).toHaveLength(1));
    const failed = [...library.documents.documents.values()].find((document) => document.name === "PM-3200 電表手冊");
    if (failed === undefined) throw new Error("the demo library has no PM-3200 manual");
    expect(library.documents.uploads[0]?.request).toEqual({ modelId: failed.model_id, documentId: failed.id });
  });

  test("dropping files on the drop area stages them", async () => {
    await openLibrary();
    const dialog = await openUploadDialog();
    const zone = within(dialog).getByRole("group", { name: "Drop PDF files here" });
    fireEvent.dragOver(zone);
    expect(within(dialog).getByText("Drop to upload")).toBeTruthy();
    fireEvent.drop(zone, { dataTransfer: { files: [pdf("dropped.pdf")] } });
    expect(await within(dialog).findByText("dropped.pdf", { exact: false })).toBeTruthy();
  });

  test("says so when there is no model to upload for", async () => {
    const library = documentLibrary({ seeded: false });
    library.devices.models.clear();
    await openLibrary(library);
    const dialog = await openUploadDialog();
    expect(within(dialog).getByText(/There is no device model yet/)).toBeTruthy();
  });
});

describe("retrying, downloading and deleting", () => {
  test("retrying a failed version queues it again", async () => {
    const { library } = await openLibrary();
    fireEvent.click(screen.getByRole("button", { name: "Retry processing PM-3200 電表手冊" }));
    await waitFor(() => expect(within(row("PM-3200 電表手冊")).getByText("Queued")).toBeTruthy());
    expect(await screen.findByText("Processing of “PM-3200 電表手冊” will be tried again.")).toBeTruthy();
    expect(library.documents.calls.some((call) => call.method === "retryProcessing")).toBe(true);
  });

  test("downloading asks for the original's address and starts the download", async () => {
    const click = spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    try {
      const { library } = await openLibrary();
      fireEvent.click(
        within(await openMenu("Actions for XG-200 使用手冊")).getByRole("menuitem", { name: "Download original" }),
      );
      await waitFor(() => expect(click).toHaveBeenCalled());
      expect(library.documents.calls.find((call) => call.method === "getDownloadUrl")?.args[0]).toMatchObject({
        version: 2,
      });
    } finally {
      click.mockRestore();
    }
  });

  test("deleting asks first, names what goes, and removes the document from the list", async () => {
    const { library } = await openLibrary();
    fireEvent.click(
      within(await openMenu("Actions for PM-3200 電表手冊")).getByRole("menuitem", { name: "Delete document" }),
    );
    const dialog = await screen.findByRole("alertdialog", { name: "Delete “PM-3200 電表手冊”?" });
    expect(
      within(dialog).getByText(/Every version \(up to v1\), the page images and the index are deleted together/),
    ).toBeTruthy();
    expect(within(dialog).getByText(/can't be undone/)).toBeTruthy();
    // Nothing happened before the confirmation, and Cancel keeps the document.
    expect(library.documents.calls.some((call) => call.method === "deleteDocument")).toBe(false);
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(screen.getByRole("link", { name: "PM-3200 電表手冊" })).toBeTruthy();

    fireEvent.click(
      within(await openMenu("Actions for PM-3200 電表手冊")).getByRole("menuitem", { name: "Delete document" }),
    );
    fireEvent.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete document" }));
    await waitFor(() => expect(screen.queryByRole("link", { name: "PM-3200 電表手冊" })).toBeNull());
    expect(await screen.findByText("Deleted “PM-3200 電表手冊”. The AI no longer cites it.")).toBeTruthy();
    expect(library.documents.documents.values().next().value).toBeDefined();
  });

  test("a refused delete is shown in the dialog, which stays open", async () => {
    const { library } = await openLibrary();
    library.documents.failNext("deleteDocument", refusal(403, "permission_denied", "error.permission_denied"));
    fireEvent.click(
      within(await openMenu("Actions for XG-200 使用手冊")).getByRole("menuitem", { name: "Delete document" }),
    );
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Delete document" }));
    expect(await within(dialog).findByText(/Couldn't delete the document: You don't have permission/)).toBeTruthy();
    expect(screen.getByRole("alertdialog")).toBeTruthy();
  });
});
