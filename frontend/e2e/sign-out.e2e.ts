// Signing out of this browser, and out of every device (frontend/src/session/useSignOut.ts,
// SettingsPage.tsx): the account menu and the confirmation dialog are Radix pop-ups, so each one
// is opened and closed under the CSP checks too. "Another session" is a second device with its
// own cookie, outside the page's browser.
import { uniqueEmail } from "./support/control.ts";
import { expect, otherDevice, signInWithCookie, test } from "./support/fixtures.ts";
import { checkPopup, respondingTo } from "./support/page.ts";

const CURRENT = "/api/identity/v1/sessions/current";
const SESSIONS = "/api/identity/v1/sessions";

test("the account menu signs this browser out and leaves the other session alone", async ({
  page,
  control,
  watch,
  baseURL,
}) => {
  const email = uniqueEmail("out");
  await control.createUser(email, [{ tenant: "acme", role: "admin" }]);
  const device = await otherDevice(baseURL ?? "", await control.createSession(email, "acme"));
  await signInWithCookie(page, await control.createSession(email, "acme"));
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();

  const trigger = page.getByRole("button", { name: "Account menu" });
  // Closed with Escape first: nothing is signed out.
  await checkPopup(page, watch, "account menu (Escape)", {
    open: () => trigger.click(),
    content: page.getByRole("menu"),
    close: () => page.keyboard.press("Escape"),
  });
  await expect(trigger).toBeFocused();
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();

  const out: { status: number | null } = { status: null };
  await checkPopup(page, watch, "account menu (Sign out)", {
    open: () => trigger.click(),
    content: page.getByRole("menu"),
    close: async () => {
      const response = await respondingTo(page, "DELETE", CURRENT, () =>
        page.getByRole("menuitem", { name: "Sign out" }).click(),
      );
      out.status = response.status();
    },
  });
  expect(out.status).toBe(204);
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByText("You signed out.", { exact: true })).toBeVisible();

  // This browser's session has ended; the other device's has not.
  expect((await page.request.get(CURRENT)).status()).toBe(401);
  expect((await device.get(CURRENT)).status()).toBe(200);
  expect(await control.liveSessions(email)).toBe(1);
  await device.dispose();
});

test("signing out of all devices asks first, ends every session and leaves the other device with a 401", async ({
  page,
  control,
  watch,
  baseURL,
}) => {
  const email = uniqueEmail("everywhere");
  await control.createUser(email, [{ tenant: "acme", role: "admin" }]);
  const device = await otherDevice(baseURL ?? "", await control.createSession(email, "acme"));
  await signInWithCookie(page, await control.createSession(email, "acme"));
  await page.goto("/settings");
  await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
  expect(await control.liveSessions(email)).toBe(2);

  const trigger = page.getByRole("button", { name: "Sign out of all devices" });
  const dialog = page.getByRole("alertdialog", { name: "Sign out of all devices?" });

  // Escape and Cancel close the dialog and nothing changes; focus goes back to the button.
  await checkPopup(page, watch, "confirmation dialog (Escape)", {
    open: () => trigger.click(),
    content: dialog,
    close: () => page.keyboard.press("Escape"),
  });
  await expect(trigger).toBeFocused();
  await checkPopup(page, watch, "confirmation dialog (Cancel)", {
    open: () => trigger.click(),
    content: dialog,
    close: () => dialog.getByRole("button", { name: "Cancel" }).click(),
  });
  await expect(trigger).toBeFocused();
  expect(await control.liveSessions(email), "nothing was signed out yet").toBe(2);
  expect((await device.get(CURRENT)).status()).toBe(200);

  // Confirming ends every session of the account.
  const confirmed: { status: number | null } = { status: null };
  await checkPopup(page, watch, "confirmation dialog (confirm)", {
    open: () => trigger.click(),
    content: dialog,
    close: async () => {
      const response = await respondingTo(page, "DELETE", SESSIONS, () =>
        dialog.getByRole("button", { name: "Sign out of all devices" }).click(),
      );
      confirmed.status = response.status();
    },
  });
  expect(confirmed.status).toBe(204);
  await expect(page).toHaveURL(/\/login$/);
  await expect(page.getByText("You signed out on all your devices.")).toBeVisible();

  expect((await device.get(CURRENT)).status(), "the other session").toBe(401);
  expect((await page.request.get(CURRENT)).status(), "this browser's session").toBe(401);
  expect(await control.liveSessions(email)).toBe(0);
  await device.dispose();
});
