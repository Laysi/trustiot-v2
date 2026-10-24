import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import { FakeIdentity, unavailable, validationFailed } from "../test/fake-identity.ts";
import { fakeWithUser, memoryPreferences, renderAt, signedInBrowser } from "../test/render.tsx";

afterEach(() => {
  cleanup();
});

const EMAIL = "alice@example.com";

function signedOut(options: Parameters<typeof fakeWithUser>[0] = {}) {
  const fake = fakeWithUser(options);
  return { ...fake, browser: fake.server.browser() };
}

async function type(label: string | RegExp, value: string) {
  const field = await screen.findByLabelText(label);
  await act(async () => {
    fireEvent.change(field, { target: { value } });
  });
  return field;
}

async function press(name: string | RegExp) {
  const button = await screen.findByRole("button", { name });
  await act(async () => {
    fireEvent.click(button);
  });
}

/** Through the first step: the address is sent, and the code step is open. */
async function requestCode(email = EMAIL) {
  await type("Email", email);
  await press("Send code");
  await screen.findByRole("heading", { name: "Check your email" });
}

describe("LoginPage", () => {
  test("signs in with the emailed code and goes back to the page that asked", async () => {
    const { server, browser } = signedOut();
    const { router } = renderAt("/login?next=%2Fsettings%3Ftab%3Daccount", { identity: browser });

    await requestCode();
    expect(
      screen.getByText(
        "If alice@example.com has a TrustIoT account, we've sent it a 8-digit code. The code expires in 10 minutes.",
      ),
    ).toBeDefined();
    expect(document.activeElement).toBe(screen.getByLabelText("Sign-in code"));
    const sent = server.outbox.at(-1);
    expect(sent?.email).toBe(EMAIL);
    const code = sent?.code ?? "";

    // A pasted code keeps only its digits.
    await type("Sign-in code", `${code.slice(0, 4)} ${code.slice(4)}`);
    expect((screen.getByLabelText("Sign-in code") as HTMLInputElement).value).toBe(code);
    await press("Sign in");

    expect(await screen.findByRole("heading", { name: "Settings" })).toBeDefined();
    expect(`${router.state.location.pathname}${router.state.location.search}`).toBe("/settings?tab=account");
    expect(server.requests.find((request) => request.operation === "verifyCode")?.input).toEqual({
      email: EMAIL,
      code,
      redirectTo: "/settings?tab=account",
    });
    expect(browser.cookie).not.toBeNull();
  });

  test("signing in again while signed in moves on too, even when the session reads the same", async () => {
    const { server, browser } = signedInBrowser();
    const before = browser.cookie;
    const { router } = renderAt("/login?next=%2Fsettings", { identity: browser });
    await requestCode();
    await type("Sign-in code", server.outbox.at(-1)?.code ?? "");
    await press("Sign in");
    expect(await screen.findByRole("heading", { name: "Settings" })).toBeDefined();
    expect(router.state.location.pathname).toBe("/settings");
    // The server ended the browser's previous session and made a new one.
    expect(browser.cookie).not.toBe(before);
  });

  test("an empty address is refused here, without a request", async () => {
    const { server, browser } = signedOut();
    renderAt("/login", { identity: browser });
    await press("Send code");
    expect(await screen.findByText("Enter a valid email address.")).toBeDefined();
    expect(screen.getByLabelText("Email").getAttribute("aria-invalid")).toBe("true");
    expect(server.operations()).not.toContain("sendCode");
  });

  test("the server's field error is shown on the address field", async () => {
    const { browser } = signedOut();
    renderAt("/login", { identity: browser });
    await type("Email", "not-an-address");
    await press("Send code");
    const field = screen.getByLabelText("Email");
    await waitFor(() => expect(field.getAttribute("aria-invalid")).toBe("true"));
    const describedBy = field.getAttribute("aria-describedby") ?? "";
    const messages = describedBy.split(" ").map((id) => document.getElementById(id)?.textContent);
    expect(messages).toContain("Enter a valid email address.");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  test("503 when the code cannot be sent says the service is unavailable, not that the address is wrong", async () => {
    const { server, browser } = signedOut();
    server.failNext("sendCode", unavailable());
    renderAt("/login", { identity: browser });
    await type("Email", EMAIL);
    await press("Send code");
    expect((await screen.findByRole("alert")).textContent).toBe(
      "The service is temporarily unavailable. Try again later.",
    );
    expect(screen.getByLabelText("Email").getAttribute("aria-invalid")).toBeNull();
    expect(screen.getByRole("heading", { name: "Sign in to TrustIoT" })).toBeDefined();
  });

  test("a wrong code (401) is shown on the code field and the page stays put", async () => {
    const { server, browser } = signedOut();
    const { router } = renderAt("/login?next=%2Fsettings", { identity: browser });
    await requestCode();
    await type("Sign-in code", "99999999");
    await press("Sign in");
    expect(
      await screen.findByText("That code is wrong or has expired. Check the email, or send a new code."),
    ).toBeDefined();
    expect(screen.getByLabelText("Sign-in code").getAttribute("aria-invalid")).toBe("true");
    // The 401 is the form's to answer: no redirect to the sign-in page with another `next`.
    expect(`${router.state.location.pathname}${router.state.location.search}`).toBe("/login?next=%2Fsettings");
    expect(browser.cookie).toBeNull();
    expect(server.operations().filter((operation) => operation === "verifyCode")).toHaveLength(1);
  });

  test("a short code is refused here; a code the server refuses (422) shows on the code field", async () => {
    const { server, browser } = signedOut();
    renderAt("/login", { identity: browser });
    await requestCode();
    await type("Sign-in code", "1234");
    await press("Sign in");
    expect(await screen.findByText("Enter the whole code from the email.")).toBeDefined();
    expect(server.operations()).not.toContain("verifyCode");

    server.failNext("verifyCode", validationFailed("/code", "login.code_invalid"));
    await type("Sign-in code", "12345678");
    await press("Sign in");
    await waitFor(() => expect(server.operations()).toContain("verifyCode"));
    expect(await screen.findByText("Enter the whole code from the email.")).toBeDefined();
    expect(screen.getByLabelText("Sign-in code").getAttribute("aria-invalid")).toBe("true");
  });

  test("a refused return path (422 /redirect_to) and a 503 on verify are shown on the form", async () => {
    const { server, browser } = signedOut();
    renderAt("/login", { identity: browser });
    await requestCode();
    server.failNext("verifyCode", validationFailed("/redirect_to", "login.redirect_invalid"));
    await type("Sign-in code", "12345678");
    await press("Sign in");
    expect((await screen.findByRole("alert")).textContent).toBe(
      "The page to return to after signing in isn't on this site.",
    );

    server.failNext("verifyCode", unavailable());
    await press("Sign in");
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe("The service is temporarily unavailable. Try again later."),
    );
    expect(screen.getByLabelText("Sign-in code").getAttribute("aria-invalid")).toBeNull();
  });

  test("a new code can be sent after the wait the server asks for; the old one stops working", async () => {
    const { server, browser } = signedOut();
    renderAt("/login", { identity: browser });
    await requestCode();
    // The server asked for 60 seconds: the button says how long is left and cannot be used.
    const waiting = screen.getByRole("button", { name: /^Send a new code in \d+ s$/ }) as HTMLButtonElement;
    expect(waiting.disabled).toBe(true);

    cleanup();
    server.resendAfterSecs = 0;
    renderAt("/login", { identity: browser });
    await requestCode();
    const first = server.outbox.at(-1)?.code;
    await press("Send a new code");
    expect(await screen.findByText("We sent a new code to alice@example.com.")).toBeDefined();
    const second = server.outbox.at(-1)?.code;
    expect(second).not.toBe(first);
    await type("Sign-in code", first ?? "");
    await press("Sign in");
    expect(
      await screen.findByText("That code is wrong or has expired. Check the email, or send a new code."),
    ).toBeDefined();
  });

  test("going back to the address step keeps nothing of the code step", async () => {
    const { browser } = signedOut();
    renderAt("/login", { identity: browser });
    await requestCode();
    await press("Use a different email");
    expect(await screen.findByRole("heading", { name: "Sign in to TrustIoT" })).toBeDefined();
  });

  test("an address without an account gets the same answer as one with an account", async () => {
    const { server, browser } = signedOut();
    renderAt("/login", { identity: browser });
    await requestCode("bob@example.com");
    expect(
      screen.getByText(
        "If bob@example.com has a TrustIoT account, we've sent it a 8-digit code. The code expires in 10 minutes.",
      ),
    ).toBeDefined();
    expect(server.outbox).toEqual([]);
  });

  test("a sign-in the browser did not keep says to allow cookies", async () => {
    const { server, browser } = signedOut();
    const forgetful = { ...browser, loadSession: async () => null };
    renderAt("/login", { identity: forgetful });
    await requestCode();
    await type("Sign-in code", server.outbox.at(-1)?.code ?? "");
    await press("Sign in");
    expect((await screen.findByRole("alert")).textContent).toBe(
      "You signed in, but this browser didn't keep the sign-in. Allow cookies for this site and try again.",
    );
  });

  test("switching the language changes the page and the error already shown, and is kept in this browser", async () => {
    const { server, browser } = signedOut();
    server.failNext("sendCode", unavailable());
    const preferences = memoryPreferences({ language: "en" });
    renderAt("/login", { identity: browser, preferences });
    await type("Email", EMAIL);
    await press("Send code");
    expect((await screen.findByRole("alert")).textContent).toBe(
      "The service is temporarily unavailable. Try again later.",
    );

    await act(async () => {
      fireEvent.change(screen.getByLabelText("Language"), { target: { value: "zh-TW" } });
    });
    expect(await screen.findByRole("heading", { name: "登入 TrustIoT" })).toBeDefined();
    expect(screen.getByRole("alert").textContent).toBe("服務暫時無法使用，請稍後再試。");
    expect(screen.getByRole("button", { name: "寄送驗證碼" })).toBeDefined();
    expect(document.documentElement.lang).toBe("zh-TW");
    // Signed out, the choice is only this browser's; nothing is sent to the server.
    expect(preferences.current().language).toBe("zh-TW");
    expect(server.operations()).not.toContain("updatePreferences");
  });

  test("after sign-in the account's language replaces the one chosen on the sign-in page", async () => {
    const { server, browser } = signedOut({ preferences: { language: "en" } });
    const preferences = memoryPreferences({ language: "zh-TW" });
    renderAt("/login", { identity: browser, preferences });
    await type("Email", EMAIL);
    await press("寄送驗證碼");
    await type("驗證碼", server.outbox.at(-1)?.code ?? "");
    await press("登入");
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    await waitFor(() => expect(preferences.current().language).toBe("en"));
  });
});

describe("LoginPage with a server that is down", () => {
  test("the page itself does not need the identity service to render", async () => {
    const server = new FakeIdentity();
    server.failNext("loadSession", unavailable());
    renderAt("/login", { identity: server.browser() });
    expect(await screen.findByRole("heading", { name: "Sign in to TrustIoT" })).toBeDefined();
  });
});
