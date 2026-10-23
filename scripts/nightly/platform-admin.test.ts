import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import { pinnedTool } from "./exec.ts";
import { migrationJobsOf, parseManifests } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import {
  ALREADY_CREATED_MESSAGE,
  BOOTSTRAP_CONTAINER,
  bootstrapCronJobOf,
  bootstrapExitCode,
  CREATED_MESSAGE,
  EXIT_ALREADY_CREATED,
  EXIT_CREATED,
  exampleAddress,
  logMessages,
  runningBootstrapPod,
  TASK,
  TASK_LABEL,
  userIdOfLog,
} from "./platform-admin.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const RELEASE = "trustiot";
const USER = "2c1d0e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
const source = (...path: string[]) => readFileSync(join(REPO_ROOT, ...path), "utf8");

describe("the names the check reads are the ones the code writes", () => {
  test("the exit codes and log messages of migrate bootstrap-platform-admin", () => {
    const main = source("tools", "migrate", "src", "main.rs");
    const module = source("tools", "migrate", "src", "platform_admin.rs");
    expect(module).toContain(`pub const EXIT_ALREADY_CREATED: u8 = ${EXIT_ALREADY_CREATED};`);
    expect(EXIT_CREATED).toBe(0);
    expect(main).toContain(`"${CREATED_MESSAGE}"`);
    // The message spans two source lines joined by a line continuation.
    const [head, tail] = ALREADY_CREATED_MESSAGE.split(" (");
    expect(main).toContain(`"${head} \\`);
    expect(main).toContain(`(${tail}"`);
  });

  test("the CronJob's label, container and command", () => {
    const template = source("deploy", "chart", "templates", "platform-admin-bootstrap.yaml");
    expect(template).toContain(`"${TASK_LABEL}" "${TASK}"`);
    expect(template).toContain(`- name: ${BOOTSTRAP_CONTAINER}`);
  });
});

describe("the bootstrap CronJob (Laysi/trustiot-v2#582)", () => {
  for (const shape of discoverShapes(CHART_DIR)) {
    test(`values-${shape.name}.yaml: suspended, as the migration Job's account, stdin once, the migration role only`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const render = (...extra: string[]) => {
        const cmd = [helm, "template", RELEASE, CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), ...extra];
        const rendered = Bun.spawnSync({ cmd, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
        throwIfTimedOut(cmd, rendered);
        expect(rendered.exitCode, rendered.stderr.toString()).toBe(0);
        return parseManifests(rendered.stdout.toString());
      };
      const objects = render("--set", "services.identity.enabled=true");
      const found = bootstrapCronJobOf(objects);
      expect(found).toEqual({ cronJob: { name: `${RELEASE}-platform-admin-bootstrap`, serviceAccount: `${RELEASE}-migrate` }, problems: [] });
      // The migration Job's own account, which the Postgres NetworkPolicy and the mesh admit.
      const job = objects.find((o) => o.kind === "Job" && o.metadata?.name === migrationJobsOf(objects)[0]);
      expect((job?.spec?.template as { spec: { serviceAccountName: string } }).spec.serviceAccountName).toBe(`${RELEASE}-migrate`);
      // Without the identity service there is no CronJob, and the check says so.
      expect(bootstrapCronJobOf(render()).problems).toEqual(["the release should have one platform administrator bootstrap CronJob, found 0"]);
    }, SLOW_TEST_MS);
  }

  const cronJob = (spec: Record<string, unknown>, pod: Record<string, unknown>): K8sObject => ({
    kind: "CronJob",
    metadata: { name: "trustiot-platform-admin-bootstrap", labels: { [TASK_LABEL]: TASK } },
    spec: { ...spec, jobTemplate: { spec: { template: { spec: pod } } } },
  });
  const container = {
    name: BOOTSTRAP_CONTAINER,
    args: ["bootstrap-platform-admin"],
    stdin: true,
    stdinOnce: true,
    env: [{ name: "TRUSTIOT__POSTGRES__USERNAME" }, { name: "TRUSTIOT__POSTGRES__PASSWORD" }, { name: "TRUSTIOT__WAIT__DATABASES" }],
  };
  const good = { serviceAccountName: "trustiot-migrate", automountServiceAccountToken: false, containers: [container] };

  test("a CronJob that strays from the runbook is reported", () => {
    expect(bootstrapCronJobOf([cronJob({ suspend: true }, good)]).problems).toEqual([]);
    const cases: [Record<string, unknown>, Record<string, unknown>, string][] = [
      [{}, good, "is not suspended"],
      [{ suspend: true }, { ...good, automountServiceAccountToken: undefined }, "mounts the default token"],
      [{ suspend: true }, { ...good, initContainers: [{ name: "x" }] }, "has more than its one container"],
      [{ suspend: true }, { ...good, containers: [{ ...container, args: ["bootstrap-platform-admin", "admin@example.com"] }] }, "does not run only bootstrap-platform-admin"],
      [{ suspend: true }, { ...good, containers: [{ ...container, stdinOnce: false }] }, "does not read stdin once"],
      [{ suspend: true }, { ...good, containers: [{ ...container, env: [...container.env, { name: "TRUSTIOT__ADMIN__EMAIL" }] }] }, "beyond the migration role's: TRUSTIOT__ADMIN__EMAIL"],
    ];
    for (const [spec, pod, problem] of cases) {
      expect(bootstrapCronJobOf([cronJob(spec, pod)]).problems.join("; ")).toContain(problem);
    }
  });
});

describe("reading the run", () => {
  const pods = (state: Record<string, unknown>, name = BOOTSTRAP_CONTAINER) => ({
    items: [{ metadata: { name: "platform-admin-bootstrap-1a2b3c4d-xyz12" }, status: { containerStatuses: [{ name, state }] } }],
  });

  test("the pod to attach to, and the container's exit code once it ended", () => {
    expect(runningBootstrapPod({ items: [] })).toBeUndefined();
    expect(runningBootstrapPod(pods({ waiting: { reason: "ContainerCreating" } }))).toBeUndefined();
    expect(runningBootstrapPod(pods({ running: { startedAt: "2026-10-23T00:00:00Z" } }))).toBe("platform-admin-bootstrap-1a2b3c4d-xyz12");
    expect(runningBootstrapPod(pods({ running: {} }, "other"))).toBeUndefined();
    expect(bootstrapExitCode(pods({ running: {} }))).toBeUndefined();
    expect(bootstrapExitCode(pods({ terminated: { exitCode: 0 } }))).toBe(0);
    expect(bootstrapExitCode(pods({ terminated: { exitCode: 3 } }))).toBe(3);
  });

  test("the user ID comes from the log line with the message, nested under fields", () => {
    const line = (message: string, fields: Record<string, unknown>) => JSON.stringify({ timestamp: "2026-10-23T00:00:00Z", level: "INFO", message, fields });
    const logs = [
      line("reading the first platform administrator's email address from stdin", {}),
      "not JSON",
      line(CREATED_MESSAGE, { user_id: USER, existing_user: false }),
    ].join("\n");
    expect(userIdOfLog(logs, CREATED_MESSAGE)).toBe(USER);
    expect(userIdOfLog(logs, ALREADY_CREATED_MESSAGE)).toBeUndefined();
    expect(userIdOfLog(line(CREATED_MESSAGE, { user_id: "not-a-uuid" }), CREATED_MESSAGE)).toBeUndefined();
    expect(userIdOfLog(line(ALREADY_CREATED_MESSAGE, { user_id: USER, created_at: "2026-10-23T00:00:00Z" }), ALREADY_CREATED_MESSAGE)).toBe(USER);
  });

  test("a failure names the fixed messages only, never a field value or a line that is not JSON", () => {
    const logs = [
      JSON.stringify({ level: "INFO", message: "reading the first platform administrator's email address from stdin" }),
      JSON.stringify({ level: "ERROR", message: "the first platform administrator was not created", fields: { error: "SECRET-FIELD-VALUE" } }),
      "SECRET-PLAIN-LINE",
      JSON.stringify({ level: "WARN", note: "no message" }),
      "",
    ].join("\n");
    const messages = logMessages(logs);
    expect(messages).toEqual([
      "INFO reading the first platform administrator's email address from stdin",
      "ERROR the first platform administrator was not created",
      "(2 other lines)",
    ]);
    expect(JSON.stringify(messages)).not.toContain("SECRET");
    expect(logMessages("")).toEqual([]);
  });

  test("every address is a fresh one under example.com", () => {
    const first = exampleAddress();
    expect(first).toMatch(/^nightly-admin-[0-9a-f]{8}@example\.com$/);
    expect(exampleAddress()).not.toBe(first);
  });
});
