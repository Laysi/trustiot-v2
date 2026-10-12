/**
 * Tests of the Postgres image (deploy/images/postgres, README.md there):
 *
 *   bun test deploy/images/postgres
 *   POSTGRES_IMAGE=<local image> bun test deploy/images/postgres
 *
 * Without POSTGRES_IMAGE the image is built from this directory the way release.yml builds it
 * (podman build --platform linux/arm64). The checks of the image's files run anywhere. The
 * checks that start Postgres run the arm64 image, so they need an arm64 host (the postgres-image
 * job in ci.yml runs on one) or an arm64 emulator registered with binfmt_misc. Without either
 * they are skipped locally and fail when CI is set.
 *
 * A build on an arm64 host runs every stage natively, so it would not notice a RUN in an arm64
 * stage, which release.yml's x86 build without an emulator cannot run. The Containerfile tests
 * below check that statically instead.
 *
 * Postgres starts the way CloudNativePG runs its operand: UID and GID 26, read-only root
 * filesystem, every capability dropped, no privilege escalation, data and sockets on writable
 * mounts only.
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { containerBuildArgs } from "../../../scripts/lib/build-context.ts";
import { aarch64ElfProblem, baseImageProblems } from "../../../scripts/release/lib.ts";

const DIR = import.meta.dir;
const CONTAINERFILE = readFileSync(join(DIR, "Containerfile"), "utf8");
const PG_MAJOR = "18";
const LIB_DIR = `/usr/lib/postgresql/${PG_MAJOR}/lib`;
const EXTENSION_DIR = `/usr/share/postgresql/${PG_MAJOR}/extension`;
/** CloudNativePG's default postgresUID and postgresGID. */
const POSTGRES_UID = "26";
const SOCKET_DIR = "/controller/run";
const PGDATA = "/var/lib/postgresql/data/pgdata";
const BUILD_TIMEOUT_MS = 30 * 60_000;
const START_TIMEOUT_MS = 5 * 60_000;

/** The value of `ARG <name>=<value>` in the Containerfile. */
function containerfileArg(name: string): string {
  const match = new RegExp(`^ARG ${name}=(\\S+)$`, "m").exec(CONTAINERFILE);
  if (match?.[1] === undefined) {
    throw new Error(`the Containerfile has no ARG ${name}=<value>`);
  }
  return match[1];
}

const AGE_VERSION = containerfileArg("AGE_VERSION");

interface Result {
  exitCode: number;
  stdout: string;
  stderr: string;
}

async function exec(cmd: string[], stdin?: string): Promise<Result> {
  const proc = Bun.spawn(cmd, { stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin), stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, exitCode] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  return { exitCode, stdout, stderr };
}

async function run(cmd: string[], stdin?: string): Promise<string> {
  const result = await exec(cmd, stdin);
  if (result.exitCode !== 0) {
    throw new Error(`${cmd.slice(0, 3).join(" ")} exited with ${result.exitCode}:\n${result.stderr}${result.stdout}`);
  }
  return result.stdout;
}

/**
 * Whether podman can run linux/arm64 containers: its host is arm64 (also a remote podman, with
 * CONTAINER_HOST set), or this machine has an arm64 emulator registered with binfmt_misc.
 */
async function canRunArm64(): Promise<boolean> {
  const host = await exec(["podman", "info", "--format", "{{.Host.Arch}}"]);
  if (host.exitCode === 0 && host.stdout.trim() === "arm64") {
    return true;
  }
  const handler = "/proc/sys/fs/binfmt_misc/qemu-aarch64";
  return existsSync(handler) && readFileSync(handler, "utf8").startsWith("enabled");
}

/** The stages of a Containerfile: the FROM line and the instructions under it. */
function stages(containerfile: string): { from: string; instructions: string[] }[] {
  const result: { from: string; instructions: string[] }[] = [];
  for (const line of containerfile.replace(/\\\r?\n/g, " ").split(/\r?\n/)) {
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("#")) {
      continue;
    }
    if (/^FROM\s/i.test(trimmed)) {
      result.push({ from: trimmed, instructions: [] });
    } else {
      result.at(-1)?.instructions.push(trimmed);
    }
  }
  return result;
}

const runnable = await canRunArm64();

describe("Containerfile", () => {
  test("pins every base image by digest", () => {
    expect(baseImageProblems(CONTAINERFILE)).toEqual([]);
  });

  test("runs no program in a linux/arm64 stage, so release.yml builds it without an emulator", () => {
    const all = stages(CONTAINERFILE);
    expect(all.length).toBeGreaterThan(1);
    for (const stage of all) {
      if (!stage.from.includes("--platform=$BUILDPLATFORM")) {
        expect({ stage: stage.from, run: stage.instructions.filter((line) => /^RUN\s/i.test(line)) }).toEqual({ stage: stage.from, run: [] });
      }
    }
  });

  test("every RUN is plain sh that stops at the first error, because podman ignores SHELL for OCI images", () => {
    const instructions = stages(CONTAINERFILE).flatMap((stage) => stage.instructions);
    expect(instructions.filter((line) => /^SHELL\s/i.test(line))).toEqual([]);
    const runs = instructions.filter((line) => /^RUN\s/i.test(line));
    expect(runs.length).toBeGreaterThan(0);
    for (const line of runs) {
      expect(line).toMatch(/^RUN set -eu;/);
      // bash-only syntax that /bin/sh (dash) rejects.
      expect(line).not.toMatch(/\[\[|\]\]|\$\{[A-Za-z_]+\/|pipefail/);
    }
  });

  test("every download retries connection errors and is verified against a pinned hash", () => {
    const instructions = stages(CONTAINERFILE).flatMap((stage) => stage.instructions);
    const curls = instructions.flatMap((line) => line.match(/\bcurl --[^;]*/g) ?? []);
    // One curl call, inside the `fetch` function that the downloads share.
    expect(curls.length).toBe(1);
    for (const curl of curls) {
      expect(curl).toContain("--fail");
      expect(curl).toContain("--retry-all-errors");
      expect(curl).toContain("--retry-delay");
      expect(curl).toContain("--connect-timeout");
    }
    expect(instructions.join("\n")).toContain("sha256sum --check --strict");
    // The two AGE sources, mirror first, then the archive; both go through the same SHA-512 check.
    expect(containerfileArg("AGE_MIRROR_URL")).toMatch(/^https:\/\/dlcdn\.apache\.org\/age\/PG18$/);
    expect(containerfileArg("AGE_ARCHIVE_URL")).toBe("https://archive.apache.org/dist/age/PG18");
    const loop = /for age_base in "\$\{AGE_MIRROR_URL\}" "\$\{AGE_ARCHIVE_URL\}"; do[\s\S]*?done;/.exec(instructions.join("\n"))?.[0] ?? "";
    expect(loop).toContain('fetch age.tar.gz "${age_base}/');
    expect(loop).toContain('echo "${AGE_SHA512}  age.tar.gz" | sha512sum --check --strict');
  });

  test("ends as the postgres user that CloudNativePG expects", () => {
    const last = stages(CONTAINERFILE).at(-1);
    expect(last?.instructions.filter((line) => /^USER\s/i.test(line)).at(-1)).toBe(`USER ${POSTGRES_UID}`);
  });
});

describe("image", () => {
  let image = process.env.POSTGRES_IMAGE ?? "";
  let files = "";
  let container = "";

  beforeAll(async () => {
    if (image === "") {
      image = "localhost/trustiot-e2e/postgres:image-test";
      // containerBuildArgs checks the build context and the Containerfile against the secret
      // rules first (docs/architecture/secrets.md), like every container build in the repository.
      await run(["podman", ...containerBuildArgs(DIR, ["--platform", "linux/arm64", "--format", "oci", "--tag", image])]);
    }
    // Copy the AGE files out of the image without running it.
    files = mkdtempSync(join(tmpdir(), "trustiot-postgres-image-"));
    const created = (await run(["podman", "create", "--platform", "linux/arm64", image])).trim();
    try {
      for (const path of [`${LIB_DIR}/age.so`, `${EXTENSION_DIR}/age.control`, `${EXTENSION_DIR}/age--${AGE_VERSION}.sql`]) {
        await run(["podman", "cp", `${created}:${path}`, join(files, path.split("/").at(-1) ?? "")]);
      }
    } finally {
      await exec(["podman", "rm", "--force", "--volumes", created]);
    }
  }, BUILD_TIMEOUT_MS);

  afterAll(async () => {
    if (container !== "") {
      await exec(["podman", "rm", "--force", "--volumes", "--time", "0", container]);
    }
    if (files !== "") {
      rmSync(files, { recursive: true, force: true });
    }
  });

  test("is linux/arm64 and runs as UID 26", async () => {
    const [inspected] = JSON.parse(await run(["podman", "image", "inspect", image])) as { Architecture: string; Os: string; Config: { User?: string } }[];
    expect(inspected?.Os).toBe("linux");
    expect(inspected?.Architecture).toBe("arm64");
    expect(inspected?.Config.User).toBe(POSTGRES_UID);
  });

  test(`holds Apache AGE ${AGE_VERSION} built for aarch64`, () => {
    const library = new Uint8Array(readFileSync(join(files, "age.so")));
    expect(aarch64ElfProblem(library)).toBeUndefined();
    // The ELF package note that lets syft list AGE in the release SBOM.
    const text = new TextDecoder("latin1").decode(library);
    expect(text).toContain(`"name":"apache-age","version":"${AGE_VERSION}"`);
    expect(readFileSync(join(files, "age.control"), "utf8")).toContain(`default_version = '${AGE_VERSION}'`);
  });

  describe("running as CloudNativePG runs it", () => {
    const runningTest = runnable ? test : process.env.CI ? test : test.skip;
    if (!runnable) {
      console.log(
        process.env.CI
          ? "this machine cannot run linux/arm64 containers; with CI set the Postgres tests fail"
          : "skipped: this machine cannot run linux/arm64 containers (needs an arm64 host or binfmt_misc qemu-aarch64)",
      );
    }

    async function psql(sql: string, user = "postgres"): Promise<string> {
      return run(["podman", "exec", "--interactive", container, "psql", "--host", SOCKET_DIR, "--username", user, "--dbname", "postgres", "--no-psqlrc", "--quiet", "--tuples-only", "--no-align", "--set", "ON_ERROR_STOP=1", "--file", "-"], sql);
    }

    runningTest(
      "starts with a read-only root filesystem, no capabilities and UID 26",
      async () => {
        expect(runnable).toBe(true);
        const script = [
          "set -euo pipefail",
          `initdb --pgdata ${PGDATA} --username postgres --auth trust --encoding UTF8 --locale C.UTF-8 > /dev/null`,
          `mkdir -p ${SOCKET_DIR}`,
          // AGE in shared_preload_libraries, so sessions of any role get its parser hooks
          // without LOAD (which only superusers may run for libraries outside plugins/).
          `exec postgres -D ${PGDATA} -c listen_addresses= -c unix_socket_directories=${SOCKET_DIR} -c shared_preload_libraries=age`,
        ].join("\n");
        container = (
          await run([
            "podman", "run", "--detach", "--platform", "linux/arm64",
            "--user", `${POSTGRES_UID}:${POSTGRES_UID}`, "--read-only", "--cap-drop", "all", "--security-opt", "no-new-privileges",
            "--tmpfs", "/var/lib/postgresql/data:rw,mode=1777", "--tmpfs", "/controller:rw,mode=1777", "--tmpfs", "/tmp:rw,mode=1777",
            "--entrypoint", "bash", image, "-c", script,
          ])
        ).trim();
        const deadline = Date.now() + START_TIMEOUT_MS;
        while ((await exec(["podman", "exec", container, "pg_isready", "--host", SOCKET_DIR, "--quiet"])).exitCode !== 0) {
          if (Date.now() > deadline) {
            throw new Error(`Postgres did not start:\n${(await exec(["podman", "logs", container])).stderr}`);
          }
          await Bun.sleep(1000);
        }
        // PID 1 is postgres itself (exec above): its real and effective UID are 26.
        expect((await run(["podman", "exec", container, "awk", "/^Uid:/ {print $2, $3}", "/proc/1/status"])).trim()).toBe(`${POSTGRES_UID} ${POSTGRES_UID}`);
        expect((await run(["podman", "exec", container, "bash", "-c", "cat /proc/1/comm"])).trim()).toBe("postgres");
        // The programs CloudNativePG's instance manager calls from PATH.
        await run(["podman", "exec", container, "bash", "-c", "command -v initdb pg_ctl pg_controldata pg_basebackup pg_rewind pg_isready postgres psql"]);
      },
      START_TIMEOUT_MS + 60_000,
    );

    runningTest(
      `CREATE EXTENSION age installs ${AGE_VERSION} and cypher() creates and matches a graph`,
      async () => {
        expect(container).not.toBe("");
        const out = await psql(`
          CREATE EXTENSION age;
          SELECT extversion FROM pg_extension WHERE extname = 'age';
          LOAD 'age';
          SET search_path = ag_catalog, "$user", public;
          SELECT create_graph('plant');
          SELECT * FROM cypher('plant', $$
            CREATE (:Machine {name: 'press-1'})-[:FEEDS]->(:Machine {name: 'oven-2'})
          $$) AS (v agtype);
          SELECT * FROM cypher('plant', $$
            MATCH (a:Machine)-[:FEEDS]->(b:Machine) RETURN a.name, b.name
          $$) AS (a agtype, b agtype);
        `);
        const lines = out.split("\n").filter((line) => line !== "");
        expect(lines).toContain(AGE_VERSION);
        expect(lines).toContain(`"press-1"|"oven-2"`);
      },
      120_000,
    );

    runningTest(
      "a non-superuser role runs a read-only cypher() query without LOAD",
      async () => {
        expect(container).not.toBe("");
        await psql(`
          CREATE ROLE graph_reader LOGIN;
          GRANT USAGE ON SCHEMA ag_catalog, plant TO graph_reader;
          GRANT SELECT ON ALL TABLES IN SCHEMA plant TO graph_reader;
        `);
        const out = await psql(
          `
          BEGIN READ ONLY;
          SET LOCAL search_path = ag_catalog, "$user", public;
          SELECT * FROM cypher('plant', $$ MATCH (m:Machine) RETURN count(m) $$) AS (n agtype);
          COMMIT;
        `,
          "graph_reader",
        );
        expect(out.trim()).toBe("2");
      },
      120_000,
    );
  });
});
