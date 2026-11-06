import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, render, screen } from "@testing-library/react";
import { useEffect, useState } from "react";
import type { RouteObject } from "react-router";

import { useDocumentApi } from "../../api/pending/DocumentApiProvider.tsx";
import { MENU_ITEMS } from "../../app/menu.ts";
import { ROUTES } from "../../app/routes.tsx";
import { preview } from "../../test/fake-documents.ts";
import { DOCUMENTS_PATH } from "./paths.ts";
import { previewMenu, previewRoutes } from "./preview.ts";

afterEach(() => {
  cleanup();
});

function paths(routes: readonly RouteObject[]): string[] {
  return routes.flatMap((route) => [route.path ?? "", ...paths(route.children ?? [])]);
}

describe("the library's place in the app while its API is pending", () => {
  test("its pages and menu entry are only in the preview, not in the product's routes or menu", () => {
    expect(paths(previewRoutes)).toEqual(["documents", "documents/:documentId"]);
    expect(paths(ROUTES).filter((path) => path.startsWith("documents"))).toEqual([]);
    expect(MENU_ITEMS.some((item) => item.id === "documents")).toBe(false);
  });

  test("the menu entry is in group 3 and stands for the library and every document's page", () => {
    const [entry, ...others] = previewMenu;
    expect(others).toEqual([]);
    expect(entry?.group).toBe(3);
    expect(entry?.path).toBe(DOCUMENTS_PATH);
    expect(entry?.covers("/documents")).toBe(true);
    expect(entry?.covers("/documents/abc")).toBe(true);
    expect(entry?.covers("/documents-old")).toBe(false);
    expect(entry?.covers("/devices")).toBe(false);
  });
});

describe("the preview's fake", () => {
  function Count() {
    const api = useDocumentApi();
    const [count, setCount] = useState<number | null>(null);
    useEffect(() => {
      void api.listDocuments().then((list) => setCount(list.documents.length));
    }, [api]);
    return <p>{count === null ? "loading" : `${count} documents`}</p>;
  }

  test("is a provider, since the knowledge service has no HTTP API yet", () => {
    expect("provider" in preview).toBe(true);
    expect(preview.name.length).toBeGreaterThan(0);
  });

  test("the normal data is the design's four documents and the empty data is none", async () => {
    const Normal = preview.provider("normal");
    const { unmount } = render(
      <Normal>
        <Count />
      </Normal>,
    );
    expect(await screen.findByText("4 documents")).toBeTruthy();
    unmount();
    const Empty = preview.provider("empty");
    render(
      <Empty>
        <Count />
      </Empty>,
    );
    expect(await screen.findByText("0 documents")).toBeTruthy();
  });
});
