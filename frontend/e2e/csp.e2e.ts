// The pages under the real `services/web` headers (frontend/README.md, 「CSP 的限制」): the policy
// is the enforcing one, so a page that works here works without an injected `<style>` or inline
// script; and none of the three pages leaves a violation, a `<style>` element or a console error.
import { uniqueEmail } from "./support/control.ts";
import { expect, signInWithCookie, test } from "./support/fixtures.ts";
import { expectCspClean } from "./support/page.ts";

test("services/web sends the enforcing CSP and the Trusted Types report-only policy", async ({ page }) => {
  const response = await page.request.get("/settings");
  expect(response.status()).toBe(200);
  const policy = response.headers()["content-security-policy"] ?? "";
  for (const directive of [
    "default-src 'none'",
    "script-src 'self'",
    "style-src 'self'",
    "connect-src 'self'",
    "frame-ancestors 'none'",
  ]) {
    expect(policy, "Content-Security-Policy").toContain(directive);
  }
  expect(policy, "no inline allowance").not.toContain("unsafe-inline");
  expect(response.headers()["content-security-policy-report-only"]).toBe(
    "require-trusted-types-for 'script'; trusted-types 'none'",
  );
});

test("the sign-in, home and settings pages load without a violation, a <style> element or a console error", async ({
  page,
  control,
  watch,
}) => {
  await page.goto("/login");
  await expect(page.getByRole("heading", { name: "Sign in to TrustIoT" })).toBeVisible();
  await expectCspClean(page, watch, "/login");

  const email = uniqueEmail("csp");
  await control.createUser(email, [{ tenant: "acme", role: "admin" }]);
  await signInWithCookie(page, await control.createSession(email, "acme"));
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Home" })).toBeVisible();
  await expectCspClean(page, watch, "/");

  await page.goto("/settings");
  await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
  await expect(page.getByLabel("Theme")).toBeEnabled();
  await expectCspClean(page, watch, "/settings");
});
