import { chromium } from "playwright-core";
import { readFileSync } from "node:fs";

const BASE = "http://localhost:28300";
const cookie = readFileSync(process.env.COOKIE_FILE, "utf8").trim();
const DEV_TENANT = "00000000-0000-4000-8000-000000000b01";
const out = [];
const step = (name, ok, detail = "") => {
  out.push({ name, ok });
  console.log(`${ok ? "PASS" : "FAIL"} | ${name}${detail ? " | " + detail : ""}`);
};

const browser = await chromium.launch();
const context = await browser.newContext({ locale: "en-US" });
const page = await context.newPage();
page.on("console", (m) => {
  if (m.type() === "error") console.log("console.error:", m.text());
});
page.on("pageerror", (e) => console.log("pageerror:", e.message));
const api = [];
page.on("response", (r) => {
  const u = new URL(r.url());
  if (u.pathname.startsWith("/api") || u.pathname.startsWith("/auth")) {
    api.push(`${r.request().method()} ${u.pathname} ${r.status()}`);
  }
});

const title = (n) => page.getByRole("heading", { name: n });
const suspendedHeading = title("Organization suspended");
const putState = (state, reason) =>
  page.evaluate(
    async ([id, state, reason]) => {
      const r = await fetch(`/api/identity/v1/platform/tenants/${id}/state`, {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(reason ? { state, reason } : { state }),
      });
      return { status: r.status, body: await r.json().catch(() => null) };
    },
    [DEV_TENANT, state, reason],
  );
const session = () =>
  page.evaluate(async () => {
    const r = await fetch("/api/identity/v1/sessions/current");
    return { status: r.status, body: await r.json().catch(() => null) };
  });
const wait = (loc) => loc.waitFor({ state: "visible", timeout: 60_000 });

// 1 normal. The cookie is set from the page: Chrome accepts a Secure __Host- cookie on http://localhost.
await page.goto(BASE + "/login");
for (const attempt of [
  { name: "__Host-trustiot-session", value: cookie, domain: "localhost", path: "/", secure: true },
  { name: "__Host-trustiot-session", value: cookie, url: "https://localhost/", secure: true },
]) {
  try {
    await context.addCookies([attempt]);
  } catch (e) {
    console.log("addCookies failed:", e.message.split("\n")[0]);
    continue;
  }
  const code = await page.evaluate(async () => (await fetch("/api/identity/v1/sessions/current")).status);
  console.log("cookie attempt", Object.keys(attempt).join(","), "->", code);
  if (code === 200) break;
}
await page.goto(BASE + "/");
console.log("cookie names:", await page.evaluate(() => document.cookie.split(";").map((c) => c.split("=")[0].trim())), "len", cookie.length);
console.log("session probe:", JSON.stringify(await page.evaluate(async () => { const r = await fetch("/api/identity/v1/sessions/current"); return [r.status, await r.text()]; })));
await wait(title("Home"));
let s = await session();
step(
  "1 active tenant shows the normal home",
  s.body.active_tenant.state === "active",
  `active_tenant=${s.body.active_tenant.name}/${s.body.active_tenant.state}; tenants=${s.body.tenants.map((t) => `${t.name}:${t.state}`)}`,
);
step("1 no suspension page", (await suspendedHeading.count()) === 0);

// 2 suspend
let r = await putState("suspended", "security_or_abuse");
step("2 platform PUT suspend", r.status === 200 && r.body.state === "suspended", `status=${r.status} state=${r.body?.state} version=${r.body?.state_version}`);

// 3 reload shows the page
api.length = 0;
await page.reload();
await wait(suspendedHeading);
s = await session();
step("3 reload shows the suspension page", true, `session active_tenant.state=${s.body.active_tenant.state}`);
step("3 page names the tenant", await page.getByText("Acme Factory has been suspended").isVisible());
step("3 no home heading", (await title("Home").count()) === 0);
const refused = api.filter((l) => / (40[0-9]|50[0-9])$/.test(l));
step("3 no request was refused on the way to the page", refused.length === 0, `requests=${JSON.stringify(api)}`);
const list = page.getByRole("list", { name: "Your organizations" });
const names = (await list.getByRole("button").allTextContents()).join("|");
step("3 lists the other tenant only", names.startsWith("Beta Plant") && !names.includes("Acme"), names);
step("3 sign-out offered", await page.getByRole("button", { name: "Sign out" }).isVisible());

// 4 switch to another tenant
await list.getByRole("button", { name: /Beta Plant/ }).click();
await wait(title("Home"));
await page.waitForFunction(() => document.title === "Beta Plant · TrustIoT");
step("4 switching to Beta Plant opens its home", (await suspendedHeading.count()) === 0, `title=${await page.title()}`);

// 5 switch back to the suspended tenant through the top bar: the page, not an error
await page.getByRole("button", { name: /Organization: Beta Plant/ }).click();
await page.getByRole("menuitemradio", { name: /Acme Factory/ }).click();
await wait(suspendedHeading);
step("5 switching to the suspended tenant shows the page, no error", (await page.getByText("Could not switch organization").count()) === 0);

// 6 resume then reload
r = await putState("active");
step("6 platform PUT resume", r.status === 200 && r.body.state === "active", `status=${r.status} version=${r.body?.state_version}`);
await page.reload();
await wait(title("Home"));
step("6 reload returns to the normal home", (await suspendedHeading.count()) === 0);

// 7 suspend again, sign out from the page
r = await putState("suspended", "non_payment");
step("7 platform PUT suspend again", r.status === 200, `status=${r.status}`);
await page.reload();
await wait(suspendedHeading);
await page.getByRole("button", { name: "Sign out" }).click();
await page.waitForURL(/\/login/, { timeout: 60_000 });
step("7 sign out from the suspension page reaches the login page", true, page.url());
const after = await context.request.get(BASE + "/api/identity/v1/sessions/current");
step("7 the session has ended (401)", after.status() === 401, `status=${after.status()}`);

console.log("API requests since step 3:\n" + api.join("\n"));
await browser.close();
if (out.some((o) => !o.ok)) process.exit(1);
