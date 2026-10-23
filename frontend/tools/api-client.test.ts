import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { OUTPUT_PATH, render, SPEC_PATH } from "./api-client.ts";

describe("api client generation", () => {
  test("renders the paths and schemas of a spec, with the do-not-edit header", async () => {
    const output = await render({
      openapi: "3.1.0",
      info: { title: "t", version: "0" },
      paths: {
        "/api/x/v1/thing": {
          get: { operationId: "getThing", responses: { "200": { description: "ok" } } },
        },
      },
      components: { schemas: { Thing: { type: "object", properties: { id: { type: "string" } }, required: ["id"] } } },
    });
    expect(output.startsWith("// Generated from api/openapi.json")).toBe(true);
    expect(output).toContain('"/api/x/v1/thing"');
    expect(output).toContain("Thing: {");
  });

  test("the same spec renders the same file", async () => {
    const spec = JSON.parse(readFileSync(SPEC_PATH, "utf8"));
    expect(await render(spec)).toBe(await render(spec));
  });

  test("the committed client matches api/openapi.json (api:check)", async () => {
    expect(readFileSync(OUTPUT_PATH, "utf8")).toBe(await render(JSON.parse(readFileSync(SPEC_PATH, "utf8"))));
  });
});
