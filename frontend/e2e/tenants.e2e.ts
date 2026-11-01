// The session's tenant (frontend/src/session/): choosing one when the session has none, the only
// one switched to by itself, a user in no tenant told why and able to check again, and the
// top bar's menu. The sessions start in no tenant, as a first sign-in does.
import { TENANT_NAMES, uniqueEmail } from "./support/control.ts";
import { expect, signInWithCookie, test } from "./support/fixtures.ts";
import { checkPopup, respondingTo } from "./support/page.ts";

const TENANT_PATH = "/api/identity/v1/sessions/current/tenant";

test("a session in no tenant, for a user of two, asks which one", async ({ page, control }) => {
  const email = uniqueEmail("alice");
  await control.createUser(email, [
    { tenant: "acme", role: "admin" },
    { tenant: "beta", role: "viewer" },
  ]);
  await signInWithCookie(page, await control.createSession(email));
  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Choose an organization" })).toBeVisible();
  const list = page.getByRole("list", { name: "Your organizations" });
  await expect(list.getByRole("listitem")).toHaveText([/Acme Factory\s*Administrator/, /Beta Plant\s*Viewer/]);

  const switched = await respondingTo(page, "PUT", TENANT_PATH, () =>
    list.getByRole("button", { name: /Acme Factory/ }).click(),
  );
  expect(switched.status()).toBe(200);
  await expect(page).toHaveTitle(`${TENANT_NAMES.acme} · TrustIoT`);
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();
});

test("a user of one tenant is switched into it without being asked", async ({ page, control }) => {
  const email = uniqueEmail("bob");
  await control.createUser(email, [{ tenant: "acme", role: "editor" }]);
  await signInWithCookie(page, await control.createSession(email));
  const switched = await respondingTo(page, "PUT", TENANT_PATH, () => page.goto("/"));
  expect(switched.status()).toBe(200);

  await expect(page).toHaveTitle(`${TENANT_NAMES.acme} · TrustIoT`);
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Choose an organization" })).toHaveCount(0);
  // With one tenant the top bar names it and has no menu.
  await expect(page.getByRole("banner").getByText(TENANT_NAMES.acme)).toBeVisible();
  await expect(page.getByRole("banner").getByRole("button", { name: /^Organization:/ })).toHaveCount(0);
});

test("a user in no tenant is told so, and checking again finds a tenant added meanwhile", async ({ page, control }) => {
  const email = uniqueEmail("carol");
  await control.createUser(email, []);
  await signInWithCookie(page, await control.createSession(email));
  await page.goto("/");

  await expect(page.getByRole("heading", { name: "No organization yet" })).toBeVisible();
  const recheck = await respondingTo(page, "GET", "/api/identity/v1/sessions/current", () =>
    page.getByRole("button", { name: "Check again" }).click(),
  );
  expect(recheck.status()).toBe(200);
  await expect(page.getByRole("heading", { name: "No organization yet" })).toBeVisible();
  // Settings need no tenant.
  await expect(page.getByRole("button", { name: "Account menu" })).toBeVisible();

  await control.addMembership(email, "beta", "viewer");
  const switched = await respondingTo(page, "PUT", TENANT_PATH, () =>
    page.getByRole("button", { name: "Check again" }).click(),
  );
  expect(switched.status()).toBe(200);
  await expect(page).toHaveTitle(`${TENANT_NAMES.beta} · TrustIoT`);
  await expect(page.getByRole("heading", { name: "No organization yet" })).toHaveCount(0);
});

test("the top bar's tenant menu switches tenants, and opening and closing it breaks no CSP rule", async ({
  page,
  control,
  watch,
}) => {
  const email = uniqueEmail("menu");
  await control.createUser(email, [
    { tenant: "acme", role: "admin" },
    { tenant: "beta", role: "viewer" },
  ]);
  await signInWithCookie(page, await control.createSession(email, "acme"));
  await page.goto("/");
  await expect(page).toHaveTitle(`${TENANT_NAMES.acme} · TrustIoT`);

  const trigger = page.getByRole("button", { name: `Organization: ${TENANT_NAMES.acme}` });
  const menu = page.getByRole("menu");

  // Closed by choosing the other tenant.
  const switched: { status: number | null } = { status: null };
  await checkPopup(page, watch, "tenant menu (choice)", {
    open: () => trigger.click(),
    content: menu,
    close: async () => {
      const response = await respondingTo(page, "PUT", TENANT_PATH, () =>
        page.getByRole("menuitemradio", { name: /Beta Plant/ }).click(),
      );
      switched.status = response.status();
    },
  });
  expect(switched.status).toBe(200);
  await expect(page).toHaveTitle(`${TENANT_NAMES.beta} · TrustIoT`);
  const triggerNow = page.getByRole("button", { name: `Organization: ${TENANT_NAMES.beta}` });
  await expect(triggerNow, "focus returns to the menu's trigger").toBeFocused();

  // Closed by Escape: the choice stays where it was.
  await checkPopup(page, watch, "tenant menu (Escape)", {
    open: () => triggerNow.click(),
    content: menu,
    close: () => page.keyboard.press("Escape"),
  });
  await expect(triggerNow).toBeFocused();
  await expect(page).toHaveTitle(`${TENANT_NAMES.beta} · TrustIoT`);
});
