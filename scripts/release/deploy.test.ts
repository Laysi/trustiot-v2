import { describe, expect, test } from "bun:test";
import { type ApiResponse, type DeployInput, type GitHubApi, describeOutcome, proposeDeploy, renderPullRequestBody } from "./deploy.ts";
import { CHART_SOURCE_FILE, SHAPE_VALUES_FILE, parseShapeValues, renderChartSource } from "./flux.ts";
import { REGISTRY, type ReleasePlan } from "./lib.ts";

const REPOSITORY = "Laysi/trustiot-v2";
const CHART = `${REGISTRY}/charts/trustiot`;
const MAIN = "1".repeat(40);
const DIGEST_OLD = `sha256:${"a".repeat(64)}`;
const DIGEST_NEW = `sha256:${"b".repeat(64)}`;
/** GitHub's answer while "Allow GitHub Actions to create and approve pull requests" is off. */
const NOT_PERMITTED: ApiResponse = { status: 403, body: { message: "GitHub Actions is not permitted to create or approve pull requests." } };

function plan(version: string): ReleasePlan {
  return {
    tag: `v${version}`,
    version,
    prerelease: version.includes("-rc."),
    commit: "0123456789abcdef0123456789abcdef01234567",
    sourceDateEpoch: 1_791_630_056,
    repository: REPOSITORY,
    images: [],
    chart: { name: "trustiot", repository: CHART, version },
  };
}

function input(version: string): DeployInput {
  return {
    plan: plan(version),
    published: { images: [], chart: { name: "trustiot", ref: `${CHART}:${version}`, digest: DIGEST_NEW }, releaseUrl: "https://example.test/release" },
    shapeValues: "global:\n  deploymentMode: saas\n",
  };
}

interface Call {
  method: string;
  path: string;
  body?: unknown;
}

/** A fake GitHub: answers by method and path prefix, and records every call. */
function fakeGitHub(options: {
  deployed?: string;
  chartSource?: string;
  branchExists?: boolean;
  existingPr?: string;
  createPr?: ApiResponse;
  dispatch?: ApiResponse;
}) {
  const calls: Call[] = [];
  const deployedSource =
    options.chartSource ??
    renderChartSource({ version: options.deployed ?? "1.0.0", digest: DIGEST_OLD, repository: REPOSITORY, chartRepository: CHART });
  const repo = `/repos/${REPOSITORY}`;
  const api: GitHubApi = async (method, path, body) => {
    calls.push({ method, path, body });
    const at = (prefix: string) => path.startsWith(`${repo}${prefix}`);
    if (method === "GET" && at("/git/ref/heads/main")) return { status: 200, body: { object: { sha: MAIN } } };
    if (method === "GET" && at(`/contents/${CHART_SOURCE_FILE}?ref=${MAIN}`)) {
      return { status: 200, body: { content: Buffer.from(deployedSource).toString("base64") } };
    }
    if (method === "GET" && at("/git/ref/heads/deploy/")) return options.branchExists ? { status: 200, body: {} } : { status: 404, body: { message: "Not Found" } };
    if (method === "GET" && at(`/git/commits/${MAIN}`)) return { status: 200, body: { tree: { sha: "tree-main" } } };
    if (method === "POST" && at("/git/trees")) return { status: 201, body: { sha: "tree-new" } };
    if (method === "POST" && at("/git/commits")) return { status: 201, body: { sha: "commit-new" } };
    if (method === "POST" && at("/git/refs")) return { status: 201, body: {} };
    if (method === "GET" && at("/pulls?")) return { status: 200, body: options.existingPr ? [{ html_url: options.existingPr }] : [] };
    if (method === "POST" && at("/pulls")) return options.createPr ?? { status: 201, body: { html_url: "https://example.test/pull/9" } };
    if (method === "POST" && at("/actions/workflows/ci.yml/dispatches")) return options.dispatch ?? { status: 204, body: undefined };
    return { status: 500, body: { message: `unexpected ${method} ${path}` } };
  };
  return { api, calls };
}

describe("proposeDeploy", () => {
  test("a newer release: one signed commit off main on a new branch, a PR and a ci run", async () => {
    const { api, calls } = fakeGitHub({ deployed: "1.0.0" });
    const outcome = await proposeDeploy(api, input("1.1.0"));
    expect(outcome).toEqual({
      kind: "proposed",
      branch: "deploy/trustiot-1.1.0",
      pullRequest: "https://example.test/pull/9",
      created: true,
      branchCreated: true,
      ciDispatched: true,
    });

    const tree = calls.find((c) => c.path.endsWith("/git/trees"))?.body as { base_tree: string; tree: { path: string; content: string }[] };
    expect(tree.base_tree).toBe("tree-main");
    expect(tree.tree.map((file) => file.path)).toEqual([CHART_SOURCE_FILE, SHAPE_VALUES_FILE]);
    expect(tree.tree[0]?.content).toBe(renderChartSource({ version: "1.1.0", digest: DIGEST_NEW, repository: REPOSITORY, chartRepository: CHART }));
    expect(parseShapeValues(tree.tree[1]?.content ?? "")).toEqual({ version: "1.1.0", source: "global:\n  deploymentMode: saas\n" });

    const commit = calls.find((c) => c.path.endsWith("/git/commits") && c.method === "POST")?.body as YamlLike;
    // No author or committer: GitHub signs commits it makes for the token's app.
    expect(commit).toEqual({ message: expect.stringMatching(/^chore\(flux\): deploy trustiot 1\.1\.0\n/), tree: "tree-new", parents: [MAIN] });
    expect(calls.find((c) => c.path.endsWith("/git/refs"))?.body).toEqual({ ref: "refs/heads/deploy/trustiot-1.1.0", sha: "commit-new" });

    const pr = calls.find((c) => c.method === "POST" && c.path.endsWith("/pulls"))?.body as YamlLike;
    expect(pr).toMatchObject({ title: "chore(flux): deploy trustiot 1.1.0", head: "deploy/trustiot-1.1.0", base: "main", draft: false });
    expect(calls.at(-1)).toEqual({ method: "POST", path: `/repos/${REPOSITORY}/actions/workflows/ci.yml/dispatches`, body: { ref: "deploy/trustiot-1.1.0" } });
  });

  test("a release candidate is proposed as a draft PR with a warning", async () => {
    const { api, calls } = fakeGitHub({ deployed: "1.0.0" });
    await proposeDeploy(api, input("1.1.0-rc.1"));
    const pr = calls.find((c) => c.method === "POST" && c.path.endsWith("/pulls"))?.body as { draft: boolean; body: string };
    expect(pr.draft).toBe(true);
    expect(pr.body).toContain("候選版");
  });

  test.each(["1.0.0", "0.9.0", "1.0.0-rc.2"])("a release that is not newer than the deployed 1.0.0 (%s) proposes nothing", async (version) => {
    const { api, calls } = fakeGitHub({ deployed: "1.0.0" });
    expect(await proposeDeploy(api, input(version))).toEqual({ kind: "not-newer", deployed: "1.0.0" });
    expect(calls.every((c) => c.method === "GET")).toBe(true);
  });

  test("a re-run keeps the existing branch and does not open its PR again, open or closed", async () => {
    const { api, calls } = fakeGitHub({ deployed: "1.0.0", branchExists: true, existingPr: "https://example.test/pull/7" });
    expect(await proposeDeploy(api, input("1.1.0"))).toMatchObject({ pullRequest: "https://example.test/pull/7", created: false, branchCreated: false });
    expect(calls.filter((c) => c.method === "POST")).toEqual([]);
    expect(calls.find((c) => c.path.includes("/pulls?"))?.path).toContain("state=all");
  });

  test("a hand-edited chart source on main stops the job instead of being overwritten", async () => {
    const edited = renderChartSource({ version: "1.0.0", digest: DIGEST_OLD, repository: REPOSITORY, chartRepository: CHART }).replace(
      /  verify:[\s\S]*$/,
      "",
    );
    const { api } = fakeGitHub({ chartSource: edited });
    await expect(proposeDeploy(api, input("1.1.0"))).rejects.toThrow(/edited by hand/);
  });

  test("when Actions may not open PRs, the branch is pushed and the outcome carries what a person needs, without a ci run", async () => {
    const { api, calls } = fakeGitHub({ deployed: "1.0.0", createPr: NOT_PERMITTED });
    const outcome = await proposeDeploy(api, input("1.1.0"));
    expect(outcome).toMatchObject({
      kind: "branch-only",
      branch: "deploy/trustiot-1.1.0",
      compare: "https://github.com/Laysi/trustiot-v2/compare/main...deploy/trustiot-1.1.0?expand=1",
      branchCreated: true,
      reason: expect.stringContaining("Allow GitHub Actions to create and approve pull requests"),
      title: "chore(flux): deploy trustiot 1.1.0",
      draft: false,
    });
    expect(calls.some((c) => c.path.endsWith("/git/refs") && c.method === "POST")).toBe(true);
    // A person's PR runs ci by itself, so release.yml starts none.
    expect(calls.some((c) => c.path.includes("/dispatches"))).toBe(false);
    if (outcome.kind !== "branch-only") throw new Error("unreachable");
    expect(outcome.body).toBe(renderPullRequestBody(input("1.1.0"), "1.0.0", "person"));
  });

  test("a re-run while Actions still may not open PRs keeps the branch and answers the same way", async () => {
    const { api, calls } = fakeGitHub({ deployed: "1.0.0", branchExists: true, createPr: NOT_PERMITTED });
    expect(await proposeDeploy(api, input("1.1.0-rc.1"))).toMatchObject({ kind: "branch-only", branchCreated: false, draft: true });
    expect(calls.filter((c) => c.method === "POST").map((c) => c.path)).toEqual([`/repos/${REPOSITORY}/pulls`]);
  });

  test.each([
    { status: 403, body: { message: "Resource not accessible by integration" } },
    { status: 422, body: { message: "Validation Failed" } },
    { status: 502, body: "<html>bad gateway</html>" },
  ])("any other refusal to open the PR fails, with the compare link (%o)", async (createPr) => {
    const { api } = fakeGitHub({ deployed: "1.0.0", createPr });
    const error = proposeDeploy(api, input("1.1.0"));
    await expect(error).rejects.toThrow(`opening the deploy PR failed with HTTP ${createPr.status}`);
    await expect(error).rejects.toThrow("https://github.com/Laysi/trustiot-v2/compare/main...deploy/trustiot-1.1.0?expand=1");
  });

  test("when ci cannot be started, the error says how to start it", async () => {
    const { api } = fakeGitHub({ deployed: "1.0.0", dispatch: { status: 403, body: { message: "Resource not accessible by integration" } } });
    await expect(proposeDeploy(api, input("1.1.0"))).rejects.toThrow(/Close and reopen the PR/);
  });

  test("other API failures name the step", async () => {
    const api: GitHubApi = async () => ({ status: 502, body: "<html>bad gateway</html>" });
    await expect(proposeDeploy(api, input("1.1.0"))).rejects.toThrow(/reading main failed with HTTP 502/);
  });
});

describe("renderPullRequestBody", () => {
  test("says what changes, how to roll back, how ci runs and what to check before merging", () => {
    const body = renderPullRequestBody(input("1.1.0"), "1.0.0");
    expect(body).toContain("`1.0.0` 換成 `1.1.0`");
    expect(body).toContain(DIGEST_NEW);
    expect(body).toContain("revert 本 PR");
    expect(body).toContain("workflow_dispatch");
    expect(body).toContain("只有一個 `github-actions[bot]` 的 commit");
    expect(body).toContain("`deploy/trustiot-1.1.0`");
    expect(body).not.toContain("候選版");
  });

  test("a PR that a person opens runs ci through pull_request, not workflow_dispatch", () => {
    const body = renderPullRequestBody(input("1.1.0"), "1.0.0", "person");
    expect(body).toContain("`pull_request` 事件會照常執行 ci.yml");
    expect(body).not.toContain("workflow_dispatch");
  });
});

describe("describeOutcome", () => {
  test("a PR that was opened or already exists needs no warning", () => {
    expect(describeOutcome({ kind: "not-newer", deployed: "1.0.0" }, "0.9.0")).toEqual({
      summary: "main already deploys 1.0.0, which is not older than 0.9.0: no deploy PR.",
    });
    const opened = describeOutcome(
      { kind: "proposed", branch: "deploy/trustiot-1.1.0", pullRequest: "https://example.test/pull/9", created: true, branchCreated: true, ciDispatched: true },
      "1.1.0",
    );
    expect(opened.warning).toBeUndefined();
    expect(opened.summary).toContain("https://example.test/pull/9");
  });

  test("a branch without a PR is a warning with the compare link, and the summary holds title and body", async () => {
    const { api } = fakeGitHub({ deployed: "1.0.0", createPr: NOT_PERMITTED });
    const outcome = await proposeDeploy(api, input("1.1.0-rc.1"));
    const { summary, warning } = describeOutcome(outcome, "1.1.0-rc.1");
    const compare = "https://github.com/Laysi/trustiot-v2/compare/main...deploy/trustiot-1.1.0-rc.1?expand=1";
    expect(warning).toContain(compare);
    expect(warning).toContain("as a draft");
    expect(warning).toContain("Allow GitHub Actions to create and approve pull requests");
    expect(warning).not.toContain("\n");
    expect(summary).toContain(compare);
    expect(summary).toContain("`chore(flux): deploy trustiot 1.1.0-rc.1`");
    expect(summary).toContain(renderPullRequestBody(input("1.1.0-rc.1"), "1.0.0", "person").trimEnd());
    // The body's code spans cannot close the fence around it.
    expect(summary).toContain("````markdown\n");
  });
});

type YamlLike = Record<string, unknown>;
