import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { Toast } from "./Toast.tsx";

afterEach(() => {
  cleanup();
});

const NOTICE = { id: 1, title: "Invitation sent to a@example.com", body: "They appear once they accept." };

function show(durationMs: number, onDismiss: () => void) {
  return render(<Toast notice={NOTICE} dismissLabel="Dismiss" onDismiss={onDismiss} durationMs={durationMs} />);
}

describe("the notice", () => {
  test("is announced as a status and goes away by itself", async () => {
    let dismissed = 0;
    show(60, () => {
      dismissed += 1;
    });
    expect(screen.getByRole("status").textContent).toContain("Invitation sent to a@example.com");
    await waitFor(() => expect(dismissed).toBe(1));
  });

  test("stays while the pointer is over it, and goes after it leaves", async () => {
    let dismissed = 0;
    show(60, () => {
      dismissed += 1;
    });
    const notice = screen.getByRole("status").querySelector("section") as HTMLElement;
    act(() => {
      notice.dispatchEvent(new window.Event("pointerenter"));
    });
    await new Promise((resolve) => setTimeout(resolve, 200));
    expect(dismissed).toBe(0);
    act(() => {
      notice.dispatchEvent(new window.Event("pointerleave"));
    });
    await waitFor(() => expect(dismissed).toBe(1));
  });

  test("the close button dismisses it at once", () => {
    let dismissed = 0;
    show(60_000, () => {
      dismissed += 1;
    });
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(dismissed).toBe(1);
  });
});
