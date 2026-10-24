import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { unavailable } from "../test/fake-identity.ts";
import { fakeWithUser, renderAt, signedInBrowser } from "../test/render.tsx";
import type { SessionInfo } from "./session.ts";

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

/** A session in no tenant yet, with three to choose from: the list and the top bar both offer them. */
function choosingAmongThree() {
  const fake = fakeWithUser({ tenants: ["Acme Factory", "Beta Plant", "Gamma Works"] });
  const browser = fake.server.browser(fake.server.createSession(fake.user.email, null));
  /** The tenant the server has for this browser's session now. */
  const serverTenant = () => fake.server.sessions.get(browser.cookie ?? "")?.activeTenant ?? null;
  return { ...fake, browser, serverTenant };
}

function held() {
  let release = () => {};
  const until = new Promise<void>((resolve) => {
    release = resolve;
  });
  return { until, release };
}

describe("switching from more than one place", () => {
  test("a switch from the tenant list disables the top bar and the rest of the list until it lands", async () => {
    const { server, browser } = choosingAmongThree();
    const answer = held();
    server.holdNextAnswer("switchTenant", answer.until);
    renderAt("/", { identity: browser });
    const list = await screen.findByRole("list", { name: "Your organizations" });
    await act(async () => {
      fireEvent.click(within(list).getByRole("button", { name: /Beta Plant/ }));
    });

    const trigger = screen.getByRole("button", { name: /^Organization:/ }) as HTMLButtonElement;
    await waitFor(() => expect(trigger.disabled).toBe(true));
    for (const button of within(list).getAllByRole("button") as HTMLButtonElement[]) {
      expect(button.disabled).toBe(true);
    }

    await act(async () => {
      answer.release();
    });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    await waitFor(() => expect(trigger.disabled).toBe(false));
    expect(trigger.textContent).toContain("Beta Plant");
  });

  test("a switch from the top bar disables the tenant list until it lands", async () => {
    const { server, browser } = choosingAmongThree();
    const answer = held();
    server.holdNextAnswer("switchTenant", answer.until);
    renderAt("/", { identity: browser });
    const list = await screen.findByRole("list", { name: "Your organizations" });
    const buttons = within(list).getAllByRole("button") as HTMLButtonElement[];
    const menu = await openTenantMenu();
    await act(async () => {
      fireEvent.click(within(menu).getByRole("menuitemradio", { name: /Gamma Works/ }));
    });

    await waitFor(() => expect(buttons.map((button) => button.disabled)).toEqual([true, true, true]));
    expect((screen.getByRole("button", { name: /^Organization:/ }) as HTMLButtonElement).disabled).toBe(true);

    await act(async () => {
      answer.release();
    });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    expect(screen.getByRole("button", { name: /^Organization:/ }).textContent).toContain("Gamma Works");
  });

  test("two switches started together run one after the other, and the cache ends where the server does", async () => {
    const { server, browser, tenants, serverTenant } = choosingAmongThree();
    const [acme, beta] = tenants;
    // The server carries out the first switch at once, but its answer is late.
    const answer = held();
    server.holdNextAnswer("switchTenant", answer.until);
    const { queryClient } = renderAt("/", { identity: browser });
    const list = await screen.findByRole("list", { name: "Your organizations" });
    // Found before the menu opens: the open menu hides the rest of the page from the accessibility tree.
    const acmeInList = within(list).getByRole("button", { name: /Acme Factory/ });
    const menu = await openTenantMenu();

    // Both in one turn, before either entry can see the other's switch and disable itself.
    await act(async () => {
      fireEvent.click(acmeInList);
      fireEvent.click(within(menu).getByRole("menuitemradio", { name: /Beta Plant/ }));
    });

    // The second switch waits for the first one's answer before it is sent.
    await waitFor(() => expect(serverTenant()).toBe(acme?.id ?? null));
    expect(server.requests.filter((request) => request.operation === "switchTenant")).toEqual([
      { operation: "switchTenant", input: acme?.id },
    ]);

    await act(async () => {
      answer.release();
    });
    await waitFor(() => expect(document.title).toBe("Beta Plant · TrustIoT"));
    expect(server.requests.filter((request) => request.operation === "switchTenant")).toEqual([
      { operation: "switchTenant", input: acme?.id },
      { operation: "switchTenant", input: beta?.id },
    ]);
    expect(serverTenant()).toBe(beta?.id ?? null);
    expect(queryClient.getQueryData<SessionInfo>(["session"])?.activeTenant?.id).toBe(beta?.id);
    await waitFor(() =>
      expect((screen.getByRole("button", { name: /^Organization:/ }) as HTMLButtonElement).disabled).toBe(false),
    );
    expect(screen.getByRole("button", { name: /^Organization:/ }).textContent).toContain("Beta Plant");
  });

  test("the next switch waits until the earlier answer is cached, however long caching it takes", async () => {
    const { server, browser, tenants, serverTenant } = choosingAmongThree();
    const [acme, beta] = tenants;
    const { queryClient } = renderAt("/", { identity: browser });
    // Caching an answer first cancels the queries in flight; the first time, that takes until released.
    const cancelled = held();
    const cancelQueries = queryClient.cancelQueries.bind(queryClient);
    let calls = 0;
    queryClient.cancelQueries = (async (...args: Parameters<typeof cancelQueries>) => {
      calls += 1;
      if (calls === 1) await cancelled.until;
      return cancelQueries(...args);
    }) as typeof queryClient.cancelQueries;
    const list = await screen.findByRole("list", { name: "Your organizations" });
    const acmeInList = within(list).getByRole("button", { name: /Acme Factory/ });
    const menu = await openTenantMenu();

    await act(async () => {
      fireEvent.click(acmeInList);
      fireEvent.click(within(menu).getByRole("menuitemradio", { name: /Beta Plant/ }));
    });

    // The first answer has arrived but is not cached yet: the second switch is not sent.
    await waitFor(() => expect(calls).toBe(1));
    expect(serverTenant()).toBe(acme?.id ?? null);
    expect(server.operations().filter((operation) => operation === "switchTenant")).toHaveLength(1);

    await act(async () => {
      cancelled.release();
    });
    await waitFor(() => expect(document.title).toBe("Beta Plant · TrustIoT"));
    expect(serverTenant()).toBe(beta?.id ?? null);
    expect(queryClient.getQueryData<SessionInfo>(["session"])?.activeTenant?.id).toBe(beta?.id);
  });
});
