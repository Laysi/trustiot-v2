import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  candidates,
  decide,
  evidence,
  GitHubSource,
  parseRuns,
  runInWorkflow,
  summaryLine,
  type NightlyRun,
  type Source,
} from "./unchanged.ts";

const sha = (n: number) => n.toString(16).padStart(40, "0");
const TREE_A = sha(0xaaa);
const TREE_B = sha(0xbbb);

function run(id: number, overrides: Partial<NightlyRun> = {}): NightlyRun {
  return {
    id,
    url: `https://github.com/Laysi/trustiot-v2/actions/runs/${id}`,
    event: "schedule",
    branch: "main",
    conclusion: "success",
    sha: sha(id),
    createdAt: `2026-10-${String(10 + (id % 10)).padStart(2, "0")}T18:17:00Z`,
    ...overrides,
  };
}

/** A source with fixed runs and a tree per commit; `calls` records what was read. */
function source(runs: NightlyRun[] | Error, trees: Record<string, string | Error>): Source & { calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    async listRuns() {
      calls.push("runs");
      if (runs instanceof Error) throw runs;
      return runs;
    },
    async treeOf(commit) {
      calls.push(`tree ${commit}`);
      const tree = trees[commit];
      if (tree === undefined) throw new Error(`no tree for ${commit}`);
      if (tree instanceof Error) throw tree;
      return tree;
    },
  };
}

const HEAD = sha(0x999);

describe("decide", () => {
  test("a scheduled run whose tree equals the tree of the newest passed run skips", async () => {
    const last = run(5);
    const src = source([last], { [HEAD]: TREE_A, [last.sha]: TREE_A });
    const decision = await decide({ eventName: "schedule", sha: HEAD, runId: 99 }, src);
    expect(decision.deploy).toBe(false);
    expect(decision.tree).toBe(TREE_A);
    expect(decision.compared?.id).toBe(5);
    // The one line for the job summary names the compared run and the tree.
    expect(decision.reason).toContain(last.url);
    expect(decision.reason).toContain(TREE_A);
    expect(summaryLine(decision)).toMatch(/^Nightly skipped, nothing was deployed: .*\.\n$/);
  });

  test("trees are compared, not commits: a different commit with the same tree skips", async () => {
    const last = run(5);
    expect(last.sha).not.toBe(HEAD);
    const decision = await decide({ eventName: "schedule", sha: HEAD }, source([last], { [HEAD]: TREE_A, [last.sha]: TREE_A }));
    expect(decision.deploy).toBe(false);
  });

  test("the same commit needs one tree read only", async () => {
    const last = run(5, { sha: HEAD });
    const src = source([last], { [HEAD]: TREE_A });
    expect((await decide({ eventName: "schedule", sha: HEAD }, src)).deploy).toBe(false);
    expect(src.calls.filter((call) => call.startsWith("tree"))).toEqual([`tree ${HEAD}`]);
  });

  test("a changed tree deploys, and says which trees differ", async () => {
    const last = run(5);
    const decision = await decide({ eventName: "schedule", sha: HEAD }, source([last], { [HEAD]: TREE_B, [last.sha]: TREE_A }));
    expect(decision.deploy).toBe(true);
    expect(decision.reason).toContain(TREE_A);
    expect(decision.reason).toContain(TREE_B);
  });

  test("a manual run always deploys and reads nothing", async () => {
    for (const eventName of ["workflow_dispatch", "pull_request", ""]) {
      const src = source([run(5)], { [HEAD]: TREE_A, [sha(5)]: TREE_A });
      const decision = await decide({ eventName, sha: HEAD }, src);
      expect(decision.deploy).toBe(true);
      expect(src.calls).toEqual([]);
    }
  });

  test.each(["failure", "cancelled", "timed_out", "skipped", "startup_failure", ""])(
    "the newest run ended %p: deploys even though the tree is the same",
    async (conclusion) => {
      const last = run(5, { conclusion });
      const decision = await decide({ eventName: "schedule", sha: HEAD }, source([last], { [HEAD]: TREE_A, [last.sha]: TREE_A }));
      expect(decision.deploy).toBe(true);
    },
  );

  test("the newest run counts, not the newest successful one: a failure after a pass deploys", async () => {
    const passed = run(3, { createdAt: "2026-10-12T18:17:00Z" });
    const failed = run(4, { createdAt: "2026-10-13T18:17:00Z", conclusion: "failure" });
    const trees = { [HEAD]: TREE_A, [passed.sha]: TREE_A, [failed.sha]: TREE_A };
    const decision = await decide({ eventName: "schedule", sha: HEAD }, source([passed, failed], trees));
    expect(decision.deploy).toBe(true);
    expect(decision.reason).toContain("did not pass");
    expect(decision.reason).toContain(failed.url);
  });

  test("a failed night is retried, and the pass after it makes the next night skip", async () => {
    const trees = { [HEAD]: TREE_A, [sha(1)]: TREE_A, [sha(2)]: TREE_A };
    const failed = run(1, { createdAt: "2026-10-12T18:17:00Z", conclusion: "failure" });
    expect((await decide({ eventName: "schedule", sha: HEAD }, source([failed], trees))).deploy).toBe(true);
    const retried = run(2, { createdAt: "2026-10-13T18:17:00Z" });
    expect((await decide({ eventName: "schedule", sha: HEAD }, source([failed, retried], trees))).deploy).toBe(false);
  });

  test("a manual run counts as the last run; a passed one lets the night skip", async () => {
    const scheduled = run(3, { createdAt: "2026-10-12T18:17:00Z" });
    const manual = run(4, { createdAt: "2026-10-13T10:00:00Z", event: "workflow_dispatch" });
    const trees = { [HEAD]: TREE_A, [manual.sha]: TREE_A, [scheduled.sha]: TREE_B };
    expect((await decide({ eventName: "schedule", sha: HEAD }, source([scheduled, manual], trees))).deploy).toBe(false);
  });

  test("a manual run that simulated a failure ended as a failure, so the next night deploys and closes its issue", async () => {
    const passed = run(3, { createdAt: "2026-10-12T18:17:00Z" });
    const simulated = run(4, { createdAt: "2026-10-13T10:00:00Z", event: "workflow_dispatch", conclusion: "failure" });
    const trees = { [HEAD]: TREE_A, [passed.sha]: TREE_A, [simulated.sha]: TREE_A };
    expect((await decide({ eventName: "schedule", sha: HEAD }, source([passed, simulated], trees))).deploy).toBe(true);
  });

  test("no completed run, or none that counts, deploys", async () => {
    const trees = { [HEAD]: TREE_A };
    expect((await decide({ eventName: "schedule", sha: HEAD }, source([], trees))).deploy).toBe(true);
    const others = [
      run(1, { event: "pull_request" }),
      run(2, { event: "push" }),
      run(3, { branch: "feature" }),
      run(4, { branch: "main", event: "pull_request" }),
    ];
    const decision = await decide({ eventName: "schedule", sha: HEAD }, source(others, { ...trees, [sha(1)]: TREE_A, [sha(2)]: TREE_A, [sha(3)]: TREE_A, [sha(4)]: TREE_A }));
    expect(decision.deploy).toBe(true);
    expect(decision.reason).toContain("no completed");
  });

  test("the run itself and runs that are not completed are not compared with", async () => {
    const self = run(9, { createdAt: "2026-10-15T18:17:00Z", conclusion: "success" });
    const running = run(8, { createdAt: "2026-10-14T18:17:00Z", conclusion: "" });
    const old = run(1, { createdAt: "2026-10-10T18:17:00Z" });
    const src = source([self, running, old], { [HEAD]: TREE_A, [old.sha]: TREE_B });
    const decision = await decide({ eventName: "schedule", sha: HEAD, runId: 9 }, src);
    expect(decision.compared?.id).toBe(1);
    expect(decision.deploy).toBe(true);
  });

  test("an API failure deploys: reading the runs, or reading a tree", async () => {
    const list = await decide({ eventName: "schedule", sha: HEAD }, source(new Error("HTTP 403"), {}));
    expect(list.deploy).toBe(true);
    expect(list.reason).toContain("HTTP 403");
    const last = run(5);
    for (const trees of [
      { [last.sha]: TREE_A }, // the current commit's tree is missing
      { [HEAD]: TREE_A }, // the compared commit's tree is missing
      { [HEAD]: new Error("HTTP 500"), [last.sha]: TREE_A },
    ]) {
      const decision = await decide({ eventName: "schedule", sha: HEAD }, source([last], trees));
      expect(decision.deploy).toBe(true);
      expect(decision.reason).toContain("trees cannot be read");
    }
  });
});

describe("candidates", () => {
  test("newest first by creation time, ties by id", () => {
    const a = run(1, { createdAt: "2026-10-12T18:17:00Z" });
    const b = run(2, { createdAt: "2026-10-13T18:17:00Z" });
    const c = run(3, { createdAt: "2026-10-13T18:17:00Z" });
    expect(candidates([a, b, c]).map((r) => r.id)).toEqual([3, 2, 1]);
  });
});

describe("parseRuns", () => {
  const raw = (extra: Record<string, unknown> = {}) => ({
    id: 7,
    html_url: "https://github.com/Laysi/trustiot-v2/actions/runs/7",
    event: "schedule",
    head_branch: "main",
    head_sha: sha(7),
    status: "completed",
    conclusion: "success",
    created_at: "2026-10-14T18:17:05Z",
    ...extra,
  });

  test("reads the fields the decision needs", () => {
    expect(parseRuns({ workflow_runs: [raw()] })).toEqual([
      {
        id: 7,
        url: "https://github.com/Laysi/trustiot-v2/actions/runs/7",
        event: "schedule",
        branch: "main",
        conclusion: "success",
        sha: sha(7),
        createdAt: "2026-10-14T18:17:05Z",
      },
    ]);
  });

  test("a run that is not completed has no conclusion", () => {
    expect(parseRuns({ workflow_runs: [raw({ status: "in_progress", conclusion: null })] })[0]?.conclusion).toBe("");
  });

  test.each([null, {}, { workflow_runs: "x" }, { workflow_runs: [null] }, { workflow_runs: [{ id: "7" }] }, { workflow_runs: [{ ...raw(), head_sha: undefined }] }])(
    "rejects a body it does not know: %p",
    (body) => {
      expect(() => parseRuns(body)).toThrow();
    },
  );
});

describe("GitHubSource", () => {
  function fake(responses: (url: string) => { status: number; body: unknown }) {
    const requests: { url: string; method: string; auth: string }[] = [];
    const fetchImpl = (async (input: string | URL | Request, init?: RequestInit) => {
      const url = String(input);
      const headers = new Headers(init?.headers);
      requests.push({ url, method: init?.method ?? "GET", auth: headers.get("authorization") ?? "" });
      const { status, body } = responses(url);
      return new Response(JSON.stringify(body), { status });
    }) as typeof fetch;
    return { requests, fetchImpl };
  }
  const options = (fetchImpl: typeof fetch) => ({ apiUrl: "https://api.github.com/", repository: "Laysi/trustiot-v2", token: "t", fetchImpl });

  test("asks for the completed runs of nightly.yml on main, and only reads", async () => {
    const { requests, fetchImpl } = fake(() => ({ status: 200, body: { workflow_runs: [] } }));
    await new GitHubSource(options(fetchImpl)).listRuns();
    const url = new URL(requests[0]?.url ?? "");
    expect(url.pathname).toBe("/repos/Laysi/trustiot-v2/actions/workflows/nightly.yml/runs");
    expect(url.searchParams.get("branch")).toBe("main");
    expect(url.searchParams.get("status")).toBe("completed");
    expect(url.searchParams.get("exclude_pull_requests")).toBe("true");
    expect(requests[0]?.auth).toBe("Bearer t");
  });

  test("reads the tree of a commit from the git commit object, and refuses a ref that is not a full SHA", async () => {
    const { requests, fetchImpl } = fake(() => ({ status: 200, body: { tree: { sha: TREE_A } } }));
    const github = new GitHubSource(options(fetchImpl));
    expect(await github.treeOf(HEAD)).toBe(TREE_A);
    expect(new URL(requests[0]?.url ?? "").pathname).toBe(`/repos/Laysi/trustiot-v2/git/commits/${HEAD}`);
    await expect(github.treeOf("main")).rejects.toThrow("full commit SHA");
    await expect(github.treeOf("../x")).rejects.toThrow();
    expect(requests).toHaveLength(1);
  });

  test("a failing request names the status and never the token", async () => {
    const { fetchImpl } = fake(() => ({ status: 403, body: { message: "no" } }));
    const error = await new GitHubSource(options(fetchImpl)).listRuns().catch((e: Error) => e);
    expect(String(error)).toContain("HTTP 403");
    expect(String(error)).not.toContain("Bearer");
  });

  test("a body without a tree is an error", async () => {
    const { fetchImpl } = fake(() => ({ status: 200, body: {} }));
    await expect(new GitHubSource(options(fetchImpl)).treeOf(HEAD)).rejects.toThrow("tree.sha");
  });

  test("rejects a repository that is not owner/name and an empty token", () => {
    expect(() => new GitHubSource({ apiUrl: "x", repository: "a/b/c", token: "t" })).toThrow("owner/name");
    expect(() => new GitHubSource({ apiUrl: "x", repository: "a/b", token: "" })).toThrow("GITHUB_TOKEN");
  });
});

describe("runInWorkflow", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "nightly-unchanged-"));
  });
  afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
  });

  const written = (file: string) => readFileSync(join(dir, file), "utf8");
  const envOf = (extra: Record<string, string>) => ({
    GITHUB_EVENT_NAME: "schedule",
    GITHUB_SHA: HEAD,
    GITHUB_RUN_ID: "99",
    GITHUB_OUTPUT: join(dir, "output"),
    GITHUB_STEP_SUMMARY: join(dir, "summary"),
    ...extra,
  });

  test("writes deploy=false and one summary line when nothing changed", async () => {
    const last = run(5);
    const decision = await runInWorkflow(envOf({}), source([last], { [HEAD]: TREE_A, [last.sha]: TREE_A }));
    expect(decision.deploy).toBe(false);
    expect(written("output")).toBe("deploy=false\n");
    const summary = written("summary");
    expect(summary.trimEnd().split("\n")).toHaveLength(1);
    expect(summary).toContain(last.url);
    expect(summary).toContain(TREE_A);
  });

  test("writes deploy=true when the tree changed", async () => {
    const last = run(5);
    await runInWorkflow(envOf({}), source([last], { [HEAD]: TREE_B, [last.sha]: TREE_A }));
    expect(written("output")).toBe("deploy=true\n");
  });

  test("a missing token or repository still answers deploy=true and does not throw", async () => {
    const decision = await runInWorkflow(envOf({ GITHUB_REPOSITORY: "Laysi/trustiot-v2", GITHUB_TOKEN: "" }));
    expect(decision.deploy).toBe(true);
    expect(decision.reason).toContain("GITHUB_TOKEN");
    expect(written("output")).toBe("deploy=true\n");
  });

  test("a manual run never reads GitHub, so it needs no token", async () => {
    const decision = await runInWorkflow(envOf({ GITHUB_EVENT_NAME: "workflow_dispatch" }));
    expect(decision.deploy).toBe(true);
    expect(written("output")).toBe("deploy=true\n");
  });
});

describe("evidence (the read-only dry run)", () => {
  test("shows the decision, the trees and the newest runs", async () => {
    const last = run(5);
    const runs = [last, run(4, { conclusion: "failure" })];
    const src = source(runs, { [HEAD]: TREE_A, [last.sha]: TREE_A });
    const decision = await decide({ eventName: "schedule", sha: HEAD }, src);
    const out = evidence(decision, runs, "schedule", HEAD);
    expect(out).toContain("decision: deploy=false");
    expect(out).toContain(`tree of the commit: ${TREE_A}`);
    expect(out).toContain(`compared run: ${last.url}`);
    expect(out).toContain("(2 of 2 listed)");
    expect(out).toContain("failure");
  });
});

describe("the dry run command", () => {
  const script = join(import.meta.dir, "unchanged.ts");
  const last = run(5);
  // Each test starts a bun process; a loaded machine needs far more than the default 5 s.
  const SPAWN_TEST_MS = 120_000;

  /** A local stand-in for the GitHub API that records every request. */
  function serve(): { server: ReturnType<typeof Bun.serve>; requests: string[] } {
    const requests: string[] = [];
    const server = Bun.serve({
      port: 0,
      fetch(request) {
        const url = new URL(request.url);
        requests.push(`${request.method} ${url.pathname}`);
        if (url.pathname.endsWith("/actions/workflows/nightly.yml/runs")) {
          return Response.json({
            workflow_runs: [
              {
                id: 5,
                html_url: last.url,
                event: "schedule",
                head_branch: "main",
                head_sha: last.sha,
                status: "completed",
                conclusion: "success",
                created_at: last.createdAt,
              },
            ],
          });
        }
        if (url.pathname.endsWith("/git/ref/heads/main")) return Response.json({ object: { sha: HEAD } });
        if (url.pathname.endsWith(`/git/commits/${HEAD}`) || url.pathname.endsWith(`/git/commits/${last.sha}`)) {
          return Response.json({ tree: { sha: TREE_A } });
        }
        return new Response("not found", { status: 404 });
      },
    });
    return { server, requests };
  }

  async function dryRun(env: Record<string, string>, ...args: string[]) {
    const child = Bun.spawn(["bun", script, "--dry-run", ...args], {
      env: { PATH: Bun.env.PATH ?? "", ...env },
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
    return { code: await child.exited, stdout, stderr };
  }

  test("prints the decision and the evidence for the tip of main, with GET requests only", async () => {
    const { server, requests } = serve();
    try {
      const result = await dryRun({ GITHUB_TOKEN: "t", GITHUB_REPOSITORY: "Laysi/trustiot-v2", GITHUB_API_URL: `http://127.0.0.1:${server.port}` });
      expect(result.code).toBe(0);
      expect(result.stdout).toContain("decision: deploy=false");
      expect(result.stdout).toContain(`commit ${HEAD}`);
      expect(result.stdout).toContain(`tree of the commit: ${TREE_A}`);
      expect(result.stdout).toContain(`compared run: ${last.url}`);
      expect(requests.every((request) => request.startsWith("GET "))).toBe(true);
      expect(requests.length).toBeGreaterThanOrEqual(3);
    } finally {
      await server.stop(true);
    }
  }, SPAWN_TEST_MS);

  test("a tree that cannot be read prints a deploy decision with its reason", async () => {
    const { server } = serve();
    try {
      const env = { GITHUB_TOKEN: "t", GITHUB_REPOSITORY: "Laysi/trustiot-v2", GITHUB_API_URL: `http://127.0.0.1:${server.port}` };
      const result = await dryRun(env, "--sha", sha(0x123));
      expect(result.code).toBe(0);
      expect(result.stdout).toContain("decision: deploy=true");
      expect(result.stdout).toContain("trees cannot be read");
    } finally {
      await server.stop(true);
    }
  }, SPAWN_TEST_MS);

  test("without a token it says what to set and exits 2", async () => {
    const result = await dryRun({ GITHUB_REPOSITORY: "Laysi/trustiot-v2" });
    expect(result.code).toBe(2);
    expect(result.stderr).toContain("GITHUB_TOKEN");
  }, SPAWN_TEST_MS);
});
