// The conditions in ci.yml and nightly.yml that decide what runs (Laysi/trustiot-v2#309),
// evaluated with GitHub's own expression engine (@actions/expressions) against the event
// payloads GitHub sends, so a typo in an `if` or a concurrency group fails here and not on the
// first pull request it should have stopped.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { Evaluator, Lexer, Parser, data } from "@actions/expressions";
import { truthy } from "@actions/expressions/result";

import { PATH_JOBS } from "./changes.ts";

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
    expect(changes.permissions).toEqual({ contents: "read", actions: "read" });
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

  const smokeRuns = (github: YamlMap) => jobRuns(jobs.smoke?.if, { github });
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
    expect(jobRuns(jobs.notify?.if, { github, needs: { smoke: { result: "success" } } })).toBe(false);
  });

  test("both jobs run on the arm64 runner", () => {
    expect(jobs.smoke?.["runs-on"]).toBe("ubuntu-24.04-arm");
    expect(jobs.notify?.["runs-on"]).toBe("ubuntu-24.04-arm");
  });
});
