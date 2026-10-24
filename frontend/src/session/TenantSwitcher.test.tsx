import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { unavailable } from "../test/fake-identity.ts";
import { renderAt, signedInBrowser } from "../test/render.tsx";

afterEach(() => {
  cleanup();
});

/** Opens the tenant menu from the keyboard, as Radix's trigger expects. */
async function openTenantMenu() {
  const trigger = await screen.findByRole("button", { name: /^Organization:/ });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  return screen.findByRole("menu");
}

describe("TenantSwitcher", () => {
  test("with one tenant it names the tenant, offers no menu, and puts it in the tab title", async () => {
    renderAt("/");
    expect(await screen.findByText("Acme Factory")).toBeDefined();
    expect(screen.queryByRole("button", { name: /^Organization:/ })).toBeNull();
    await waitFor(() => expect(document.title).toBe("Acme Factory · TrustIoT"));
  });

  test("switching tenants cancels and drops the old tenant's data, keeps the user's, and retitles the tab", async () => {
    const { browser, server, tenants } = signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] });
    const [acme, beta] = tenants;
    const { queryClient } = renderAt("/", { identity: browser });
    queryClient.setQueryData(["devices", acme?.id], ["a device of Acme"]);
    // A request of the old tenant still running when the switch lands must not deliver its answer.
    let delivered = false;
    void queryClient
      .fetchQuery({
        queryKey: ["slow", acme?.id],
        queryFn: ({ signal }) =>
          new Promise((_, reject) => signal.addEventListener("abort", () => reject(signal.reason))),
      })
      .then(() => {
        delivered = true;
      })
      .catch(() => {});
    await waitFor(() => expect(document.title).toBe("Acme Factory · TrustIoT"));

    const menu = await openTenantMenu();
    const items = within(menu).getAllByRole("menuitemradio");
    expect(items.map((item) => item.getAttribute("aria-checked"))).toEqual(["true", "false"]);
    await act(async () => {
      fireEvent.click(within(menu).getByRole("menuitemradio", { name: /Beta Plant/ }));
    });

    await waitFor(() => expect(document.title).toBe("Beta Plant · TrustIoT"));
    expect(server.requests.filter((request) => request.operation === "switchTenant")).toEqual([
      { operation: "switchTenant", input: beta?.id },
    ]);
    expect(queryClient.getQueryData(["devices", acme?.id])).toBeUndefined();
    expect(queryClient.getQueryState(["slow", acme?.id])?.fetchStatus).not.toBe("fetching");
    expect(delivered).toBe(false);
    // The session and the preferences are the user's, not the tenant's: they stay cached.
    expect(queryClient.getQueryData(["preferences"])).toBeDefined();
    expect(screen.getByRole("button", { name: /^Organization:/ }).textContent).toContain("Beta Plant");
  });

  test("a switch while the preferences are still loading lets them finish loading", async () => {
    const { browser, server } = signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] });
    let release = () => {};
    server.holdNext(
      "loadPreferences",
      new Promise<void>((resolve) => {
        release = resolve;
      }),
    );
    renderAt("/settings", { identity: browser });
    const theme = () => screen.getByLabelText("Theme") as HTMLSelectElement;
    const menu = await openTenantMenu();
    await act(async () => {
      fireEvent.click(within(menu).getByRole("menuitemradio", { name: /Beta Plant/ }));
    });
    await waitFor(() => expect(document.title).toBe("Beta Plant · TrustIoT"));
    expect(theme().disabled).toBe(true);
    await act(async () => {
      release();
    });
    await waitFor(() => expect(theme().disabled).toBe(false));
  });

  test("a failed switch says so and keeps the current tenant", async () => {
    const { browser, server } = signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] });
    server.failNext("switchTenant", unavailable());
    renderAt("/", { identity: browser });
    const menu = await openTenantMenu();
    await act(async () => {
      fireEvent.click(within(menu).getByRole("menuitemradio", { name: /Beta Plant/ }));
    });
    expect((await screen.findByRole("alert")).textContent).toBe("Could not switch organization. Try again.");
    expect(screen.getByRole("button", { name: /^Organization:/ }).textContent).toContain("Acme Factory");
    expect(document.title).toBe("Acme Factory · TrustIoT");
  });

  test("the menu closes with Escape and gives focus back to its trigger", async () => {
    renderAt("/", { identity: signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] }).browser });
    const menu = await openTenantMenu();
    await act(async () => {
      fireEvent.keyDown(menu, { key: "Escape" });
    });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(screen.getByRole("button", { name: /^Organization:/ }));
  });
});
