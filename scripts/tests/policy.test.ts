// Repository rules for the scripts: no fixed credentials, no wide port binding, no unpinned
// image. These complement gitleaks, which looks for known secret shapes rather than for
// "a literal assigned to something called password".
import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

import { discoverSeeds } from "../lib/seed.ts";

const SCRIPTS = join(import.meta.dir, "..");
const REPO = join(SCRIPTS, "..");

function filesUnder(dir: string, skip: (path: string) => boolean): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (skip(path)) continue;
    if (entry.isDirectory()) out.push(...filesUnder(path, skip));
    else out.push(path);
  }
  return out;
}

// Everything that ships and runs: not the tests, which spell out secrets on purpose.
const SHIPPED = filesUnder(SCRIPTS, (path) => relative(SCRIPTS, path).startsWith("tests"));

/** A string literal assigned to a name that looks like a secret. */
const FIXED_SECRET =
  /(password|passwd|secret|token|api_?key|credential)\w*["']?\s*[:=]\s*(["'`])(?!\2)[^"'`$]{3,}\2/i;
/** Values that are deliberately wrong, used to prove that a bad login is refused. */
const ALLOWED_LITERALS = ["not-the-password"];

describe("scripts/ holds no fixed credentials", () => {
  test("no literal is assigned to a secret-looking name", () => {
    const offenders: string[] = [];
    for (const file of SHIPPED.filter((path) => /\.(ts|sql|md|json)$/.test(path))) {
      readFileSync(file, "utf8")
        .split("\n")
        .forEach((line, index) => {
          if (FIXED_SECRET.test(line) && !ALLOWED_LITERALS.some((allowed) => line.includes(allowed))) {
            offenders.push(`${relative(REPO, file)}:${index + 1}: ${line.trim()}`);
          }
        });
    }
    expect(offenders).toEqual([]);
  });

  test("the rule itself catches a fixed password", () => {
    expect(FIXED_SECRET.test('const password = "hunter2"')).toBe(true);
    expect(FIXED_SECRET.test("POSTGRES_PASSWORD: 'changeme'")).toBe(true);
    expect(FIXED_SECRET.test("password: credentials.mqtt.password")).toBe(false);
  });

  test("seed files create no login role or password", () => {
    for (const file of SHIPPED.filter((path) => path.endsWith(".sql"))) {
      expect(readFileSync(file, "utf8"), file).not.toMatch(/\bPASSWORD\b|CREATE\s+(USER|ROLE)/i);
    }
  });
});

describe("scripts/ never opens a port to other machines", () => {
  test("no wildcard bind address and no host networking", () => {
    for (const file of SHIPPED.filter((path) => path.endsWith(".ts"))) {
      const text = readFileSync(file, "utf8");
      // The Mosquitto and Postgres configs bind inside the container; the host side is the
      // publish address, which stack.test.ts covers. Nothing may name the wildcard for a host.
      expect(text, file).not.toMatch(/--network[ =]host|--publish-all|["'`]0\.0\.0\.0:/);
    }
  });
});

describe("seed discovery", () => {
  test("finds the platform seeds in the order they apply", () => {
    const files = discoverSeeds(REPO).filter((file) => file.origin === "platform");
    expect(files.map((file) => `${file.store}/${file.path.split("/").at(-1)}`)).toEqual([
      "postgres/001-dev-smoke.sql",
      "clickhouse/001-database.sql",
      "clickhouse/002-table.sql",
      "clickhouse/003-rows.sql",
    ]);
  });

  test("picks up a service's own seeds after the platform's, by service name", () => {
    const root = mkdtempSync(join(tmpdir(), "trustiot-seed-test-"));
    try {
      for (const [path, name] of [
        ["scripts/seed/postgres", "001-a.sql"],
        ["services/zeta/seed/postgres", "002-b.sql"],
        ["services/zeta/seed/postgres", "001-b.sql"],
        ["services/alpha/seed/clickhouse", "001-c.sql"],
        ["services/alpha/seed/clickhouse", "notes.txt"],
        ["services/beta", "README.md"],
      ] as const) {
        mkdirSync(join(root, path), { recursive: true });
        writeFileSync(join(root, path, name), "SELECT 1");
      }
      expect(discoverSeeds(root).map((file) => relative(root, file.path))).toEqual([
        "scripts/seed/postgres/001-a.sql",
        "services/alpha/seed/clickhouse/001-c.sql",
        "services/zeta/seed/postgres/001-b.sql",
        "services/zeta/seed/postgres/002-b.sql",
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("an empty tree has no seeds", () => {
    const root = mkdtempSync(join(tmpdir(), "trustiot-seed-test-"));
    try {
      expect(discoverSeeds(root)).toEqual([]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
