// The checks the other tests rely on, shown to catch what they claim to: a check that cannot fail
// proves nothing. Each test does on purpose what the check is for.
import { expect, test } from "./support/fixtures.ts";
import { expectCspClean } from "./support/page.ts";

test("an injected <style> is a CSP violation and a <style> element, and both are reported", async ({ page, watch }) => {
  await page.goto("/login");
  await expect(page.getByRole("heading", { name: "Sign in to TrustIoT" })).toBeVisible();
  await expectCspClean(page, watch, "before");

  // What a package that writes its rules into the document would do (react-style-singleton did).
  await page.evaluate(() => {
    const style = document.createElement("style");
    style.textContent = "body { outline: 1px solid red }";
    document.head.append(style);
  });
  await expect
    .poll(async () => (await watch.violations(page)).length, { message: "the violation the injected style caused" })
    .toBeGreaterThan(0);
  const [violation] = await watch.violations(page);
  expect(violation).toContain("style-src");
  await expect(expectCspClean(page, watch, "after")).rejects.toThrow(/securitypolicyviolation|<style>/);
  expect(await page.locator("style").count()).toBe(1);
  // The browser also logged the refusal as a console error, which was the point.
  watch.forget();
});

test("a console error is reported, and the browser's own line for a refused request is not", async ({
  page,
  watch,
}) => {
  await page.goto("/login");
  await expect(page.getByRole("heading", { name: "Sign in to TrustIoT" })).toBeVisible();
  // The page's own check for a session was a 401, which the browser logged: not a problem.
  expect(watch.problems()).toEqual([]);

  await page.evaluate(() => console.error("probe"));
  await expect
    .poll(() => watch.problems(), { message: "the console error the page logged" })
    .toEqual(["console.error: probe"]);
  await expect(expectCspClean(page, watch, "after")).rejects.toThrow(/console errors/);
  watch.forget();
  await expectCspClean(page, watch, "after forgetting");
});
