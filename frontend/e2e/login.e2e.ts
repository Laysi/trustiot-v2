// Signing in with the code that comes by email (frontend/src/pages/LoginPage.tsx), against the real
// identity service and the mail relay of the run. Each test has its own address: the service
// limits how many codes one address gets (60 s apart, 5 an hour).
import { type Role, TENANT_NAMES, uniqueEmail } from "./support/control.ts";
import { expect, test } from "./support/fixtures.ts";
import { respondingTo, signInWithCode } from "./support/page.ts";

const ADMIN_OF_ACME: { tenant: "acme"; role: Role }[] = [{ tenant: "acme", role: "admin" }];

test("a visit without a session is a 401 that shows the sign-in page, not an error", async ({ page }) => {
  const session = await respondingTo(page, "GET", "/api/identity/v1/sessions/current", () => page.goto("/login"));
  expect(session.status()).toBe(401);
  await expect(page.getByRole("heading", { name: "Sign in to TrustIoT" })).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("a wrong code is refused with a 401 on the code field, in the language chosen, and the right code signs in", async ({
  page,
  control,
}) => {
  const email = uniqueEmail("wrong-code");
  await control.createUser(email, ADMIN_OF_ACME);
  await page.goto("/login");
  await page.locator('input[name="email"]').fill(email);
  const sent = await respondingTo(page, "POST", "/auth/login", () =>
    page.getByRole("button", { name: "Send code" }).click(),
  );
  expect(sent.status()).toBe(202);

  const code = await control.waitForCode(email);
  // The same length with the last digit changed, so it cannot be the real one.
  const wrong = `${code.slice(0, -1)}${(Number(code.slice(-1)) + 1) % 10}`;
  const codeField = page.locator('input[name="code"]');
  await codeField.fill(wrong);
  const refused = await respondingTo(page, "POST", "/auth/login", () =>
    page.getByRole("button", { name: "Sign in", exact: true }).click(),
  );
  expect(refused.status()).toBe(401);
  await expect(page.getByText("That code is wrong or has expired.")).toBeVisible();
  await expect(codeField).toHaveAttribute("aria-invalid", "true");

  // The error that is already shown is translated again when the language changes.
  await page.getByLabel("Language").selectOption("zh-TW");
  await expect(page.getByText("驗證碼不正確或已失效。")).toBeVisible();
  await expect(page.getByText("That code is wrong or has expired.")).toHaveCount(0);

  await codeField.fill(code);
  const verified = await respondingTo(page, "POST", "/auth/login", () =>
    page.getByRole("button", { name: "登入", exact: true }).click(),
  );
  expect(verified.status()).toBe(200);
  expect(await verified.json()).toEqual({ redirect_to: "/" });
  // The account's language (auto: the browser's, English) replaces the one chosen on the sign-in page.
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();
  await expect(page).toHaveURL("/");
  await expect(page).toHaveTitle(`${TENANT_NAMES.acme} · TrustIoT`);
});

test("a page that needs a session comes back to itself after signing in (?next)", async ({ page, control }) => {
  const email = uniqueEmail("next");
  await control.createUser(email, ADMIN_OF_ACME);
  await page.goto("/settings");
  await expect(page).toHaveURL(/\/login\?next=%2Fsettings$/);

  await page.locator('input[name="email"]').fill(email);
  await respondingTo(page, "POST", "/auth/login", () => page.getByRole("button", { name: "Send code" }).click());
  const code = await control.waitForCode(email);
  await page.locator('input[name="code"]').fill(code);
  const verified = await respondingTo(page, "POST", "/auth/login", () =>
    page.getByRole("button", { name: "Sign in", exact: true }).click(),
  );
  expect(verified.request().postDataJSON()).toMatchObject({ step: "verify_code", redirect_to: "/settings" });
  expect(await verified.json()).toEqual({ redirect_to: "/settings" });
  await expect(page).toHaveURL(/\/settings$/);
  await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
});

test("a sign-in leaves exactly one live session in the database", async ({ page, control }) => {
  const email = uniqueEmail("again");
  await control.createUser(email, ADMIN_OF_ACME);
  await page.goto("/login");
  await signInWithCode(page, control, email);
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();
  expect(await control.liveSessions(email)).toBe(1);
});
