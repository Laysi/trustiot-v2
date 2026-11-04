// Builds the API answers the browser check replays (fixtures.json): the identity service's session
// and preferences, and the dashboard API's answers for two dashboards, taken from the same
// in-memory server the component tests use (frontend/src/test/fake-dashboards.ts), so the bodies
// are the ones the SPA's zod schemas and the API spec agree on.
//
//   bun docs/test-reports/data/2026-11-04-997-custom-page-wiring/scripts/gen-fixtures.ts <out.json>
import { writeFileSync } from "node:fs";

import { SPECS } from "../../../../../frontend/src/dashboard/fixtures.ts";
import { FakeDashboardServer } from "../../../../../frontend/src/test/fake-dashboards.ts";

const out = process.argv[2];
if (out === undefined) throw new Error("usage: gen-fixtures.ts <out.json>");

/** A page that writes the first power cell and the line name through the runtime, as the AI's pages do. */
const CLEAN_PAGE = `<p>Power <b id="v"></b> kW on <b id="l"></b></p>
<script>
  tq.on("data", (tq) => {
    tq.text(document.getElementById("v"), "c|power|kw|0", ",.2f");
    tq.text(document.getElementById("l"), "c|power|line|0");
  });
</script>`;

/** The same page, but two seconds after it has drawn it writes a number the data does not have. */
const TAMPERING_PAGE = CLEAN_PAGE.replace(
  "</script>",
  `setTimeout(() => { document.getElementById("v").textContent = "999.99"; }, 2000);\n</script>`,
);

const server = new FakeDashboardServer();
const clean = server.addDashboard({
  title: "Plant power",
  versions: [[SPECS.kpi, { kind: "custom_page" }]],
  page: CLEAN_PAGE,
});
const tampering = server.addDashboard({
  title: "Plant power (tampered)",
  versions: [[{ kind: "custom_page" }]],
  page: TAMPERING_PAGE,
});

async function call(method: string, path: string, body?: unknown): Promise<unknown> {
  const response = await server.fetch(
    new Request(`http://localhost${path}`, {
      method,
      ...(body === undefined ? {} : { body: JSON.stringify(body), headers: { "content-type": "application/json" } }),
    }),
  );
  if (!response.ok) throw new Error(`${method} ${path}: ${response.status}`);
  return response.json();
}

const dashboards: Record<string, unknown> = {};
for (const [name, dashboard] of Object.entries({ clean, tampering })) {
  const view = (await call("GET", `/api/assistant/v1/dashboards/${dashboard.id}`)) as {
    version: { spec: { datasets: { name: string }[] } };
  };
  const runs: Record<string, unknown> = {};
  for (const { name: dataset } of view.version.spec.datasets) {
    runs[dataset] = await call("POST", `/api/assistant/v1/dashboards/${dashboard.id}/dataset-runs`, {
      version: 1,
      dataset,
    });
  }
  dashboards[name] = { id: dashboard.id, view, runs };
}

const tenant = { tenant_id: "00000000-0000-4000-8000-0000000000b1", name: "Acme Factory", role: "editor", state: "active" };
const fixtures = {
  session: {
    user_id: "00000000-0000-4000-8000-0000000000a1",
    email: "viewer@example.test",
    platform_admin: false,
    active_tenant: tenant,
    tenants: [tenant],
  },
  preferences: { language: "en", ai_language: "en", time_zone: "UTC", theme: "light" },
  dashboards,
};
writeFileSync(out, `${JSON.stringify(fixtures, null, 1)}\n`);
console.log(`wrote ${out}: ${Object.keys(dashboards).join(", ")}`);
