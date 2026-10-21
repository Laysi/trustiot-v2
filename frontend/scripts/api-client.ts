/**
 * Generates the API client's types, src/api/schema.gen.ts, from api/openapi.json, the spec that
 * tools/openapi generates from the services' code. The typed fetch (openapi-fetch) reads only
 * these types, so a changed API type changes what the frontend compiles against.
 *
 * Usage, from the repository root:
 *   bun run --cwd frontend api:generate   writes the file
 *   bun run --cwd frontend api:check      compares instead; exits 1 when the file is out of date
 */

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import openapiTS, { astToString, type OpenAPI3 } from "openapi-typescript";

export const SPEC_PATH = fileURLToPath(new URL("../../api/openapi.json", import.meta.url));
export const OUTPUT_PATH = fileURLToPath(new URL("../src/api/schema.gen.ts", import.meta.url));

const HEADER =
  "// Generated from api/openapi.json by frontend/scripts/api-client.ts. Do not edit by hand:\n" +
  "// run `bun run --cwd frontend api:generate` after `cargo run -p openapi`.\n\n";

/** The file's content for `spec` (a parsed OpenAPI document). */
export async function render(spec: OpenAPI3): Promise<string> {
  // openapi-typescript resolves references in place; the copy keeps the caller's object intact.
  const ast = await openapiTS(structuredClone(spec));
  return HEADER + astToString(ast);
}

async function main(args: readonly string[]): Promise<number> {
  const check = args.includes("--check");
  const unknown = args.filter((arg) => arg !== "--check");
  if (unknown.length > 0) {
    console.error(`unknown argument(s): ${unknown.join(" ")}\nusage: bun scripts/api-client.ts [--check]`);
    return 2;
  }
  const expected = await render(JSON.parse(readFileSync(SPEC_PATH, "utf8")));
  if (!check) {
    writeFileSync(OUTPUT_PATH, expected);
    return 0;
  }
  let current = "";
  try {
    current = readFileSync(OUTPUT_PATH, "utf8");
  } catch {
    // A missing file is out of date like any other.
  }
  if (current !== expected) {
    console.error(
      "frontend/src/api/schema.gen.ts does not match api/openapi.json.\n" +
        "Run `bun run --cwd frontend api:generate` and commit the result.",
    );
    return 1;
  }
  return 0;
}

if (import.meta.main) {
  process.exit(await main(process.argv.slice(2)));
}
