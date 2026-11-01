// Drives the built SPA in a real Chromium through the ingress stand-in and services/web (the
// production CSP headers). A listener installed before the first script records every
// `securitypolicyviolation` (enforced or report-only), and every console message is kept; a step
// passes only when its UI result is the expected one and no violation was reported.
import { readFileSync } from "node:fs";
import { chromium } from "playwright-core";

const BASE = "http://localhost:28300";
const CONTROL = "http://127.0.0.1:28301";
const cookie = readFileSync(process.env.COOKIE_FILE, "utf8").trim();
const COOKIE_NAME = "__Host-trustiot-session";
const DEVICE = "00000000-0000-4000-8000-0000000000d1";
const MODEL = "00000000-0000-4000-8000-0000000000e1";
const STORAGE_KEY = "trustiot.preferences";
const locales = {
  en: JSON.parse(readFileSync("en.json", "utf8")),
  "zh-TW": JSON.parse(readFileSync("zh-TW.json", "utf8")),
};
const T = (locale, key, vars = {}) => {
  const text = locales[locale][key];
  if (text === undefined) throw new Error(`no message ${key}`);
  return text.replace(/\{(\w+)\}/g, (_, name) => String(vars[name]));
};

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const CSP_TEXT = /content security policy|refused to|trustedhtml|trustedscript|trusted type|require-trusted-types|unsafe-eval/i;

const browser = await chromium.launch();
const steps = [];
let cur = null;
const toClose = [];

async function rules(list) {
  const r = await fetch(`${CONTROL}/rules`, { method: "PUT", body: JSON.stringify(list) });
  if (!r.ok) throw new Error(`rules ${r.status}`);
}
const json = (value) => JSON.stringify(value);
const respond = (name, method, path, status, body, type) => ({
  name,
  method,
  path,
  respond: { status, body: typeof body === "string" ? body : json(body), type: type ?? (typeof body === "string" ? "text/plain" : "application/json") },
});

/** A browser context with the violation listener in place before any page script runs. */
async function fresh({ signedIn = true, locale = "en-US", storage } = {}) {
  const context = await browser.newContext({ locale, colorScheme: "light" });
  toClose.push(context);
  await context.exposeFunction("__trustiotReport", (report) => cur.csp.push(report));
  await context.addInitScript(() => {
    document.addEventListener(
      "securitypolicyviolation",
      (e) =>
        window.__trustiotReport({
          disposition: e.disposition,
          directive: e.violatedDirective,
          effective: e.effectiveDirective,
          blocked: String(e.blockedURI).slice(0, 80),
          sample: String(e.sample).slice(0, 80),
          source: `${String(e.sourceFile).slice(0, 80)}:${e.lineNumber}`,
        }),
      true,
    );
  });
  if (storage !== undefined) {
    await context.addInitScript(
      ([key, value]) => {
        try {
          if (!sessionStorage.getItem("__seeded")) {
            localStorage.setItem(key, value);
            sessionStorage.setItem("__seeded", "1");
          }
        } catch {}
      },
      [STORAGE_KEY, storage],
    );
  }
  if (signedIn) {
    try {
      await context.addCookies([{ name: COOKIE_NAME, value: cookie, domain: "localhost", path: "/", secure: true }]);
    } catch {
      await context.addCookies([{ name: COOKIE_NAME, value: cookie, url: "https://localhost/", secure: true }]);
    }
  }
  const page = await context.newPage();
  page.on("console", (m) => cur.console.push(`${m.type()}: ${m.text().replace(/\s+/g, " ").slice(0, 220)}`));
  page.on("pageerror", (e) => cur.console.push(`pageerror: ${e.message.slice(0, 220)}`));
  page.on("requestfailed", (r) => cur.console.push(`requestfailed: ${r.url().slice(0, 100)} ${r.failure()?.errorText}`));
  page.on("response", (r) => {
    const u = new URL(r.url());
    const type = (r.headers()["content-type"] ?? "-").split(";")[0];
    if (u.pathname.startsWith("/api") || u.pathname.startsWith("/auth")) {
      cur.requests.push(`${r.request().method()} ${u.pathname} ${r.status()} ${type}`);
    }
    if (r.request().resourceType() === "document") {
      const h = r.headers();
      cur.documents.push({
        path: u.pathname,
        status: r.status(),
        csp: h["content-security-policy"] ?? "",
        reportOnly: h["content-security-policy-report-only"] ?? "",
      });
    }
  });
  return page;
}

const note = (text) => {
  cur.notes.push(text);
};
const show = (page, text, name = text) =>
  page.getByText(text, { exact: false }).first().waitFor({ state: "visible", timeout: 60_000 }).catch(() => {
    throw new Error(`waited for text "${name}" and it never appeared`);
  });
const heading = (page, text) =>
  page.getByRole("heading", { name: text, exact: true }).waitFor({ state: "visible", timeout: 60_000 }).catch(() => {
    throw new Error(`waited for heading "${text}" and it never appeared`);
  });
const expectEqual = (what, actual, expected) => {
  note(`${what} = ${JSON.stringify(actual)}`);
  if (actual !== expected) throw new Error(`${what}: got ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
};
const docState = (page) =>
  page.evaluate(() => ({
    lang: document.documentElement.lang,
    theme: document.documentElement.dataset.theme,
    stored: localStorage.getItem("trustiot.preferences"),
  }));
const expectDocumentCsp = () => {
  if (cur.documents.length === 0) throw new Error("no document response was recorded");
  for (const d of cur.documents) {
    if (!d.csp.includes("script-src 'self'") || d.csp.includes("unsafe-eval") || !d.reportOnly.includes("require-trusted-types-for 'script'")) {
      throw new Error(`document ${d.path} did not carry the production CSP: ${d.csp} | ${d.reportOnly}`);
    }
  }
  note(`document responses (${cur.documents.length}) carry the production CSP: script-src 'self', no 'unsafe-eval', Trusted Types report-only`);
};

async function step(name, fn, { control = false } = {}) {
  cur = { name, csp: [], console: [], requests: [], documents: [], notes: [] };
  await fetch(`${CONTROL}/hits`, { method: "DELETE" });
  await rules([]);
  let error = null;
  try {
    await fn();
  } catch (e) {
    error = e;
  }
  await sleep(500);
  const hits = await (await fetch(`${CONTROL}/hits`)).json();
  for (const context of toClose.splice(0)) await context.close().catch(() => {});
  const cspConsole = cur.console.filter((line) => CSP_TEXT.test(line));
  const clean = cur.csp.length === 0 && cspConsole.length === 0;
  let ok = error === null;
  if (control) {
    // A control must trigger the violation, which proves the listener sees what it is meant to.
    ok = ok && !clean;
  } else {
    ok = ok && clean;
  }
  console.log(`\n### ${name}`);
  for (const line of cur.notes) console.log(`  ${line}`);
  const requests = [...new Set(cur.requests)];
  console.log(`  api responses seen by the page: ${requests.length ? "\n    " + requests.join("\n    ") : "none"}`);
  console.log(`  stand-in rule hits: ${hits.length ? hits.map((h) => `${h.rule} (${h.method} ${h.path} -> ${h.status})`).join("; ") : "none"}`);
  console.log(`  securitypolicyviolation events: ${cur.csp.length ? "\n    " + cur.csp.map((c) => JSON.stringify(c)).join("\n    ") : "none"}`);
  const other = cur.console.filter((line) => !/^log:/.test(line));
  console.log(`  console (non-log): ${other.length ? "\n    " + other.join("\n    ") : "none"}`);
  if (error) console.log(`  ERROR: ${error.message}`);
  console.log(`  RESULT: ${ok ? "PASS" : "FAIL"}${control ? " (control: a violation was expected)" : ""}`);
  steps.push({ name, ok, control, csp: cur.csp.length, cspConsole: cspConsole.length });
}

/** Valid bodies for the device page, as the assets and telemetry services would send them. */
function devicePayloads() {
  const device = {
    device_id: DEVICE,
    name: "Boiler 1",
    hardware_id: "hw-1",
    model_id: MODEL,
    transport: "modbus_tcp",
    host: "10.0.0.1",
    port: 502,
    unit_id: 1,
    poll_interval_ms: 1000,
    revision: 1,
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
  };
  const model = {
    model_id: MODEL,
    name: "Boiler model",
    revision: 1,
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
    points: [{ key: "temperature", function_code: 3, address: 0, data_type: "f32", byte_order: "abcd", description: null, length: null, scale: null, offset: null }],
  };
  const now = Date.now();
  const iso = (t) => new Date(t).toISOString();
  const latest = { device_id: DEVICE, earliest: iso(now - 3_600_000), points: [{ point: "temperature", time: iso(now - 1000), value: { type: "float", value: 21.5 } }] };
  const series = {
    device_id: DEVICE,
    point: "temperature",
    tz: "UTC",
    interval: "1m",
    from: iso(now - 3_600_000),
    to: iso(now),
    truncated: false,
    earliest: iso(now - 3_600_000),
    points: [0, 1, 2, 3].map((i) => ({ time: iso(now - 3_600_000 + i * 600_000), count: 10, last: { type: "float", value: 20 + i }, min: 19 + i, max: 21 + i, avg: 20 + i })),
  };
  return { device, model, latest, series };
}

const openLogin = async (page, locale = "en") => {
  await page.goto(`${BASE}/login`);
  await heading(page, T(locale, "page.login.title"));
};
const home = async (page) => {
  await page.goto(`${BASE}/`);
  await heading(page, T("en", "page.home.title"));
};
const patchPreferences = (page) => page.waitForResponse((r) => r.url().includes("/api/identity/v1/preferences") && r.request().method() === "PATCH");

// ---------------------------------------------------------------------------------------------
// 0 Positive controls: the listener and the production CSP really do report what zod's probe would.
await step(
  "0a control: a same-origin script that calls `new Function` is blocked and reported",
  async () => {
    // page.evaluate runs outside the page's CSP, so the probe has to be a script the page loads
    // itself: a same-origin file, which `script-src 'self'` lets run, with the CSP then judging
    // its eval the way it would judge zod's probe.
    await rules([
      respond("control script", "GET", "/static/control-eval.js", 200, 'try { new Function(""); window.__evalProbe = "allowed"; } catch (e) { window.__evalProbe = e.name; }', "text/javascript"),
    ]);
    const page = await fresh({ signedIn: false });
    await openLogin(page);
    await page.evaluate(
      () =>
        new Promise((resolve) => {
          const script = document.createElement("script");
          script.src = "/static/control-eval.js";
          script.onload = resolve;
          script.onerror = resolve;
          document.head.append(script);
        }),
    );
    note(`new Function("") inside the page's own script -> ${await page.evaluate(() => window.__evalProbe)}`);
    await sleep(300);
    if (!cur.csp.some((c) => c.effective === "script-src" && c.blocked === "eval")) throw new Error("no script-src eval violation was reported");
  },
  { control: true },
);
await step(
  "0d control: the same build with zod's `jitless` switched off reports the probe on load",
  async () => {
    await fetch(`${CONTROL}/web`, { method: "PUT", body: "http://127.0.0.1:28290" });
    try {
      const page = await fresh({ signedIn: false });
      await openLogin(page);
      await sleep(500);
      const probe = cur.csp.find((c) => c.effective === "script-src" && c.blocked === "eval");
      note(`violation from the bundle's own code: ${JSON.stringify(probe ?? null)}`);
      if (!probe) throw new Error("the build without jitless did not report the eval probe");
    } finally {
      await fetch(`${CONTROL}/web`, { method: "PUT", body: "" });
    }
  },
  { control: true },
);
await step(
  "0b control: a cross-origin image is blocked and reported (img-src)",
  async () => {
    const page = await fresh({ signedIn: false });
    await openLogin(page);
    await page.evaluate(() => {
      const img = document.createElement("img");
      img.src = "https://example.invalid/x.png";
      document.body.append(img);
    });
    await sleep(500);
    if (!cur.csp.some((c) => c.effective === "img-src")) throw new Error("no img-src violation was reported");
  },
  { control: true },
);
await step(
  "0c control: an HTML string written to the DOM is reported (Trusted Types, report-only)",
  async () => {
    const page = await fresh({ signedIn: false });
    await openLogin(page);
    await page.evaluate(() => {
      const div = document.createElement("div");
      div.innerHTML = "<b>x</b>";
    });
    await sleep(300);
    if (!cur.csp.some((c) => c.disposition === "report" && c.effective === "require-trusted-types-for")) {
      throw new Error("no Trusted Types report was recorded");
    }
  },
  { control: true },
);

// ---------------------------------------------------------------------------------------------
// A (item 7): deep links, language switch, error pages.
await step("A1 signed out: deep link /settings goes to the login page, and refreshes there", async () => {
  const page = await fresh({ signedIn: false });
  await page.goto(`${BASE}/settings`);
  await heading(page, T("en", "page.login.title"));
  expectEqual("url after the deep link", new URL(page.url()).pathname + new URL(page.url()).search, "/login?next=%2Fsettings");
  await page.reload();
  await heading(page, T("en", "page.login.title"));
  expectEqual("url after refresh", new URL(page.url()).pathname + new URL(page.url()).search, "/login?next=%2Fsettings");
  expectDocumentCsp();
});

await step("A2 signed in: deep links /settings, /, /devices/<id> and an unknown path load and refresh", async () => {
  const page = await fresh();
  await page.goto(`${BASE}/settings`);
  await heading(page, T("en", "settings.title"));
  note("deep link /settings: heading Settings");
  await page.reload();
  await heading(page, T("en", "settings.title"));
  note("refresh /settings: heading Settings");
  await home(page);
  note("deep link /: heading Home");
  await page.reload();
  await heading(page, T("en", "page.home.title"));
  note("refresh /: heading Home");
  await page.goto(`${BASE}/devices/${DEVICE}`);
  await heading(page, T("en", "device.not_found.title"));
  note(`deep link /devices/<id> (the stand-in answers the assets API with plain-text 404): heading ${T("en", "device.not_found.title")}`);
  await page.reload();
  await heading(page, T("en", "device.not_found.title"));
  await page.goto(`${BASE}/no/such/page`);
  await heading(page, T("en", "page.not_found.title"));
  note(`deep link /no/such/page: heading ${T("en", "page.not_found.title")}`);
  await page.reload();
  await heading(page, T("en", "page.not_found.title"));
  expectDocumentCsp();
});

await step("A3 signed out: the login page's language picker switches the interface and survives a refresh", async () => {
  const page = await fresh({ signedIn: false });
  await openLogin(page);
  await page.getByLabel(T("en", "page.login.language"), { exact: true }).selectOption("zh-TW");
  await heading(page, T("zh-TW", "page.login.title"));
  expectEqual("html lang", (await docState(page)).lang, "zh-TW");
  await page.reload();
  await heading(page, T("zh-TW", "page.login.title"));
  note(`after refresh the heading is still "${T("zh-TW", "page.login.title")}"`);
  await page.getByLabel(T("zh-TW", "page.login.language"), { exact: true }).selectOption("en");
  await heading(page, T("en", "page.login.title"));
  expectEqual("html lang after switching back", (await docState(page)).lang, "en");
});

await step("A4 signed in: the settings page saves a language and a theme through the real API", async () => {
  const page = await fresh();
  await page.goto(`${BASE}/settings`);
  await heading(page, T("en", "settings.title"));
  const saved = patchPreferences(page);
  await page.getByLabel(T("en", "settings.language"), { exact: true }).selectOption("zh-TW");
  expectEqual("PATCH preferences (language) status", (await saved).status(), 200);
  await heading(page, T("zh-TW", "settings.title"));
  expectEqual("html lang", (await docState(page)).lang, "zh-TW");
  await page.reload();
  await heading(page, T("zh-TW", "settings.title"));
  note("after refresh the server's preference still gives the Chinese interface");
  const themed = patchPreferences(page);
  await page.getByLabel(T("zh-TW", "settings.theme"), { exact: true }).selectOption("dark");
  expectEqual("PATCH preferences (theme) status", (await themed).status(), 200);
  await page.waitForFunction(() => document.documentElement.dataset.theme === "dark");
  expectEqual("data-theme", (await docState(page)).theme, "dark");
  const back = patchPreferences(page);
  await page.getByLabel(T("zh-TW", "settings.language"), { exact: true }).selectOption("en");
  await back;
  await page.getByLabel(T("en", "settings.theme"), { exact: true }).selectOption("system");
  await heading(page, T("en", "settings.title"));
  note("language and theme set back");
});

await step("A5 error display: the session answer is 503 (plain text), then 401 (the ingress's)", async () => {
  const page = await fresh();
  await rules([respond("session 503", "GET", "/api/identity/v1/sessions/current", 503, "upstream unavailable")]);
  await page.goto(`${BASE}/`);
  await heading(page, T("en", "page.unavailable.title"));
  note(`503 on sessions/current: heading ${T("en", "page.unavailable.title")}`);
  await rules([respond("session 401", "GET", "/api/identity/v1/sessions/current", 401, "unauthenticated")]);
  await page.reload();
  await heading(page, T("en", "page.login.title"));
  expectEqual("url after a 401 on the session", new URL(page.url()).pathname, "/login");
});

// ---------------------------------------------------------------------------------------------
// B (item 8): the paths through zod.
await step("B1 unwrap, success: session, preferences and a device page with valid bodies", async () => {
  const page = await fresh();
  const { device, model, latest, series } = devicePayloads();
  await rules([
    respond("device ok", "GET", `/api/assets/v1/devices/${DEVICE}`, 200, device),
    respond("model ok", "GET", `/api/assets/v1/device-models/${MODEL}`, 200, model),
    respond("latest ok", "GET", `/api/telemetry/v1/devices/${DEVICE}/latest`, 200, latest),
    respond("series ok", "GET", `/api/telemetry/v1/devices/${DEVICE}/series`, 200, series),
  ]);
  const seriesAnswered = page.waitForResponse((r) => r.url().includes("/series"), { timeout: 60_000 });
  await page.goto(`${BASE}/devices/${DEVICE}`);
  await show(page, "Boiler 1");
  note("device page with valid device, model, latest and series bodies: shows Boiler 1");
  await show(page, "21.5", "latest value 21.5");
  note("latest value 21.5 shown");
  expectEqual("series request status", (await seriesAnswered).status(), 200);
  await sleep(500);
});

await step("B2a unwrap, failure: 403 tenant_suspended body (error body parsed) shows the suspension page", async () => {
  const page = await fresh();
  const body = { code: "tenant_suspended", message_key: "error.tenant_suspended" };
  await rules([respond("device 403 suspended", "GET", `/api/assets/v1/devices/${DEVICE}`, 403, body)]);
  await page.goto(`${BASE}/devices/${DEVICE}`);
  await heading(page, T("en", "page.tenant_suspended.title"));
  note(`heading ${T("en", "page.tenant_suspended.title")}`);
});

await step("B2b unwrap, failure: error bodies of other shapes on the device request", async () => {
  const cases = [
    ["500 M1 body with details and field_errors that hold junk", 500, { code: "internal", message_key: "error.internal", details: { max: 5, nested: { a: 1 }, flag: true, text: "x" }, field_errors: [7, { nope: 1 }, { field: "/a", message_key: "x", details: { n: Number.POSITIVE_INFINITY } }, { field: "/b", message_key: "error.internal" }] }, T("en", "device.load_failed", { reason: T("en", "error.internal") })],
    ["500 body with an unknown code (not an M1 body)", 500, { code: "made_up", message_key: "x" }, T("en", "device.load_failed", { reason: "" }).replace(/\s*$/, "")],
    ["409 conflict body", 409, { code: "conflict", message_key: "error.conflict" }, T("en", "error.conflict")],
    ["500 JSON array body", 500, [1, 2, 3], T("en", "device.load_failed", { reason: "" }).replace(/\s*$/, "")],
    ["500 text/plain body", 500, "boom", T("en", "device.load_failed", { reason: "" }).replace(/\s*$/, "")],
    ["500 empty JSON null body", 500, "null", T("en", "device.load_failed", { reason: "" }).replace(/\s*$/, "")],
  ];
  for (const [label, status, body, shown] of cases) {
    const page = await fresh();
    await rules([respond(label, "GET", `/api/assets/v1/devices/${DEVICE}`, status, body, body === "null" ? "application/json" : undefined)]);
    await page.goto(`${BASE}/devices/${DEVICE}`);
    await show(page, shown.slice(0, 40), `the error of case "${label}"`);
    const text = (await page.locator("[role=alert]").first().innerText()).replace(/\s+/g, " ");
    note(`${label}: page shows "${text}"`);
  }
});

await step("B2c unwrap, failure: 422 with field_errors on a preferences change; 409; 500", async () => {
  const page = await fresh();
  await page.goto(`${BASE}/settings`);
  await heading(page, T("en", "settings.title"));
  const attempts = [
    ["422 field_errors with one valid entry and junk", respond("patch 422", "PATCH", "/api/identity/v1/preferences", 422, { code: "validation_failed", message_key: "error.validation_failed", field_errors: [3, { field: "/theme", message_key: "preferences.theme_invalid" }, { bogus: true }] }), "theme", "dark", T("en", "preferences.theme_invalid")],
    ["409 conflict", respond("patch 409", "PATCH", "/api/identity/v1/preferences", 409, { code: "conflict", message_key: "error.conflict" }), "theme", "light", T("en", "settings.save_failed", { reason: T("en", "error.conflict") })],
    ["500 plain text", respond("patch 500", "PATCH", "/api/identity/v1/preferences", 500, "boom"), "theme", "dark", T("en", "settings.save_failed", { reason: "" }).slice(0, 12)],
  ];
  for (const [label, rule, field, value, shown] of attempts) {
    await rules([rule]);
    await page.getByLabel(T("en", `settings.${field}`), { exact: true }).selectOption(value);
    await show(page, shown, `the message of case "${label}"`);
    note(`${label}: shows "${shown}"`);
  }
});

await step("B3a ResponseShapeError: preferences 200 with a language that is not allowed", async () => {
  const page = await fresh();
  await rules([{ name: "preferences language 5", method: "GET", path: "/api/identity/v1/preferences", patch: { set: { language: 5 } } }]);
  await page.goto(`${BASE}/settings`);
  await show(page, T("en", "settings.load_failed"), "settings.load_failed");
  note("the settings page says the preferences could not be loaded (the 2xx body was rejected by the schema)");
});

await step("B3b ResponseShapeError: session 200 without `tenants`", async () => {
  const page = await fresh();
  await rules([{ name: "session without tenants", method: "GET", path: "/api/identity/v1/sessions/current", patch: { drop: ["tenants"] } }]);
  await page.goto(`${BASE}/`);
  await heading(page, T("en", "page.unavailable.title"));
  note(`the layout shows "${T("en", "page.unavailable.title")}" (the session body was rejected by the schema)`);
});

await step("B3c ResponseShapeError: device 200 with a wrong type; then latest with an unknown reading type and a series of the wrong shape", async () => {
  const page = await fresh();
  await rules([respond("device wrong shape", "GET", `/api/assets/v1/devices/${DEVICE}`, 200, { device_id: 1 })]);
  await page.goto(`${BASE}/devices/${DEVICE}`);
  await show(page, T("en", "device.load_failed", { reason: "" }).slice(0, 25), "device.load_failed");
  note('device 200 {"device_id":1}: the device page shows its load failure');

  const { device, model, latest, series } = devicePayloads();
  latest.points[0].value = { type: "weird", value: 1 };
  series.points = "none";
  await rules([
    respond("device ok", "GET", `/api/assets/v1/devices/${DEVICE}`, 200, device),
    respond("model ok", "GET", `/api/assets/v1/device-models/${MODEL}`, 200, model),
    respond("latest unknown reading type", "GET", `/api/telemetry/v1/devices/${DEVICE}/latest`, 200, latest),
    respond("series points is a string", "GET", `/api/telemetry/v1/devices/${DEVICE}/series`, 200, series),
  ]);
  await page.reload();
  await show(page, T("en", "device.latest.failed", { reason: "" }).slice(0, 30), "device.latest.failed");
  note("latest with a reading of an unknown type: the latest-values card shows its failure (the discriminated union rejected it)");
  await show(page, T("en", "device.history.failed", { reason: "" }).slice(0, 28), "device.history.failed");
  note("series whose points is a string: the history card shows its failure");
});

await step("B3e login: error bodies (429, 401), a valid code-sent answer, and ResponseShapeErrors on both steps", async () => {
  const page = await fresh({ signedIn: false });
  await openLogin(page);
  const email = page.getByLabel(T("en", "page.login.email"), { exact: true });
  const send = () => page.getByRole("button", { name: T("en", "page.login.send_code"), exact: true }).click();
  await email.fill("someone@example.com");

  await rules([respond("send 429", "POST", "/auth/login", 429, { code: "rate_limited", message_key: "error.rate_limited" })]);
  await send();
  await show(page, T("en", "error.rate_limited"), "error.rate_limited");
  note("send_code 429 with an M1 body: the form shows the translated message");

  await rules([respond("send 200 wrong shape", "POST", "/auth/login", 200, { ok: true })]);
  await send();
  await sleep(1500);
  await heading(page, T("en", "page.login.title"));
  note(`send_code 200 {"ok":true}: still on the e-mail step; alert: ${JSON.stringify((await page.locator("[role=alert]").allInnerTexts()).map((t) => t.replace(/\s+/g, " ")))}`);

  await rules([respond("send ok", "POST", "/auth/login", 200, { code_length: 6, expires_in_secs: 600, resend_after_secs: 30 })]);
  await send();
  await heading(page, T("en", "page.login.code_title"));
  note("send_code 200 with a valid answer: the code step opens (union schema, success)");

  const code = page.getByLabel(T("en", "page.login.code"), { exact: true });
  const verify = () => page.getByRole("button", { name: T("en", "page.login.verify"), exact: true }).click();
  await code.fill("123456");
  await rules([respond("verify 401", "POST", "/auth/login", 401, { code: "unauthenticated", message_key: "error.unauthenticated" })]);
  await verify();
  await show(page, T("en", "page.login.code_rejected"), "page.login.code_rejected");
  note("verify_code 401 with an M1 body: the wrong-code message");

  await rules([respond("verify 200 wrong shape", "POST", "/auth/login", 200, { redirect_to: 5 })]);
  await verify();
  await sleep(1500);
  await heading(page, T("en", "page.login.code_title"));
  note(`verify_code 200 {"redirect_to":5}: still on the code step; alert: ${JSON.stringify((await page.locator("[role=alert]").allInnerTexts()).map((t) => t.replace(/\s+/g, " ")))}`);
});

// B4: the browser's own copy of the preferences, read on the first paint.
const storageCases = [
  ["valid v2", json({ v: 2, language: "zh-TW", aiLanguage: "en", timeZone: "Asia/Taipei", theme: "dark" }), "zh-TW", "dark"],
  ["old v1 (kept a resolved locale)", json({ v: 1, locale: "zh-TW", language: "zh-TW", timeZone: "Asia/Taipei", theme: "dark" }), "en", "light"],
  ["v2, unknown language and theme, unknown time zone", json({ v: 2, language: "klingon", aiLanguage: "nope", timeZone: "Mars/Olympus", theme: "neon" }), "en", "light"],
  ["v2, one good field among bad ones", json({ v: 2, language: "zh-TW", aiLanguage: 5, timeZone: 7, theme: ["dark"] }), "zh-TW", "light"],
  ["v2, only the theme is good", json({ v: 2, language: null, theme: "dark" }), "en", "dark"],
  ["v2 with __proto__ and extra members", '{"v":2,"__proto__":{"theme":"dark"},"constructor":{"x":1},"language":"zh-TW","extra":[1,2]}', "zh-TW", "light"],
  ["not JSON", "{not json", "en", "light"],
  ["JSON null", "null", "en", "light"],
  ["JSON array", "[]", "en", "light"],
  ["JSON string", '"zh-TW"', "en", "light"],
  ["JSON number", "42", "en", "light"],
  ["empty string", "", "en", "light"],
];
await step("B4a preferences read from localStorage with old or invalid content (signed out, first paint)", async () => {
  for (const [label, raw, lang, theme] of storageCases) {
    const page = await fresh({ signedIn: false, storage: raw });
    await openLogin(page, lang);
    const state = await docState(page);
    note(`${label}: stored ${JSON.stringify(raw).slice(0, 90)} -> lang ${state.lang}, theme ${state.theme}`);
    if (state.lang !== lang || state.theme !== theme) throw new Error(`${label}: expected lang ${lang} theme ${theme}, got ${state.lang} ${state.theme}`);
  }
});
await step("B4b preferences read from localStorage with invalid content, then replaced by the server's (signed in)", async () => {
  for (const raw of ['{"v":2,"language":"klingon","theme":"neon"}', "{not json"]) {
    const page = await fresh({ storage: raw });
    const answered = page.waitForResponse((r) => r.url().includes("/api/identity/v1/preferences") && r.request().method() === "GET", { timeout: 60_000 });
    await page.goto(`${BASE}/settings`);
    await heading(page, T("en", "settings.title"));
    const server = await (await answered).json();
    const expected = JSON.stringify({ v: 2, language: server.language, aiLanguage: server.ai_language, timeZone: server.time_zone, theme: server.theme });
    await page
      .waitForFunction((want) => localStorage.getItem("trustiot.preferences") === want, expected, { timeout: 60_000 })
      .catch(() => {
        throw new Error(`the stored copy never became the server's values ${expected}`);
      });
    const state = await docState(page);
    expectEqual("stored copy equals the server's preferences", state.stored, expected);
    note(`stored ${JSON.stringify(raw)} -> after the server answered, stored ${state.stored}; lang ${state.lang}, theme ${state.theme}`);
  }
});

// Last: it ends the real session.
await step("B3d ResponseShapeError: a 200 body where none is expected (sign out); then the real 204", async () => {
  const page = await fresh();
  await home(page);
  await rules([respond("logout 200 with a body", "DELETE", "/api/identity/v1/sessions/current", 200, { unexpected: true })]);
  await page.getByRole("button", { name: T("en", "user_menu.label") }).click();
  await page.getByRole("menuitem", { name: T("en", "user_menu.sign_out") }).click();
  await show(page, T("en", "user_menu.sign_out_failed", { reason: "" }).slice(0, 18), "user_menu.sign_out_failed");
  note("sign-out reported a failure: the 200 body was rejected by z.undefined()");
  await rules([]);
  await page.getByRole("button", { name: T("en", "user_menu.dismiss") }).click().catch(() => {});
  await page.getByRole("button", { name: T("en", "user_menu.label") }).click();
  await page.getByRole("menuitem", { name: T("en", "user_menu.sign_out") }).click();
  await heading(page, T("en", "page.login.title"));
  note("the real DELETE /api/identity/v1/sessions/current (204, z.undefined()) returned to the login page");
});

const failed = steps.filter((s) => !s.ok);
const real = steps.filter((s) => !s.control);
console.log("\n## summary");
console.log(`steps: ${steps.length} (${steps.filter((s) => s.control).length} controls), failed: ${failed.length}`);
console.log(`securitypolicyviolation events outside the controls: ${real.reduce((n, s) => n + s.csp, 0)}`);
console.log(`console messages naming a CSP or Trusted Types problem outside the controls: ${real.reduce((n, s) => n + s.cspConsole, 0)}`);
for (const s of failed) console.log(`FAILED: ${s.name}`);
await browser.close();
process.exit(failed.length === 0 ? 0 : 1);
