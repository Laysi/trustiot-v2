// Steps that several tests share: waiting for the request an action sends, signing in with the
// emailed code, and the four checks after a Radix pop-up opens and closes.
import type { Locator, Page, Response } from "@playwright/test";

import type { Control } from "./control.ts";
import { expect, type Watch } from "./fixtures.ts";

/**
 * Runs `action` and returns the response to the request it sends: the first one with this method
 * whose path ends with `pathEnd`. The wait starts before the action, so a fast answer is not missed.
 */
export async function respondingTo(
  page: Page,
  method: string,
  pathEnd: string,
  action: () => Promise<unknown>,
): Promise<Response> {
  const [response] = await Promise.all([
    page.waitForResponse(
      (candidate) => candidate.request().method() === method && new URL(candidate.url()).pathname.endsWith(pathEnd),
    ),
    action(),
  ]);
  return response;
}

/** The email step and the code step of the sign-in page, up to the code being accepted. */
export async function signInWithCode(page: Page, control: Control, email: string): Promise<void> {
  await page.locator('input[name="email"]').fill(email);
  const sent = await respondingTo(page, "POST", "/auth/login", () =>
    page.getByRole("button", { name: "Send code" }).click(),
  );
  expect(sent.status(), "the answer to asking for a code").toBe(202);
  const code = await control.waitForCode(email);
  await page.locator('input[name="code"]').fill(code);
  const verified = await respondingTo(page, "POST", "/auth/login", () =>
    page.getByRole("button", { name: "Sign in", exact: true }).click(),
  );
  expect(verified.status(), "the answer to the right code").toBe(200);
}

/** The scroll lock state of the page: `data-scroll-locked` on the body, and the body's overflow. */
async function scrollLock(page: Page): Promise<{ attribute: boolean; overflow: string }> {
  return page.evaluate(() => ({
    attribute: document.body.hasAttribute("data-scroll-locked"),
    overflow: getComputedStyle(document.body).overflow,
  }));
}

/**
 * The checks the CSP needs after every step (frontend/README.md, 「CSP 的限制」): no
 * `securitypolicyviolation` event, no `<style>` element in the document and no console error.
 */
export async function expectCspClean(page: Page, watch: Watch, when: string): Promise<void> {
  expect(await watch.violations(page), `${when}: securitypolicyviolation events`).toEqual([]);
  expect(await page.locator("style").count(), `${when}: <style> elements`).toBe(0);
  expect(watch.problems(), `${when}: console errors`).toEqual([]);
}

/**
 * One Radix pop-up (a DropdownMenu or an AlertDialog) opened and closed, with the four checks
 * after each: while it is open the body is marked `data-scroll-locked` and does not scroll (the
 * stylesheet's rule for that attribute is what locks it, as the CSP allows no injected style),
 * and after it closes the lock is gone; at both points nothing violated the CSP, the document has
 * no `<style>` element and the console has no error.
 */
export async function checkPopup(
  page: Page,
  watch: Watch,
  name: string,
  steps: { open: () => Promise<unknown>; content: Locator; close: () => Promise<unknown> },
): Promise<void> {
  await steps.open();
  await expect(steps.content, `${name} is open`).toBeVisible();
  await expect
    .poll(async () => scrollLock(page), { message: `${name} open: the body is scroll-locked` })
    .toEqual({ attribute: true, overflow: "hidden" });
  await expectCspClean(page, watch, `${name} open`);
  await steps.close();
  await expect(steps.content, `${name} is closed`).toBeHidden();
  await expect
    .poll(async () => (await scrollLock(page)).attribute, { message: `${name} closed: the scroll lock is released` })
    .toBe(false);
  expect((await scrollLock(page)).overflow, `${name} closed: the body scrolls again`).not.toBe("hidden");
  await expectCspClean(page, watch, `${name} closed`);
}
