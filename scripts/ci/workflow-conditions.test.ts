// The conditions in ci.yml and nightly.yml that decide what runs (Laysi/trustiot-v2#309),
// evaluated with GitHub's own expression engine (@actions/expressions) against the event
// payloads GitHub sends, so a typo in an `if` or a concurrency group fails here and not on the
// first pull request it should have stopped. Also the runner of every job in these workflows
// and in release.yml (Laysi/trustiot-v2#310).
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { Evaluator, Lexer, Parser, data } from "@actions/expressions";
import { truthy } from "@actions/expressions/result";

import { PATH_JOBS } from "./changes.ts";
import { pullRequestTriggerProblem } from "./tested-tree.ts";

const ROOT = join(import.meta.dir, "..", "..");

type YamlMap = Record<string, unknown>;

function workflow(name: string): YamlMap {
  return Bun.YAML.parse(readFileSync(join(ROOT, ".github", "workflows", name), "utf8")) as YamlMap;
}

function jobsOf(doc: YamlMap): Record<string, YamlMap> {
  return doc.jobs as Record<string, YamlMap>;
}

/** The status of the jobs a job needs, as the status functions see it. */
type Status = "success" | "failure" | "cancelled";
const STATUS_FUNCTIONS = ["always", "success", "failure", "cancelled"] as const;

/** The expression inside `${{ }}`; a job `if` may also be written without the delimiters. */
function expressionOf(value: unknown): string {
  if (typeof value !== "string") {
    throw new Error(`expected an expression, got ${JSON.stringify(value)}`);
  }
  const match = /^\s*\$\{\{([\s\S]*)\}\}\s*$/.exec(value);
  return (match?.[1] ?? value).trim();
}

/** Evaluates `expression` the way GitHub does for the given contexts and job status. */
function evaluate(expression: string, contexts: YamlMap, status: Status = "success"): data.ExpressionData {
  const functions = STATUS_FUNCTIONS.map((name) => ({ name, minArgs: 0, maxArgs: 0 }));
  const ast = new Parser(new Lexer(expression).lex().tokens, Object.keys(contexts), functions).parse();
  const implementations = new Map(
    functions.map((info) => [
      info.name,
      { ...info, call: () => new data.BooleanData(info.name === "always" || info.name === status) },
    ]),
  );
  const context = JSON.parse(JSON.stringify(contexts), data.reviver) as data.Dictionary;
  return new Evaluator(ast, context, implementations).evaluate();
}

/**
 * Whether a job with this `if` runs. Without a status function GitHub prepends `success() &&`
 * (https://docs.github.com/actions/reference/workflows-and-actions/expressions#status-check-functions).
 */
function jobRuns(condition: unknown, contexts: YamlMap, status: Status = "success"): boolean {
  if (condition === undefined) {
    return status === "success";
  }
  const expression = expressionOf(condition);
  const implicitSuccess = !STATUS_FUNCTIONS.some((name) => new RegExp(`\\b${name}\\(`).test(expression));
  return (!implicitSuccess || status === "success") && truthy(evaluate(expression, contexts, status));
}

describe("ci.yml", () => {
  const ci = workflow("ci.yml");
  const jobs = jobsOf(ci);

  test("every job runs on the 2-core arm64 runner", () => {
    for (const [id, job] of Object.entries(jobs)) {
      expect(`${id}: ${String(job["runs-on"])}`).toBe(`${id}: ubuntu-24.04-arm`);
    }
  });

  test("the changes job runs the decision script with the full history and passes each answer on", () => {
    const changes = jobs.changes as YamlMap;
    expect(changes.needs).toBeUndefined();
    expect(changes.if).toBeUndefined();
    // Read-only scopes for the API reads of changes.ts and tested-tree.ts.
    expect(changes.permissions).toEqual({ contents: "read", actions: "read", "pull-requests": "read", issues: "read" });
    const steps = changes.steps as YamlMap[];
    const checkout = steps.find((step) => String(step.uses).startsWith("actions/checkout@"));
    expect((checkout?.with as YamlMap)["fetch-depth"]).toBe(0);
    const decide = steps.find((step) => step.id === "decide");
    expect(decide?.run).toBe("bun scripts/ci/changes.ts");
    // Only a push, which runs main's reviewed copy of the script, gets the token.
    const marker = crypto.randomUUID();
    const passed = (event_name: string) =>
      evaluate(expressionOf((decide?.env as YamlMap).GITHUB_TOKEN), { github: { event_name, token: marker } }).coerceString();
    expect(passed("push")).toBe(marker);
    expect(passed("pull_request")).toBe("");
    expect(changes.outputs).toEqual(
      Object.fromEntries(PATH_JOBS.map((job) => [job, `\${{ steps.decide.outputs.${job} }}`])),
    );
  });

  test("exactly the path-dependent jobs wait for changes; gitleaks and workflow-policy always run", () => {
    const waiting = Object.entries(jobs)
      .filter(([, job]) => job.needs !== undefined || job.if !== undefined)
      .map(([id]) => id)
      .toSorted();
    expect(waiting).toEqual([...PATH_JOBS].toSorted());
    for (const id of ["gitleaks", "workflow-policy"]) {
      expect(jobs[id]).toBeDefined();
      expect(jobRuns(jobs[id]?.if, {})).toBe(true);
    }
  });

  describe.each([...PATH_JOBS])("%s", (job) => {
    const condition = jobs[job]?.if;
    const answer = (value: string | undefined, status: Status = "success") =>
      jobRuns(
        condition,
        { needs: { changes: { result: status, outputs: value === undefined ? {} : { [job]: value } } } },
        status,
      );

    test("needs the changes job", () => {
      expect(jobs[job]?.needs).toBe("changes");
    });

    test("runs when changes says true and is skipped when it says false", () => {
      expect(answer("true")).toBe(true);
      expect(answer("false")).toBe(false);
    });

    test("runs when changes failed or gave no answer", () => {
      expect(answer(undefined, "failure")).toBe(true);
      expect(answer("", "failure")).toBe(true);
      expect(answer(undefined)).toBe(true);
    });

    test("does not run when the workflow was cancelled", () => {
      expect(answer("true", "cancelled")).toBe(false);
    });
  });

  // A push to main skips a path-dependent job that passed in the merged pull request's run on the
  // same tree (scripts/ci/tested-tree.ts, Laysi/trustiot-v2#431). That is only sound while the job
  // does the same on a pull request as on a push, and while it can be found by name in the run.
  test("every pull request event runs ci.yml: its pull_request trigger has no filter", () => {
    expect(pullRequestTriggerProblem(readFileSync(join(ROOT, ".github", "workflows", "ci.yml"), "utf8"))).toBeUndefined();
  });

  test("each path-dependent job is named after its id, and no other job shares the name", () => {
    for (const job of PATH_JOBS) {
      expect(`${job}: ${String(jobs[job]?.name)}`).toBe(`${job}: ${job}`);
    }
    const names = Object.entries(jobs).map(([id, job]) => String(job.name ?? id));
    expect(names.toSorted()).toEqual([...new Set(names)].toSorted());
  });

  /**
   * References to what differs between a pull request run and a push run of the same tree: the
   * event, the ref, the commit, the workflow ref and the actor, as contexts (also by index and
   * through toJSON(github)) or as environment variables. A job can still read them in ways no
   * pattern finds (a computed variable name), and it can look at HEAD's parents or history, which
   * differ (a pull request run tests a two-parent merge commit): path-dependent jobs must not do
   * either, which review has to catch.
   */
  const EVENT_DEPENDENT =
    /github\s*\[|toJSON\(\s*github\s*\)|github\.(?:event\w*|ref\w*|head_ref|base_ref|sha|workflow_ref|workflow_sha|actor\w*|triggering_actor)\b|GITHUB_(?:EVENT_NAME|EVENT_PATH|REF\w*|HEAD_REF|BASE_REF|SHA|WORKFLOW_REF|WORKFLOW_SHA|ACTOR\w*|TRIGGERING_ACTOR)\b/g;

  test("the event pattern finds every spelling it is meant to", () => {
    const spellings = [
      "${{ github.event_name }}",
      "${{ github['event_name'] }}",
      "${{ github.event_path }}",
      "${{ github.event.pull_request.number }}",
      "${{ toJSON(github) }}",
      "${{ github.ref_name }}",
      "${{ github.head_ref }}",
      "${{ github.sha }}",
      "${{ github.workflow_ref }}",
      "${{ github.actor }}",
      "${{ github.triggering_actor }}",
      "$GITHUB_EVENT_NAME",
      "$GITHUB_REF",
      "process.env.GITHUB_SHA",
      "$GITHUB_WORKFLOW_REF",
      "$GITHUB_ACTOR",
      "$GITHUB_TRIGGERING_ACTOR",
    ];
    expect(spellings.filter((spelling) => spelling.match(EVENT_DEPENDENT) === null)).toEqual([]);
    for (const harmless of ["${{ github.workspace }}", "${{ github.token }}", "$GITHUB_OUTPUT", "$GITHUB_REPOSITORY", "${{ github.run_id }}"]) {
      expect(harmless.match(EVENT_DEPENDENT)).toBeNull();
    }
  });
  /** The one allowed difference: only main saves the Rust build cache, which a run never fails on. */
  const RUST_CACHE_SAVE_IF = "${{ github.ref == 'refs/heads/main' }}";

  test("the path-dependent jobs do not depend on the event, apart from saving the Rust cache", () => {
    for (const job of PATH_JOBS) {
      const definition = JSON.stringify(jobs[job]).replaceAll(JSON.stringify(RUST_CACHE_SAVE_IF), '""');
      expect(`${job}: ${definition.match(EVENT_DEPENDENT) ?? []}`).toBe(`${job}: `);
    }
    const rustCache = ((jobs.rust?.steps ?? []) as YamlMap[]).find((step) => String(step.uses).startsWith("Swatinem/rust-cache@"));
    expect((rustCache?.with as YamlMap)["save-if"]).toBe(RUST_CACHE_SAVE_IF);
  });

  // changes.ts lets `rust` run on main whenever a merge changes the cache key (affectsRustCacheKey),
  // assuming rust-cache's default key. An input that changes the key (key, shared-key, prefix-key,
  // workspaces, env-vars, add-job-id-key, add-rust-environment-hash-key, ...) needs that rule
  // revisited first. The version is not pinned here: the rule only affects CI cost, so a rust-cache
  // upgrade must not fail CI; whoever upgrades it re-checks "Cache Details" in its README and that
  // it still skips saving an unchanged key (docs/architecture/ci-release.md).
  test("the Rust cache step keeps the default key that changes.ts assumes", () => {
    const steps = (jobs.rust?.steps ?? []) as YamlMap[];
    const caches = steps.filter((step) => String(step.uses).startsWith("Swatinem/rust-cache@"));
    expect(caches.length).toBe(1);
    expect(Object.keys((caches[0]?.with ?? {}) as YamlMap).toSorted()).toEqual(["cache-bin", "save-if"]);
  });

  // The key also holds rustc's version and hash. rustup installs the toolchain rust-toolchain.toml
  // names, so they change only with that file (a key input) while its channel is an exact version;
  // a floating channel such as `stable` would change the key with no file changing.
  test("the Rust toolchain is an exact version, so rustc changes only with rust-toolchain.toml", () => {
    const toolchain = Bun.TOML.parse(readFileSync(join(ROOT, "rust-toolchain.toml"), "utf8")) as { toolchain?: { channel?: unknown } };
    expect(String(toolchain.toolchain?.channel)).toMatch(/^\d+\.\d+\.\d+$/);
  });

  test("the code the path-dependent jobs run does not read the event either", () => {
    const listed = Bun.spawnSync(["git", "ls-files", "-z"], { cwd: ROOT, stdout: "pipe", stderr: "pipe" });
    expect(listed.exitCode).toBe(0);
    // What the jobs build, run or import: the Rust workspace, deploy/ (chart, flux, host installer
    // with its package.json scripts, images), the single-page app (frontend/, with its package.json
    // scripts and vite.config.ts), the scripts the chart, flux and image tests import (the dashboard
    // check's image test builds with the nightly's and the release's build scripts;
    // scripts/ci/changes.test.ts checks those imports against the path rules), and the local
    // actions.
    const prefixes = [".github/actions/", "crates/", "services/", "tools/", "edge/", "factory/", "deploy/", "frontend/", "scripts/lib/", "scripts/flux/"];
    const files = new Set([
      "scripts/release/containerfile.ts",
      "scripts/release/exec.ts",
      "scripts/release/flux.ts",
      "scripts/release/lib.ts",
      "scripts/release/rust-build.ts",
      "scripts/release/rust-context.ts",
      "scripts/release/static-build.ts",
      "scripts/nightly/exec.ts",
      "scripts/nightly/image-cache.ts",
      "scripts/nightly/platform.ts",
      "scripts/nightly/service-image.ts",
      "scripts/ci/check-secret-handling.ts",
    ]);
    const code = /\.(?:rs|tsx?|js|mjs|cjs|json|sh|ya?ml|toml|tpl|sql)$|(?:^|\/)(?:Containerfile|Dockerfile)[^/]*$/;
    const scanned = listed.stdout
      .toString()
      .split("\0")
      .filter((path) => (files.has(path) || prefixes.some((prefix) => path.startsWith(prefix))) && code.test(path));
    for (const expected of [
      "deploy/host/package.json",
      "deploy/images/postgres/Containerfile",
      "frontend/package.json",
      "frontend/vite.config.ts",
      "frontend/src/main.tsx",
      ...files,
    ]) {
      expect(scanned).toContain(expected);
    }
    // check-secret-handling.ts names `github.sha` in a message as a value it accepts in build
    // arguments; it never reads the event.
    const allowed: Record<string, string[]> = { "scripts/ci/check-secret-handling.ts": ["github.sha"] };
    const offending = scanned
      .map((path) => {
        const source = readFileSync(join(ROOT, path), "utf8");
        // Comments explain; only code counts (JavaScript and TypeScript comments).
        const code = /\.[cm]?[jt]sx?$/.test(path) ? source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "") : source;
        return { path, found: [...(code.match(EVENT_DEPENDENT) ?? [])] };
      })
      .filter(({ path, found }) => found.some((match) => !(allowed[path] ?? []).includes(match)));
    expect(offending).toEqual([]);
  });

  test("a pull request's new push still cancels its running CI; main runs never cancel", () => {
    const concurrency = ci.concurrency as YamlMap;
    const cancel = (event_name: string) =>
      truthy(evaluate(expressionOf(concurrency["cancel-in-progress"]), { github: { event_name } }));
    expect(cancel("pull_request")).toBe(true);
    expect(cancel("push")).toBe(false);
  });
});

describe("nightly.yml", () => {
  const nightly = workflow("nightly.yml");
  const jobs = jobsOf(nightly);
  const on = nightly.on as YamlMap;
  const concurrency = nightly.concurrency as YamlMap;

  /** The `github` context of a pull request event. */
  function pullRequest(action: string, labels: string[], added?: string): YamlMap {
    return {
      event_name: "pull_request",
      ref: "refs/pull/42/merge",
      run_id: 1000 + labels.length + action.length,
      event: {
        action,
        ...(added === undefined ? {} : { label: { name: added } }),
        pull_request: { number: 42, labels: labels.map((name) => ({ name })) },
      },
    };
  }
  const schedule: YamlMap = { event_name: "schedule", ref: "refs/heads/main", run_id: 7, event: {} };
  const dispatch: YamlMap = { event_name: "workflow_dispatch", ref: "refs/heads/main", run_id: 8, event: {} };

  /** What the `decide` job leaves for the others: `deploy` is 'true' or 'false', or empty when the job did not run or failed. */
  type DecideState = { result: "success" | "skipped" | "failure" | "cancelled"; deploy?: string };
  const needsOf = (decide: DecideState) => ({
    decide: { result: decide.result, outputs: decide.deploy === undefined ? {} : { deploy: decide.deploy } },
  });
  /** Not a scheduled run: `decide` is skipped and leaves no output. */
  const NOT_ASKED: DecideState = { result: "skipped" };
  const smokeRuns = (github: YamlMap, decide: DecideState = NOT_ASKED, status: Status = "success") =>
    jobRuns(jobs.smoke?.if, { github, needs: needsOf(decide) }, status);
  const group = (github: YamlMap) => evaluate(expressionOf(concurrency.group), { github }).coerceString();
  const cancels = (github: YamlMap) => truthy(evaluate(expressionOf(concurrency["cancel-in-progress"]), { github }));

  test("schedule and workflow_dispatch are unchanged and always run the smoke test", () => {
    expect((on.schedule as YamlMap[])[0]?.cron).toBe("17 18 * * *");
    expect(on.workflow_dispatch).toBeDefined();
    expect(smokeRuns(schedule)).toBe(true);
    expect(smokeRuns(dispatch)).toBe(true);
  });

  test("pull requests trigger on the chart paths, including when a label is added", () => {
    const trigger = on.pull_request as YamlMap;
    expect(trigger.types).toEqual(["opened", "synchronize", "reopened", "labeled"]);
    expect(trigger.paths).toContain("deploy/chart/**");
    expect(trigger.paths).toContain(".github/workflows/nightly.yml");
  });

  test("a pull request without the smoke label deploys nothing", () => {
    for (const action of ["opened", "synchronize", "reopened"]) {
      expect(smokeRuns(pullRequest(action, []))).toBe(false);
      expect(smokeRuns(pullRequest(action, ["review:ai", "area:ci"]))).toBe(false);
    }
    expect(smokeRuns(pullRequest("labeled", ["review:ai"], "review:ai"))).toBe(false);
  });

  test("adding the smoke label deploys, and so does every later push", () => {
    expect(smokeRuns(pullRequest("labeled", ["smoke"], "smoke"))).toBe(true);
    expect(smokeRuns(pullRequest("labeled", ["review:ai", "Smoke"], "Smoke"))).toBe(true);
    for (const action of ["opened", "synchronize", "reopened"]) {
      expect(smokeRuns(pullRequest(action, ["review:ai", "smoke"]))).toBe(true);
    }
  });

  test("adding another label to a smoke pull request neither deploys nor cancels the deployment", () => {
    const deploying = pullRequest("synchronize", ["smoke"]);
    const other = pullRequest("labeled", ["smoke", "review:ai"], "review:ai");
    expect(smokeRuns(other)).toBe(false);
    expect(group(other)).not.toBe(group(deploying));
    expect(group(other)).toStartWith("nightly-no-smoke-");
  });

  test("deploying runs of one pull request share a group, so a newer push replaces the older run", () => {
    const pushed = pullRequest("synchronize", ["smoke"]);
    const labelled = pullRequest("labeled", ["smoke"], "smoke");
    expect(group(pushed)).toBe("nightly-42");
    expect(group(labelled)).toBe("nightly-42");
    expect(cancels(pushed)).toBe(true);
  });

  test("scheduled and manual runs keep their group and are never cancelled", () => {
    expect(group(schedule)).toBe("nightly-refs/heads/main");
    expect(group(dispatch)).toBe("nightly-refs/heads/main");
    expect(cancels(schedule)).toBe(false);
    expect(cancels(dispatch)).toBe(false);
  });

  test("notify never runs for a pull request", () => {
    const github = { ...pullRequest("synchronize", ["smoke"]), ref_name: "42/merge", event: { repository: { default_branch: "main" } } };
    expect(jobRuns(jobs.notify?.if, { github, needs: { ...needsOf(NOT_ASKED), smoke: { result: "success" } } })).toBe(false);
  });

  describe("skipping a night when main is unchanged (Laysi/trustiot-v2#433)", () => {
    const decideJob = jobs.decide as YamlMap;
    const asked = (deploy: string | undefined, result: DecideState["result"] = "success"): DecideState => ({
      result,
      ...(deploy === undefined ? {} : { deploy }),
    });
    const dispatchRef = { ...dispatch, ref_name: "main", event: { repository: { default_branch: "main" } } };
    const scheduleRef = { ...schedule, ref_name: "main", event: { repository: { default_branch: "main" } } };
    const notifyRuns = (github: YamlMap, decide: DecideState, smoke: "success" | "failure" | "cancelled" | "skipped") =>
      jobRuns(jobs.notify?.if, { github, needs: { ...needsOf(decide), smoke: { result: smoke } } }, smoke === "success" ? "success" : "failure");

    test("the decide job runs for scheduled runs only, so a pull request never runs its code and a manual run starts no runner for it", () => {
      const runs = (github: YamlMap) => jobRuns(decideJob.if, { github });
      expect(runs(schedule)).toBe(true);
      expect(runs(dispatch)).toBe(false);
      expect(runs(pullRequest("synchronize", ["smoke"]))).toBe(false);
      expect(runs(pullRequest("labeled", ["smoke"], "smoke"))).toBe(false);
      expect(decideJob.needs).toBeUndefined();
    });

    test("the decide job reads runs and commits and writes nothing", () => {
      expect(decideJob.permissions).toEqual({ contents: "read", actions: "read" });
      expect(decideJob["runs-on"]).toBe("ubuntu-24.04-arm");
      const steps = decideJob.steps as YamlMap[];
      const unchanged = steps.find((step) => step.id === "unchanged");
      expect(unchanged?.run).toBe("bun scripts/nightly/unchanged.ts");
      expect(unchanged?.env).toEqual({ GITHUB_TOKEN: "${{ github.token }}" });
      expect(decideJob.outputs).toEqual({ deploy: "${{ steps.unchanged.outputs.deploy }}" });
      // Nothing but the checkout of the default branch and bun runs before the script.
      expect(steps.map((step) => String(step.uses ?? step.run))).toEqual([
        expect.stringMatching(/^actions\/checkout@[0-9a-f]{40}$/),
        "./.github/actions/setup-proto",
        "bun scripts/nightly/unchanged.ts",
      ]);
    });

    test("smoke waits for decide", () => {
      expect(jobs.smoke?.needs).toBe("decide");
      expect(jobs.notify?.needs).toEqual(["decide", "smoke"]);
    });

    test("a scheduled run whose decide said 'false' deploys nothing", () => {
      expect(smokeRuns(schedule, asked("false"))).toBe(false);
    });

    test("a scheduled run deploys when decide said 'true', gave no answer or failed", () => {
      expect(smokeRuns(schedule, asked("true"))).toBe(true);
      expect(smokeRuns(schedule, asked(undefined))).toBe(true);
      expect(smokeRuns(schedule, asked("", "failure"))).toBe(true);
      expect(smokeRuns(schedule, asked(undefined, "failure"))).toBe(true);
    });

    test("a manual run always deploys: decide is skipped for it and leaves no output", () => {
      expect(smokeRuns(dispatch)).toBe(true);
      expect(smokeRuns({ ...dispatch, ref: "refs/heads/feature" })).toBe(true);
    });

    test("a cancelled workflow starts no deployment", () => {
      expect(smokeRuns(schedule, asked("true", "cancelled"), "cancelled")).toBe(false);
    });

    test("the pull request gate is unchanged: the smoke label decides, decide is never asked", () => {
      expect(smokeRuns(pullRequest("synchronize", []))).toBe(false);
      expect(smokeRuns(pullRequest("synchronize", ["smoke"]))).toBe(true);
      expect(smokeRuns(pullRequest("labeled", ["review:ai"], "review:ai"))).toBe(false);
    });

    test("notify does not run on a skipped night, so the failure issue is neither opened nor closed", () => {
      expect(notifyRuns(scheduleRef, asked("false"), "skipped")).toBe(false);
    });

    test("notify runs after a deployed night, passed or failed, and after a manual run", () => {
      for (const smoke of ["success", "failure", "cancelled"] as const) {
        expect(notifyRuns(scheduleRef, asked("true"), smoke)).toBe(true);
        expect(notifyRuns(scheduleRef, asked(undefined, "failure"), smoke)).toBe(true);
        expect(notifyRuns(dispatchRef, NOT_ASKED, smoke)).toBe(true);
      }
    });
  });

  describe("the cache of the Rust images (Laysi/trustiot-v2#60, Laysi/trustiot-v2#65)", () => {
    const steps = (jobs.smoke?.steps ?? []) as YamlMap[];
    const step = (id: string) => {
      const found = steps.find((s) => s.id === id);
      if (found === undefined) throw new Error(`smoke has no step ${id}`);
      return found;
    };
    const savesCache = (github: YamlMap) => truthy(evaluate(expressionOf((jobs.smoke?.env as YamlMap).NIGHTLY_SAVES_CACHE), { github }));
    const hit = (value: boolean) => ({ "rust-images-cache": { outputs: { "cache-hit": value ? "true" : "false" } } });
    const save = steps.find((s) => typeof s.uses === "string" && s.uses.startsWith("actions/cache/save@"));
    const restore = step("rust-images-cache");

    test("only scheduled and manual runs on main may save; a pull request never does", () => {
      expect(savesCache(schedule)).toBe(true);
      expect(savesCache(dispatch)).toBe(true);
      expect(savesCache({ ...dispatch, ref: "refs/heads/feature" })).toBe(false);
      for (const github of [pullRequest("synchronize", ["smoke"]), pullRequest("labeled", ["smoke"], "smoke")]) {
        expect(savesCache(github)).toBe(false);
      }
      // The save step itself requires the same decision, whatever the earlier steps did.
      const env = { NIGHTLY_SAVES_CACHE: "false" };
      const built = { ...hit(false), "rust-images-built": { outputs: { built: "true" } } };
      expect(jobRuns(save?.if, { github: pullRequest("synchronize", ["smoke"]), env, steps: built }, "failure")).toBe(false);
      expect(jobRuns(save?.if, { github: schedule, env: { NIGHTLY_SAVES_CACHE: "true" }, steps: built }, "failure")).toBe(true);
      expect(jobRuns(step("rust-images-built").if, { github: schedule, env: { NIGHTLY_SAVES_CACHE: "true" }, steps: hit(true) })).toBe(false);
      // Without every tarball (the step printed nothing) there is nothing to save.
      expect(jobRuns(save?.if, { github: schedule, env: { NIGHTLY_SAVES_CACHE: "true" }, steps: hit(false) }, "failure")).toBe(false);
    });

    test("restores by the exact key only, and saves the same path under the same key", () => {
      expect(restore.uses).toMatch(/^actions\/cache\/restore@[0-9a-f]{40}$/);
      const restoreWith = restore.with as YamlMap;
      expect(Object.keys(restoreWith).sort()).toEqual(["key", "path"]);
      expect(save?.with).toEqual(restoreWith);
      expect(restoreWith.key).toBe("${{ steps.rust-images.outputs.key }}");
      expect(restoreWith.path).toBe("${{ runner.temp }}/image-cache");
      // The key of every image together, and the save only when the directory holds them all.
      expect(step("rust-images").run).toBe('bun scripts/nightly/image-cache.ts key >> "$GITHUB_OUTPUT"');
      expect(step("rust-images-built").run).toBe('bun scripts/nightly/image-cache.ts complete "$RUNNER_TEMP/image-cache" >> "$GITHUB_OUTPUT"');
    });

    test("installs the Rust build tools only on a miss", () => {
      const tools = steps.find((s) => s.uses === "./.github/actions/setup-proto" && String((s.with as YamlMap).tools).includes("zig"));
      expect(jobRuns(tools?.if, { steps: hit(true) })).toBe(false);
      expect(jobRuns(tools?.if, { steps: hit(false) })).toBe(true);
      expect(steps.indexOf(tools!)).toBeGreaterThan(steps.indexOf(restore));
    });
  });

  test("both jobs run on the arm64 runner", () => {
    expect(jobs.smoke?.["runs-on"]).toBe("ubuntu-24.04-arm");
    expect(jobs.notify?.["runs-on"]).toBe("ubuntu-24.04-arm");
  });
});

describe("release.yml", () => {
  test("every job runs on the arm64 runner", () => {
    // The build job builds on the architecture the images ship for, and a rebuild that
    // reproduces a release runs on arm64 too (docs/architecture/ci-release.md).
    const jobs = jobsOf(workflow("release.yml"));
    expect(Object.keys(jobs)).toEqual(["plan", "build", "publish", "verify", "deploy-pr", "report"]);
    for (const [id, job] of Object.entries(jobs)) {
      expect(`${id}: ${String(job["runs-on"])}`).toBe(`${id}: ubuntu-24.04-arm`);
    }
  });
});
