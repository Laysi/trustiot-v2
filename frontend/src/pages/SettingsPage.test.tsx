import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { unauthenticated, unavailable, validationFailed } from "../test/fake-identity.ts";
import { memoryPreferences, renderAt, signedInBrowser } from "../test/render.tsx";

afterEach(() => {
  cleanup();
});

function select(label: string): HTMLSelectElement {
  return screen.getByLabelText(label) as HTMLSelectElement;
}

async function choose(label: string, value: string) {
  await act(async () => {
    fireEvent.change(select(label), { target: { value } });
  });
}

/** The text of what describes a control: its hint and its error. */
function description(control: HTMLElement): string[] {
  return (control.getAttribute("aria-describedby") ?? "")
    .split(" ")
    .filter(Boolean)
    .map((id) => document.getElementById(id)?.textContent ?? "");
}

async function openSettings(options: Parameters<typeof signedInBrowser>[0] = {}, store = memoryPreferences()) {
  const fake = signedInBrowser(options);
  const rendered = renderAt("/settings", { identity: fake.browser, preferences: store });
  await screen.findByRole("heading", { name: "Settings" });
  await waitFor(() => expect(select("Theme").disabled).toBe(false));
  return { ...fake, ...rendered, store };
}

describe("SettingsPage preferences", () => {
  test("shows the account's values, not the browser's copy, and copies them into the browser", async () => {
    const store = memoryPreferences({ language: "en", aiLanguage: "auto", timeZone: "UTC", theme: "light" });
    await openSettings(
      { preferences: { language: "en", aiLanguage: "zh-TW", timeZone: "Asia/Taipei", theme: "dark" } },
      store,
    );
    expect(select("Interface language").value).toBe("en");
    expect(select("AI answer language").value).toBe("zh-TW");
    expect(select("Time zone").value).toBe("Asia/Taipei");
    expect(select("Theme").value).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    await waitFor(() =>
      expect(store.current()).toEqual({ language: "en", aiLanguage: "zh-TW", timeZone: "Asia/Taipei", theme: "dark" }),
    );
  });

  test("the controls wait for the server's values before they can be changed", async () => {
    const fake = signedInBrowser();
    let release = () => {};
    fake.server.holdNext(
      "loadPreferences",
      new Promise<void>((resolve) => {
        release = resolve;
      }),
    );
    renderAt("/settings", { identity: fake.browser });
    await screen.findByRole("heading", { name: "Settings" });
    expect(select("Theme").disabled).toBe(true);
    expect(screen.getByText("Loading…").closest('[role="status"]')).not.toBeNull();
    await act(async () => {
      release();
    });
    await waitFor(() => expect(select("Theme").disabled).toBe(false));
  });

  test("a change is saved to the account, field by field, and applies at once", async () => {
    const { server, user, store } = await openSettings();
    await choose("AI answer language", "en");
    await waitFor(() => expect(screen.getByText("Saved")).toBeDefined());
    expect(server.requests.filter((request) => request.operation === "updatePreferences")).toEqual([
      { operation: "updatePreferences", input: { aiLanguage: "en" } },
    ]);
    expect(user.preferences.aiLanguage).toBe("en");
    expect(store.current().aiLanguage).toBe("en");

    await choose("Theme", "dark");
    await waitFor(() => expect(user.preferences.theme).toBe("dark"));
    expect(document.documentElement.dataset.theme).toBe("dark");
    expect(server.requests.filter((request) => request.operation === "updatePreferences").at(-1)?.input).toEqual({
      theme: "dark",
    });
  });

  test("a value the server refuses (422) goes back to the saved one, with the reason on that field", async () => {
    const { server, user } = await openSettings({ preferences: { timeZone: "Asia/Taipei" } });
    server.failNext("updatePreferences", validationFailed("/time_zone", "preferences.time_zone_invalid"));
    await choose("Time zone", "Europe/Berlin");
    await waitFor(() => expect(select("Time zone").getAttribute("aria-invalid")).toBe("true"));
    expect(description(select("Time zone"))).toContain("Choose a valid time zone.");
    expect(select("Time zone").value).toBe("Asia/Taipei");
    expect(user.preferences.timeZone).toBe("Asia/Taipei");
    // Only the refused field is marked.
    expect(select("Theme").getAttribute("aria-invalid")).toBeNull();

    // The error is kept, not its text: switching the language translates it.
    await choose("Interface language", "zh-TW");
    expect(await screen.findByRole("heading", { name: "個人設定" })).toBeDefined();
    expect(description(select("時區"))).toContain("請選擇有效的時區。");

    // A later change that the server accepts clears the error.
    await choose("時區", "Europe/Berlin");
    await waitFor(() => expect(select("時區").getAttribute("aria-invalid")).toBeNull());
    expect(user.preferences.timeZone).toBe("Europe/Berlin");
  });

  test("a save that fails for another reason says so on the field and keeps the saved value", async () => {
    const { server } = await openSettings();
    server.failNext("updatePreferences", unavailable());
    await choose("Theme", "dark");
    await waitFor(() =>
      expect(description(select("Theme"))).toContain(
        "Not saved: The service is temporarily unavailable. Try again later.",
      ),
    );
    expect(select("Theme").value).toBe("system");
    expect(document.documentElement.dataset.theme).toBe("light");
  });

  test("the server's answer is what stays, in every browser the user signs in to", async () => {
    const { server, user } = await openSettings();
    await choose("Theme", "dark");
    await waitFor(() => expect(user.preferences.theme).toBe("dark"));
    cleanup();

    // Another browser, whose own copy still says light, gets the account's dark theme.
    const other = server.browser(server.createSession(user.email, server.tenants.values().next().value ?? null));
    renderAt("/settings", { identity: other, preferences: memoryPreferences({ theme: "light" }) });
    await screen.findByRole("heading", { name: "Settings" });
    await waitFor(() => expect(select("Theme").value).toBe("dark"));
    expect(document.documentElement.dataset.theme).toBe("dark");
  });

  test("preferences that cannot be loaded cannot be changed, and can be loaded again", async () => {
    const fake = signedInBrowser({ preferences: { theme: "dark" } });
    fake.server.failNext("loadPreferences", unavailable());
    renderAt("/settings", { identity: fake.browser });
    expect(
      await screen.findByText("Your saved preferences couldn't be loaded, so they can't be changed right now."),
    ).toBeDefined();
    expect(select("Theme").disabled).toBe(true);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    });
    await waitFor(() => expect(select("Theme").disabled).toBe(false));
    expect(select("Theme").value).toBe("dark");
  });

  test("changing the interface language changes the page's text and is saved", async () => {
    const { server, user } = await openSettings();
    await choose("Interface language", "zh-TW");
    expect(await screen.findByRole("heading", { name: "個人設定" })).toBeDefined();
    expect(screen.getByRole("heading", { name: "偏好設定" })).toBeDefined();
    expect(document.documentElement.lang).toBe("zh-TW");
    await waitFor(() => expect(user.preferences.language).toBe("zh-TW"));
    expect(server.requests.filter((request) => request.operation === "updatePreferences").at(-1)?.input).toEqual({
      language: "zh-TW",
    });
  });

  test("the account card shows the address and the platform role", async () => {
    await openSettings({ platformAdmin: true });
    const account = screen.getByRole("region", { name: "Account" });
    expect(within(account).getByText("alice@example.com")).toBeDefined();
    expect(within(account).getByText("Platform administrator")).toBeDefined();
  });
});

describe("signing out", () => {
  test("of all devices asks first, ends every session, and says so on the sign-in page", async () => {
    const { server, user, browser, router } = await openSettings();
    const tenant = server.tenants.values().next().value ?? null;
    const other = server.browser(server.createSession(user.email, tenant));
    expect(await other.loadSession()).not.toBeNull();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Sign out of all devices" }));
    });
    const dialog = await screen.findByRole("alertdialog", { name: "Sign out of all devices?" });
    expect(server.operations()).not.toContain("logOutEverywhere");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Sign out of all devices" }));
    });

    expect(await screen.findByText("You signed out on all your devices.")).toBeDefined();
    expect(router.state.location.pathname).toBe("/login");
    expect(router.state.location.search).toBe("");
    expect(browser.cookie).toBeNull();
    // The other browser's session ended too: its next request is signed out.
    expect(await other.loadSession()).toBeNull();
  });

  test("of all devices can be cancelled, and focus goes back to the button", async () => {
    const { server } = await openSettings();
    const trigger = screen.getByRole("button", { name: "Sign out of all devices" });
    await act(async () => {
      fireEvent.click(trigger);
    });
    const dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    });
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(document.activeElement).toBe(trigger);
    expect(server.operations()).not.toContain("logOutEverywhere");
  });

  test("of all devices that fails says why in the dialog and stays signed in", async () => {
    const { server, router } = await openSettings();
    server.failNext("logOutEverywhere", unavailable());
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Sign out of all devices" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Sign out of all devices" }));
    });
    expect(
      await within(dialog).findByText("Could not sign out: The service is temporarily unavailable. Try again later."),
    ).toBeDefined();
    expect(router.state.location.pathname).toBe("/settings");
  });

  test("of all devices when this session had already ended sends the user to sign in", async () => {
    const { server, router } = await openSettings();
    server.failNext("logOutEverywhere", unauthenticated());
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Sign out of all devices" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Sign out of all devices" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
    expect(router.state.location.search).toBe("?next=%2Fsettings");
  });

  test("of this browser from the account menu leaves the other sessions alone", async () => {
    const { server, user, browser, router } = await openSettings();
    const other = server.browser(server.createSession(user.email, null));
    await act(async () => {
      fireEvent.keyDown(screen.getByRole("button", { name: "Account menu" }), { key: "Enter" });
    });
    const menu = await screen.findByRole("menu");
    expect(within(menu).getByText("alice@example.com")).toBeDefined();
    await act(async () => {
      fireEvent.click(within(menu).getByRole("menuitem", { name: "Sign out" }));
    });
    expect(await screen.findByText("You signed out.")).toBeDefined();
    expect(router.state.location.pathname).toBe("/login");
    expect(browser.cookie).toBeNull();
    expect(await other.loadSession()).not.toBeNull();
  });
});
