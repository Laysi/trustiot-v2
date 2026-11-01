// The personal settings (frontend/src/pages/SettingsPage.tsx): saved to the account one field at
// a time, the server's refusal shown on the field, and the server's values winning over a stale
// copy in the browser.
import { uniqueEmail } from "./support/control.ts";
import { expect, otherDevice, signInWithCookie, test } from "./support/fixtures.ts";
import { respondingTo } from "./support/page.ts";

const PREFERENCES = "/api/identity/v1/preferences";

interface Stored {
  language: string;
  ai_language: string;
  time_zone: string;
  theme: string;
}

test("each field is saved on its own with a PATCH of just that field", async ({ page, control, baseURL }) => {
  const email = uniqueEmail("prefs");
  await control.createUser(email, [{ tenant: "acme", role: "admin" }]);
  const cookie = await control.createSession(email, "acme");
  await signInWithCookie(page, cookie);
  await page.goto("/settings");
  await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
  // The fields wait for the account's saved values.
  const theme = page.getByLabel("Theme");
  await expect(theme).toBeEnabled();

  const changes: { label: string; value: string; body: Record<string, string> }[] = [
    { label: "Theme", value: "light", body: { theme: "light" } },
    { label: "Time zone", value: "Europe/Berlin", body: { time_zone: "Europe/Berlin" } },
    { label: "AI answer language", value: "zh-TW", body: { ai_language: "zh-TW" } },
  ];
  for (const change of changes) {
    const saved = await respondingTo(page, "PATCH", PREFERENCES, () =>
      page.getByLabel(change.label).selectOption(change.value),
    );
    expect(saved.status(), `saving ${change.label}`).toBe(200);
    expect(saved.request().postDataJSON(), `what was sent for ${change.label}`).toEqual(change.body);
    await expect(page.getByLabel(change.label)).toHaveValue(change.value);
    await expect(page.getByRole("status").filter({ hasText: "Saved" })).toBeVisible();
  }
  // The theme applies at once.
  await expect
    .poll(() => page.evaluate(() => document.documentElement.dataset.theme), { message: "the theme on <html>" })
    .toBe("light");

  // What the account holds is what was saved, read by another device.
  const device = await otherDevice(baseURL ?? "", await control.createSession(email, "acme"));
  const stored = (await (await device.get(PREFERENCES)).json()) as Stored;
  expect(stored).toMatchObject({ theme: "light", time_zone: "Europe/Berlin", ai_language: "zh-TW", language: "auto" });
  await device.dispose();
});

test.describe("a refused value", () => {
  test.use({ locale: "zh-TW" });

  test("an invalid time zone is a 422 on its own field, goes back, and the message follows the language", async ({
    page,
    control,
  }) => {
    const email = uniqueEmail("zone");
    await control.createUser(email, [{ tenant: "acme", role: "admin" }]);
    await signInWithCookie(page, await control.createSession(email, "acme"));
    await page.goto("/settings");
    const zone = page.getByLabel("時區", { exact: true });
    await expect(zone).toBeEnabled();
    await expect(page.getByRole("heading", { name: "個人設定", level: 1 })).toBeVisible();

    // The browser's list and the server's can differ; a name only the server refuses is added by hand.
    await zone.evaluate((select) => {
      const option = document.createElement("option");
      option.value = "Mars/Olympus";
      option.textContent = "Mars/Olympus";
      select.append(option);
    });
    const refused = await respondingTo(page, "PATCH", PREFERENCES, () => zone.selectOption("Mars/Olympus"));
    expect(refused.status()).toBe(422);
    const message = page.getByText("請選擇有效的時區。");
    await expect(message).toBeVisible();
    await expect(zone).toHaveAttribute("aria-invalid", "true");
    await expect(zone, "the field goes back to the saved value").toHaveValue("auto");
    // The other fields are not marked.
    await expect(page.getByLabel("主題", { exact: true })).not.toHaveAttribute("aria-invalid", "true");

    // The language changes, and the refusal already shown is translated again.
    const saved = await respondingTo(page, "PATCH", PREFERENCES, () =>
      page.getByLabel("介面語言", { exact: true }).selectOption("en"),
    );
    expect(saved.status()).toBe(200);
    await expect(page.getByText("Choose a valid time zone.")).toBeVisible();
    await expect(message).toHaveCount(0);
  });
});

test("a value another session saved is what a reload shows, and the browser's copy follows", async ({
  page,
  control,
  baseURL,
}) => {
  const email = uniqueEmail("sync");
  await control.createUser(email, [{ tenant: "acme", role: "admin" }]);
  await signInWithCookie(page, await control.createSession(email, "acme"));
  await page.goto("/settings");
  await expect(page.getByLabel("Theme")).toBeEnabled();
  await expect(page.getByLabel("Interface language")).toHaveValue("auto");

  const device = await otherDevice(baseURL ?? "", await control.createSession(email, "acme"));
  const changed = await device.patch(PREFERENCES, { data: { language: "zh-TW", theme: "dark" } });
  expect(changed.status()).toBe(200);
  await device.dispose();

  const loaded = await respondingTo(page, "GET", PREFERENCES, () => page.reload());
  expect(((await loaded.json()) as Stored).theme).toBe("dark");
  await expect(page.getByRole("heading", { name: "個人設定", level: 1 })).toBeVisible();
  await expect(page.getByLabel("介面語言", { exact: true })).toHaveValue("zh-TW");
  await expect(page.getByLabel("主題", { exact: true })).toHaveValue("dark");
  const copy = await page.evaluate(() => localStorage.getItem("trustiot.preferences"));
  expect(JSON.parse(copy ?? "{}")).toMatchObject({ language: "zh-TW", theme: "dark" });
});
