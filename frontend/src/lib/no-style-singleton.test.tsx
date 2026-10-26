import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { STYLE_SINGLETON_ALIAS } from "../../vite.config.ts";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "../components/ui/alert-dialog.tsx";
import { Button } from "../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../components/ui/dropdown-menu.tsx";
import * as shim from "./no-style-singleton.ts";

afterEach(() => {
  cleanup();
});

/** The names react-style-singleton exports, from its type declarations. */
function upstreamExports(): string[] {
  const declarations = readFileSync(
    fileURLToPath(new URL("../../node_modules/react-style-singleton/dist/es5/index.d.ts", import.meta.url)),
    "utf8",
  );
  return [...declarations.matchAll(/export \{ (\w+) \}/g)].map((match) => match[1] ?? "").sort();
}

describe("no-style-singleton", () => {
  test("the build aliases exactly react-style-singleton to this file", () => {
    expect(STYLE_SINGLETON_ALIAS.find.test("react-style-singleton")).toBe(true);
    expect(STYLE_SINGLETON_ALIAS.find.test("react-style-singleton/dist/es2015/index.js")).toBe(false);
    expect(STYLE_SINGLETON_ALIAS.replacement).toBe(fileURLToPath(new URL("./no-style-singleton.ts", import.meta.url)));
  });

  test("it has every export of the package it stands in for", () => {
    expect(upstreamExports()).toEqual(Object.keys(shim).sort());
    expect(upstreamExports().length).toBeGreaterThan(0);
  });

  test("each call returns a new component, which react-remove-scroll uses to tell its locks apart", () => {
    expect(shim.styleSingleton()).not.toBe(shim.styleSingleton());
  });

  test("an open menu or dialog locks the page's scroll without writing a <style> element", async () => {
    render(
      <>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button>Menu</Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            <DropdownMenuItem>Item</DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
        <AlertDialog>
          <AlertDialogTrigger asChild>
            <Button>Dialog</Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogTitle>Title</AlertDialogTitle>
            <AlertDialogCancel>Close</AlertDialogCancel>
          </AlertDialogContent>
        </AlertDialog>
      </>,
    );

    await act(async () => {
      fireEvent.keyDown(screen.getByRole("button", { name: "Menu" }), { key: "Enter" });
    });
    await screen.findByRole("menu");
    expect(document.body.dataset.scrollLocked).toBe("1");
    expect(document.querySelectorAll("style")).toHaveLength(0);
    await act(async () => {
      fireEvent.keyDown(screen.getByRole("menu"), { key: "Escape" });
    });
    await waitFor(() => expect(document.body.dataset.scrollLocked).toBeUndefined());

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Dialog" }));
    });
    await screen.findByRole("alertdialog");
    expect(document.body.dataset.scrollLocked).toBe("1");
    // Radix positions the popup and its focus guards through the element's style object (CSSOM),
    // which the CSP's style-src does not govern; only <style> elements and markup attributes are.
    expect(document.querySelectorAll("style")).toHaveLength(0);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Close" }));
    });
    await waitFor(() => expect(document.body.dataset.scrollLocked).toBeUndefined());
  });
});
