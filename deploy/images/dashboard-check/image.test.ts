/**
 * Tests of the dashboard check image (deploy/images/dashboard-check, README.md there;
 * Laysi/trustiot-v2#150):
 *
 *   bun test deploy/images/dashboard-check
 *   DASHBOARD_CHECK_IMAGE=<local image> bun test deploy/images/dashboard-check
 *
 * Without DASHBOARD_CHECK_IMAGE the image is built the way the nightly builds it, which is the way
 * release.yml builds it (scripts/nightly/service-image.ts buildServiceImage: the cross-compiled
 * binary, the frontend's checked build and this directory's Containerfile). That needs rustup
 * and the pinned zig, cargo-zigbuild and cargo-auditable (`proto install`), and it rebuilds
 * frontend/dist. DASHBOARD_CHECK_SITE=<dir> packs that build output of frontend/ instead of
 * building it.
 *
 * The pages are the sandbox's fixtures (frontend/sandbox/fixtures, frontend/sandbox/README.md
 * 「固定樣本」), all of them, judged by their own matchesExpectation; the fixtures' module needs
 * the frontend's packages (`bun install --cwd frontend --frozen-lockfile`, which the build runs).
 *
 * The container engine is docker, or podman when docker is not installed (the memory limit needs
 * a rootful engine or cgroup delegation, which GitHub's runners give docker); it must run linux/arm64
 * containers (an arm64 host, or an arm64 emulator registered with binfmt_misc). Without one the
 * image tests are skipped locally and fail when CI is set. The Containerfile tests run anywhere.
 *
 * The service runs as the chart runs it (deploy/chart/README.md, 「儀表板檢查」): read-only root
 * filesystem, /tmp a memory-backed file system of the chart's tmpSizeLimit, every capability
 * dropped, no privilege escalation, the engine's default seccomp profile, the chart's memory
 * limit and UID 65532.
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { RunLog } from "../../../scripts/nightly/exec.ts";
import { buildServiceImage, findServiceImageTools } from "../../../scripts/nightly/service-image.ts";
import { targetStageRunProblems } from "../../../scripts/release/containerfile.ts";
import { baseImageProblems } from "../../../scripts/release/lib.ts";

const DIR = import.meta.dir;
const ROOT = resolve(DIR, "..", "..", "..");
const CONTAINERFILE = readFileSync(join(DIR, "Containerfile"), "utf8");
const CHART_VALUES = Bun.YAML.parse(readFileSync(join(ROOT, "deploy", "chart", "values.yaml"), "utf8")) as {
  services: Record<string, { tmpSizeLimit: string; resources: { limits: { memory: string } } }>;
};
const CHART = CHART_VALUES.services["dashboard-check"];
if (CHART === undefined) {
  throw new Error("deploy/chart/values.yaml has no services.dashboard-check");
}
const FIXTURES = join(ROOT, "frontend", "sandbox", "fixtures");
/** A case of frontend/sandbox/fixtures/cases.json. */
interface FixtureCase {
  name: string;
  /** The page's file, relative to the fixtures. */
  page: string;
  /** Whether the frontend's unit test runs it too; the others only a browser can judge. */
  unit: boolean;
  about: string;
  expect: unknown;
}
const FIXTURE_FILE = JSON.parse(readFileSync(join(FIXTURES, "cases.json"), "utf8")) as { request: string; cases: FixtureCase[] };
/** The shared CheckRequest without its page. */
const FIXTURE_REQUEST = JSON.parse(readFileSync(join(FIXTURES, FIXTURE_FILE.request), "utf8")) as Record<string, unknown>;
/**
 * frontend/sandbox/fixtures/expect.ts, loaded when the image tests start: the module imports the
 * frontend's packages, which the scripts' type check does not have.
 */
const EXPECT_MODULE = join(FIXTURES, "expect.ts");
type MatchesExpectation = (verdict: unknown, expectation: unknown) => boolean;
const USER = "65532:65532";
const CHROMIUM = "/usr/lib/chromium/chromium-headless-shell";
const CHECK_ROUTE = "/internal/dashboard-check/v1/check";
/** A cold build: the Rust release build, the frontend build and the Debian packages. */
const BUILD_TIMEOUT_MS = 60 * 60_000;
/** Waiting for the service's readiness after it starts. */
const START_TIMEOUT_MS = 2 * 60_000;
/**
 * One check: the service answers within its queue wait, CHECK_TIME_LIMIT and its cleanup
 * (services/dashboard-check/README.md); this bound only stops a test that hangs.
 */
const CHECK_TIMEOUT_MS = 2 * 60_000;
/** Waiting for the browser processes of the last check to be gone. */
const SWEEP_TIMEOUT_MS = 60_000;

/** The value of `ARG <name>=<value>` in the Containerfile. */
function containerfileArg(name: string): string {
  const match = new RegExp(`^ARG ${name}=(\\S+)$`, "m").exec(CONTAINERFILE);
  if (match?.[1] === undefined) {
    throw new Error(`the Containerfile has no ARG ${name}=<value>`);
  }
  return match[1];
}

/** The chart's Kubernetes quantity in mebibytes (`128Mi`), as the engine's size (`128m`). */
function mebibytes(quantity: string): string {
  const match = /^(\d+)Mi$/.exec(quantity);
  if (match?.[1] === undefined) {
    throw new Error(`expected a quantity in Mi, got ${quantity}`);
  }
  return `${match[1]}m`;
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

interface Result {
  exitCode: number;
  stdout: string;
  stderr: string;
}

async function exec(cmd: string[]): Promise<Result> {
  try {
    const proc = Bun.spawn(cmd, { stdin: "ignore", stdout: "pipe", stderr: "pipe" });
    const [stdout, stderr, exitCode] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
    return { exitCode, stdout, stderr };
  } catch (error) {
    return { exitCode: 127, stdout: "", stderr: String(error) };
  }
}

async function run(cmd: string[]): Promise<string> {
  const result = await exec(cmd);
  if (result.exitCode !== 0) {
    throw new Error(`${cmd.slice(0, 3).join(" ")} exited with ${result.exitCode}:\n${result.stderr}${result.stdout}`);
  }
  return result.stdout;
}

/** docker, or podman when docker is not installed. */
const ENGINE = Bun.which("docker") === null && Bun.which("podman") !== null ? "podman" : "docker";

/** Whether the engine runs linux/arm64 containers: its host is arm64, or binfmt_misc has an emulator. */
async function canRunArm64(): Promise<boolean> {
  const format = ENGINE === "podman" ? "{{.Host.Arch}}" : "{{.Architecture}}";
  const host = await exec([ENGINE, "info", "--format", format]);
  if (host.exitCode === 0 && ["arm64", "aarch64"].includes(host.stdout.trim())) {
    return true;
  }
  const handler = "/proc/sys/fs/binfmt_misc/qemu-aarch64";
  return existsSync(handler) && readFileSync(handler, "utf8").startsWith("enabled");
}

/** The engine's options that run a container as the chart runs the pod. */
function hardened(): string[] {
  return [
    "--platform", "linux/arm64",
    "--user", USER,
    "--read-only",
    "--tmpfs", `/tmp:rw,mode=1777,size=${mebibytes(CHART!.tmpSizeLimit)}`,
    "--cap-drop", "ALL",
    "--security-opt", "no-new-privileges",
    "--memory", mebibytes(CHART!.resources.limits.memory),
  ];
}

/** Memory use in MiB from the engine's `stats` output (`94.1MiB / 512MiB`): every reading. */
function memoryReadings(text: string): number[] {
  const unit: Record<string, number> = { B: 1 / 1024 / 1024, KiB: 1 / 1024, kB: 1 / 1024, MiB: 1, MB: 1, GiB: 1024, GB: 1024 };
  return [...text.matchAll(/([\d.]+)\s*(B|KiB|kB|MiB|MB|GiB|GB)\s*\//g)].map((match) => Number(match[1]) * (unit[match[2]!] ?? Number.NaN));
}

describe("Containerfile", () => {
  test("pins every base image by digest", () => {
    expect(baseImageProblems(CONTAINERFILE)).toEqual([]);
  });

  test("runs no program in a linux/arm64 stage, so release.yml builds it without an emulator", () => {
    expect(targetStageRunProblems(CONTAINERFILE)).toEqual([]);
    const last = stages(CONTAINERFILE).at(-1);
    // The image is the base and copies only: no RUN, no ADD (which could fetch from the network).
    expect(last?.instructions.map((line) => line.split(/\s/)[0]?.toUpperCase())).toEqual(["COPY", "COPY", "COPY", "USER", "ENTRYPOINT"]);
  });

  test("every RUN is plain sh that stops at the first error, because podman ignores SHELL for OCI images", () => {
    const instructions = stages(CONTAINERFILE).flatMap((stage) => stage.instructions);
    expect(instructions.filter((line) => /^SHELL\s/i.test(line))).toEqual([]);
    const runs = instructions.filter((line) => /^RUN\s/i.test(line));
    expect(runs.length).toBeGreaterThan(0);
    for (const line of runs) {
      expect(line).toMatch(/^RUN set -eu;/);
      expect(line).not.toMatch(/\[\[|\]\]|\$\{[A-Za-z_]+\/|pipefail/);
    }
  });

  test("takes every package from the pinned Debian snapshot over https, checked against Debian's archive keys", () => {
    expect(containerfileArg("DEBIAN_SNAPSHOT")).toMatch(/^\d{8}T\d{6}Z$/);
    expect(CONTAINERFILE).toContain("Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp");
    expect(CONTAINERFILE).toContain("URIs: https://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}");
    expect(CONTAINERFILE).toContain("URIs: https://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}");
    // The signatures prove the files are Debian's, https that they come from the snapshot
    // server itself: apt checks the server's certificate against the CA bundle of the pinned
    // base image, and nothing turns that check off or falls back to plain http.
    expect(CONTAINERFILE).not.toContain("http://");
    expect(CONTAINERFILE).toMatch(/^COPY --from=base \/etc\/ssl\/certs\/ca-certificates\.crt \/apt\/ca-certificates\.crt$/m);
    expect(CONTAINERFILE).toContain(`'Acquire::https::CaInfo "/apt/ca-certificates.crt";'`);
    expect(CONTAINERFILE).not.toMatch(/Verify-Peer|Verify-Host|Acquire::http::/i);
    // apt must never be told to accept what it cannot verify.
    expect(CONTAINERFILE).not.toMatch(/AllowUnauthenticated|allow-unauthenticated|trusted=yes|AllowInsecureRepositories|AllowDowngradeToInsecureRepositories/i);
  });

  test("Renovate tracks the Chromium version from Debian's security suite (renovate.json5)", () => {
    expect(CONTAINERFILE).toMatch(
      /^# renovate: datasource=deb depName=chromium-headless-shell registryUrl=https:\/\/deb\.debian\.org\/debian-security\?suite=trixie-security&components=main&binaryArch=arm64\nARG CHROMIUM_VERSION=\S+$/m,
    );
  });

  test("ends as the service's numeric user", () => {
    const last = stages(CONTAINERFILE).at(-1);
    expect(last?.instructions.filter((line) => /^USER\s/i.test(line))).toEqual([`USER ${USER}`]);
  });
});

const runnable = await canRunArm64();

describe("image", () => {
  const runningTest = runnable || process.env.CI ? test : test.skip;
  if (!runnable) {
    console.log(
      process.env.CI
        ? `${ENGINE} cannot run linux/arm64 containers here; with CI set the image tests fail`
        : `skipped: ${ENGINE} cannot run linux/arm64 containers here (needs an arm64 host or binfmt_misc qemu-aarch64)`,
    );
  }

  let image = process.env.DASHBOARD_CHECK_IMAGE ?? "";
  const container = `trustiot-dashboard-check-image-test-${process.pid}`;
  const containers: string[] = [];
  let files = "";
  let api = "";
  const timings: number[] = [];
  let sampler: ReturnType<typeof Bun.spawn> | undefined;
  let samples = "";
  let matchesExpectation: MatchesExpectation = () => false;

  beforeAll(async () => {
    if (!runnable) {
      return;
    }
    if (image === "") {
      image = "localhost/trustiot-e2e/dashboard-check:image-test";
      const tools = { ...(await findServiceImageTools(ROOT)), containerTool: ENGINE };
      const site = process.env.DASHBOARD_CHECK_SITE;
      await buildServiceImage({ repoRoot: ROOT, log: new RunLog() }, "dashboard-check", tools, undefined, {
        tag: image,
        ...(site === undefined || site === "" ? {} : { site: resolve(site) }),
      });
    }
    matchesExpectation = ((await import(EXPECT_MODULE)) as { matchesExpectation: MatchesExpectation }).matchesExpectation;
    // The package database the release SBOM is made from, copied out without running the image.
    files = mkdtempSync(join(tmpdir(), "trustiot-dashboard-check-image-"));
    const created = (await run([ENGINE, "create", "--platform", "linux/arm64", image])).trim();
    try {
      for (const name of ["chromium-headless-shell", "chromium-common"]) {
        await run([ENGINE, "cp", `${created}:/var/lib/dpkg/status.d/${name}`, join(files, name)]);
      }
    } finally {
      await exec([ENGINE, "rm", "--force", "--volumes", created]);
    }

    containers.push(container);
    await run([ENGINE, "run", "--detach", "--name", container, ...hardened(), "--publish", "127.0.0.1::8080", "--publish", "127.0.0.1::8081", image]);
    const address = async (port: number) => {
      const line = (await run([ENGINE, "port", container, `${port}/tcp`])).trim().split("\n")[0] ?? "";
      return `http://${line.replace(/^0\.0\.0\.0:/, "127.0.0.1:")}`;
    };
    api = await address(8080);
    const health = await address(8081);
    const deadline = Date.now() + START_TIMEOUT_MS;
    for (;;) {
      const ready = await fetch(`${health}/readyz`).then((response) => response.status === 200, () => false);
      if (ready) {
        break;
      }
      if (Date.now() > deadline) {
        const logs = await exec([ENGINE, "logs", container]);
        throw new Error(`the service did not become ready within ${START_TIMEOUT_MS / 1000} s:\n${logs.stdout}${logs.stderr}`);
      }
      await Bun.sleep(1000);
    }
    // The engine's memory readings while the checks run, for the measurement in the log.
    sampler = Bun.spawn([ENGINE, "stats", "--format", "{{.MemUsage}}", container], { stdin: "ignore", stdout: "pipe", stderr: "ignore" });
    void (async () => {
      const decoder = new TextDecoder();
      for await (const chunk of sampler.stdout as ReadableStream<Uint8Array>) {
        samples += decoder.decode(chunk);
      }
    })();
  }, BUILD_TIMEOUT_MS);

  afterAll(async () => {
    sampler?.kill();
    if (timings.length > 0) {
      const sorted = [...timings].sort((a, b) => a - b);
      const memory = memoryReadings(samples);
      console.log(
        `checks: ${timings.length}, time per check ${sorted[0]}–${sorted.at(-1)} ms (median ${sorted[Math.floor(sorted.length / 2)]} ms); ` +
          `highest memory reading of the container ${memory.length === 0 ? "none" : `${Math.max(...memory).toFixed(1)} MiB`} of ${CHART.resources.limits.memory}`,
      );
    }
    for (const name of containers) {
      // podman waits for its stop timeout unless told otherwise; docker has no such option.
      const removed = await exec([ENGINE, "rm", "--force", "--volumes", ...(ENGINE === "podman" ? ["--time", "0"] : []), name]);
      if (removed.exitCode !== 0 && !/no such container/i.test(removed.stderr)) {
        console.error(`could not remove the container ${name}: ${removed.stderr.trim()}`);
      }
    }
    if (files !== "") {
      rmSync(files, { recursive: true, force: true });
    }
  });

  runningTest("is linux/arm64 and runs the service as UID 65532", async () => {
    const [inspected] = JSON.parse(await run([ENGINE, "image", "inspect", image])) as {
      Architecture: string;
      Os: string;
      Config: { User?: string; Entrypoint?: string[] };
    }[];
    expect(inspected?.Os).toBe("linux");
    expect(inspected?.Architecture).toBe("arm64");
    expect(inspected?.Config.User).toBe(USER);
    expect(inspected?.Config.Entrypoint).toEqual(["/usr/local/bin/dashboard-check"]);
  });

  runningTest("lists the pinned Chromium in its package database, where the release SBOM finds it", () => {
    const version = containerfileArg("CHROMIUM_VERSION");
    for (const name of ["chromium-headless-shell", "chromium-common"]) {
      const control = readFileSync(join(files, name), "utf8");
      expect(control).toContain(`Package: ${name}\n`);
      expect(control).toContain(`Version: ${version}\n`);
      expect(control).toContain("Architecture: arm64\n");
    }
  });

  test("the fixtures hold the pages only a browser can judge", () => {
    expect(FIXTURE_FILE.cases.length).toBeGreaterThan(0);
    expect(FIXTURE_FILE.cases.filter((fixture) => !fixture.unit).length).toBeGreaterThan(0);
    expect(new Set(FIXTURE_FILE.cases.map((fixture) => fixture.name)).size).toBe(FIXTURE_FILE.cases.length);
  });

  for (const fixture of FIXTURE_FILE.cases) {
    runningTest(
      `${fixture.name}${fixture.unit ? "" : " (browser only)"}: ${JSON.stringify(fixture.expect).slice(0, 120)}`,
      async () => {
        const started = performance.now();
        const response = await fetch(`${api}${CHECK_ROUTE}`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ ...FIXTURE_REQUEST, page: readFileSync(join(FIXTURES, fixture.page), "utf8") }),
        });
        const text = await response.text();
        timings.push(Math.round(performance.now() - started));
        expect({ status: response.status, body: text.slice(0, 500) }).toMatchObject({ status: 200 });
        const verdict = JSON.parse(text) as unknown;
        if (!matchesExpectation(verdict, fixture.expect)) {
          throw new Error(`${fixture.name}: the verdict ${text.slice(0, 2000)} does not meet ${JSON.stringify(fixture.expect)} (${fixture.about})`);
        }
        // An address never reaches the verdict (a page could spell data into one).
        expect(text).not.toContain("check.invalid");
        expect(text).not.toContain("/sandbox/");
      },
      CHECK_TIMEOUT_MS,
    );
  }

  runningTest(
    "leaves no browser process behind after the checks",
    async () => {
      const deadline = Date.now() + SWEEP_TIMEOUT_MS;
      let processes: string[] = [];
      for (;;) {
        // The lines of `top` after its header: one per process of the container.
        processes = (await run([ENGINE, "top", container])).trim().split("\n").slice(1);
        if (processes.length === 1) {
          break;
        }
        if (Date.now() > deadline) {
          throw new Error(`processes still running ${SWEEP_TIMEOUT_MS / 1000} s after the last check:\n${processes.join("\n")}`);
        }
        await Bun.sleep(1000);
      }
      expect(processes[0]).toContain("dashboard-check");
    },
    SWEEP_TIMEOUT_MS + 30_000,
  );

  runningTest(
    "Chromium refuses to start without --no-sandbox under the engine's default seccomp profile",
    async () => {
      // Why the service starts Chromium with --no-sandbox (services/dashboard-check/README.md,
      // 「威脅模型」): Chromium's own sandbox needs namespaces or a set-user-ID helper, which the
      // pod's settings take away.
      const name = `${container}-sandbox`;
      containers.push(name);
      const result = await exec([ENGINE, "run", "--name", name, ...hardened(), "--entrypoint", CHROMIUM, image, "--headless", "--dump-dom", "about:blank"]);
      expect(result.exitCode).not.toBe(0);
      expect(result.stderr).toContain("No usable sandbox!");
    },
    START_TIMEOUT_MS,
  );
});
