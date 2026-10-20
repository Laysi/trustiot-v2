/**
 * The deploy proposal that release.yml makes after a release is published and verified
 * (Laysi/trustiot-v2#70): one commit on a new branch `deploy/trustiot-<version>` off main that
 * rewrites the chart source and the shape values in deploy/flux/ (flux.ts) of every cluster in
 * DEPLOY_CLUSTERS that runs an older version (Laysi/trustiot-v2#457: the SaaS cluster and the demo
 * environment), a pull request of that branch, and a ci.yml run on it. Merging the PR deploys the
 * release: Flux on each cluster pulls main. Reverting it rolls back.
 *
 * Everything goes through the GitHub REST API with the job's GITHUB_TOKEN, so the commit is
 * made and signed by GitHub (github-actions[bot]) and the job needs no git credentials. A PR
 * opened with GITHUB_TOKEN starts no workflow, so ci.yml is started with workflow_dispatch on
 * the branch (the one event GITHUB_TOKEN may trigger); its checks attach to the PR's head commit.
 *
 * A re-run is safe: an existing branch is kept as it is, a PR of the branch (open or closed) is
 * not opened again, and a release that is not newer than what main deploys on any cluster
 * proposes nothing. A kept branch without a PR is proposed only when it changes exactly the files
 * of the clusters that main now needs moved, each one what this run would write; otherwise the job
 * stops, because the PR text is computed from main and the release and would misstate the branch.
 *
 * When the repository setting "Allow GitHub Actions to create and approve pull requests" is off
 * (the documented default, docs/architecture/ci-release.md), GitHub refuses the PR. That is not a
 * failure of the release: the branch is pushed, and the outcome `branch-only` carries the compare
 * link for a person to open the PR (their PR runs ci by itself). Every other refusal throws.
 */

import {
  CHART_SHAPE_VALUES,
  type ClusterName,
  DEPLOY_CLUSTERS,
  chartSourceFile,
  deployBranch,
  isNewerRelease,
  parseChartSource,
  renderChartSource,
  renderShapeValues,
  shapeValuesFile,
} from "./flux.ts";
import { type PublishedRelease, type ReleasePlan, ReleaseError, isMap } from "./lib.ts";

export const CI_WORKFLOW_FILE = "ci.yml";

export interface ApiResponse {
  status: number;
  body: unknown;
}

/** One authenticated GitHub REST call; `path` starts with `/repos/<owner>/<name>`. */
export type GitHubApi = (method: "GET" | "POST" | "PATCH", path: string, body?: unknown) => Promise<ApiResponse>;

/** The REST client used in the workflow. */
export function githubApi(token: string, apiUrl = "https://api.github.com"): GitHubApi {
  return async (method, path, body) => {
    const response = await fetch(`${apiUrl}${path}`, {
      method,
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${token}`,
        "X-GitHub-Api-Version": "2022-11-28",
        ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(30_000),
    });
    const text = await response.text();
    let parsed: unknown = text;
    try {
      parsed = text === "" ? undefined : JSON.parse(text);
    } catch {
      // Keep the text: error pages are not always JSON.
    }
    return { status: response.status, body: parsed };
  };
}

function field(value: unknown, ...keys: string[]): unknown {
  let current = value;
  for (const key of keys) {
    current = isMap(current) ? current[key] : undefined;
  }
  return current;
}

function apiMessage(response: ApiResponse): string {
  const message = field(response.body, "message");
  return typeof message === "string" ? message : `HTTP ${response.status}`;
}

async function expect(api: GitHubApi, what: string, method: "GET" | "POST" | "PATCH", path: string, body?: unknown, statuses = [200, 201]): Promise<unknown> {
  const response = await api(method, path, body);
  if (!statuses.includes(response.status)) {
    throw new ReleaseError(`${what} failed with HTTP ${response.status}: ${apiMessage(response)}`);
  }
  return response.body;
}

export interface DeployInput {
  plan: ReleasePlan;
  published: Omit<PublishedRelease, "releaseUrl"> & { releaseUrl?: string };
  /** deploy/chart/values-saas.yaml of the released commit. */
  shapeValues: string;
  serverUrl?: string;
}

/** The version main deploys on one cluster. */
export interface ClusterVersion {
  cluster: ClusterName;
  version: string;
}

export type DeployOutcome =
  | { kind: "not-newer"; deployed: ClusterVersion[] }
  | { kind: "proposed"; branch: string; pullRequest: string; created: boolean; branchCreated: boolean; ciDispatched: boolean }
  | { kind: "branch-only"; branch: string; compare: string; branchCreated: boolean; reason: string; title: string; body: string; draft: boolean };

/** GitHub's answer when the repository does not let workflows open pull requests. */
const PR_CREATION_DISABLED = /not permitted to create or approve pull requests/i;

/** The PR title, also the subject of the deploy commit. */
export function pullRequestTitle(version: string): string {
  return `chore(flux): deploy trustiot ${version}`;
}

/**
 * The PR text (zh-TW; PR bodies are read by the team). `moved` are the clusters the PR moves
 * (with the version each one runs now), `kept` the clusters already at this release or newer.
 * `openedBy` says who opens the PR: release.yml itself, or a person from the compare link when
 * Actions may not open PRs.
 */
export function renderPullRequestBody(
  input: DeployInput,
  moved: readonly ClusterVersion[],
  kept: readonly ClusterVersion[] = [],
  openedBy: "workflow" | "person" = "workflow",
): string {
  const { plan, published } = input;
  const release = input.published.releaseUrl ? `[${plan.tag}](${input.published.releaseUrl})` : `\`${plan.tag}\``;
  return [
    `release.yml 已發佈並驗證 ${release}，本 PR 把下列叢集的部署換成 \`${plan.version}\`：`,
    "",
    "| 叢集 | 目前的版本 | 換成 |",
    "| --- | --- | --- |",
    ...moved.map((m) => `| \`${m.cluster}\` | \`${m.version}\` | \`${plan.version}\` |`),
    "",
    ...(kept.length > 0
      ? [`不變（已經是這一版或更新的版本）：${kept.map((k) => `\`${k.cluster}\`（\`${k.version}\`）`).join("、")}。`, ""]
      : []),
    ...(plan.prerelease
      ? [
          "> [!WARNING]",
          "> 這是候選版（rc），tag 不一定在 main 上。只有確定要讓上列叢集跑這個候選版時才合併；否則關閉本 PR。",
          "",
        ]
      : []),
    "## 變更",
    "",
    ...moved.flatMap((m) => [
      `- \`${chartSourceFile(m.cluster)}\`：chart \`oci://${plan.chart.repository}\` 版本 \`${plan.version}\`，digest \`${published.chart.digest}\`；Flux 只接受 release.yml 在 \`${plan.tag}\` 上的 keyless 簽章。`,
      `- \`${shapeValuesFile(m.cluster)}\`：\`${plan.tag}\` 的 \`deploy/chart/values-saas.yaml\`。`,
    ]),
    "",
    "## 合併之後",
    "",
    "- 合併即部署：每個叢集的 Flux 每分鐘檢查一次 main，取得新的 commit 後以 Helm 升級（`deploy/flux/README.md`）。",
    "- 回到前一版：revert 本 PR 並合併。只要讓其中一個叢集回到前一版時，另開 PR 只把那個叢集的兩個檔案改回前一版的內容。",
    "",
    "## CI",
    "",
    openedBy === "workflow"
      ? "以 GITHUB_TOKEN 開的 PR 不會觸發 workflow，所以 release.yml 另以 `workflow_dispatch` 在本分支執行 ci.yml，結果顯示在本 PR 的 checks。若沒有看到 ci 的結果，關閉再重新開啟本 PR 即會觸發。"
      : "本 PR 由人從 release.yml 推好的分支開立（repo 不允許 Actions 開 PR），`pull_request` 事件會照常執行 ci.yml。",
    "",
    "## 合併前確認",
    "",
    `- 本 PR 只有一個 \`github-actions[bot]\` 的 commit，而且只改上列的檔案。有寫入權限的人都能再推 commit 到分支 \`${deployBranch(plan.version)}\`，多出的 commit 不是 release.yml 做的。`,
    "",
  ].join("\n");
}

/** The files the deploy commit writes for the clusters it moves, with their contents. */
function deployFiles(input: DeployInput, moved: readonly ClusterVersion[]): { path: string; content: string }[] {
  const { plan, published } = input;
  const chartSource = renderChartSource({
    version: plan.version,
    digest: published.chart.digest,
    repository: plan.repository,
    chartRepository: plan.chart.repository,
  });
  const shapeValues = renderShapeValues(plan.version, input.shapeValues);
  return moved.flatMap((m) => [
    { path: chartSourceFile(m.cluster), content: chartSource },
    { path: shapeValuesFile(m.cluster), content: shapeValues },
  ]);
}

/**
 * Stops when a branch kept from an earlier run does not change exactly what a PR opened now would
 * say: the chart source and shape values of the clusters in `moved`, against the branch's merge
 * base with main, each file byte for byte what this run would write. Otherwise the PR text could
 * name a cluster that the branch leaves alone, keep quiet about one it moves, or describe values
 * that the branch does not deploy.
 */
async function requireBranchMatches(
  api: GitHubApi,
  input: DeployInput,
  where: { repo: string; branch: string; mainSha: string; branchSha: unknown; moved: readonly ClusterVersion[] },
): Promise<void> {
  const { repo, branch, mainSha, branchSha, moved } = where;
  const { plan, published } = input;
  if (typeof branchSha !== "string") {
    throw new ReleaseError(`GitHub returned no commit for branch ${branch}`);
  }
  const compare = await expect(api, `comparing branch ${branch} with main`, "GET", `${repo}/compare/${mainSha}...${branchSha}`);
  const listed = field(compare, "files");
  const files = (Array.isArray(listed) ? listed : []).map((file) => field(file, "filename")).filter((name): name is string => typeof name === "string").sort();
  const changes = (names: readonly string[]) => DEPLOY_CLUSTERS.filter((cluster) => names.includes(chartSourceFile(cluster)) || names.includes(shapeValuesFile(cluster)));
  const written = deployFiles(input, moved);
  const expected = written.map((file) => file.path).sort();
  if (JSON.stringify(files) !== JSON.stringify(expected)) {
    const other = files.filter((name) => !DEPLOY_CLUSTERS.some((c) => name === chartSourceFile(c) || name === shapeValuesFile(c)));
    throw new ReleaseError(
      `branch ${branch} exists from an earlier run and changes ${changes(files).join(", ") || "no cluster"}` +
        `${other.length > 0 ? ` and ${other.join(", ")}` : ""}, but main now needs ${moved.map((m) => m.cluster).join(", ")} moved: ` +
        "a PR of it would not say what it deploys. No PR is open for it: delete the branch and re-run the deploy-pr job",
    );
  }
  for (const { path, content } of written) {
    const contents = await expect(api, `reading ${path} on branch ${branch}`, "GET", `${repo}/contents/${path}?ref=${branchSha}`);
    const encoded = field(contents, "content");
    if (typeof encoded !== "string") {
      throw new ReleaseError(`${path} on branch ${branch} has no content`);
    }
    const kept = Buffer.from(encoded, "base64").toString("utf8");
    if (kept === content) continue;
    if (moved.some((m) => chartSourceFile(m.cluster) === path)) {
      // Names the release the branch deploys; a hand-edited file stops parseChartSource itself.
      const source = parseChartSource(kept, plan.repository, plan.chart.repository, path);
      throw new ReleaseError(
        `${path} on branch ${branch} deploys ${source.version} (${source.digest}), not ${plan.version} (${published.chart.digest}): ` +
          "delete the branch and re-run the deploy-pr job",
      );
    }
    throw new ReleaseError(
      `${path} on branch ${branch} is not ${CHART_SHAPE_VALUES} of ${plan.tag}: delete the branch and re-run the deploy-pr job`,
    );
  }
}

/**
 * Proposes deploying the release: returns what it did, including `branch-only` when the
 * repository does not let Actions open PRs. Throws a ReleaseError with the next step for a
 * person when GitHub refuses anything else.
 */
export async function proposeDeploy(api: GitHubApi, input: DeployInput): Promise<DeployOutcome> {
  const { plan, published } = input;
  const repo = `/repos/${plan.repository}`;
  const mainSha = field(await expect(api, "reading main", "GET", `${repo}/git/ref/heads/main`), "object", "sha");
  if (typeof mainSha !== "string") {
    throw new ReleaseError("GitHub returned no commit for main");
  }
  // What every cluster deploys on main. A missing or hand-edited chart source stops the job.
  const deployed: ClusterVersion[] = [];
  for (const cluster of DEPLOY_CLUSTERS) {
    const file = chartSourceFile(cluster);
    const contents = await expect(api, `reading ${file} on main`, "GET", `${repo}/contents/${file}?ref=${mainSha}`);
    const encoded = field(contents, "content");
    if (typeof encoded !== "string") {
      throw new ReleaseError(`${file} on main has no content`);
    }
    const current = parseChartSource(Buffer.from(encoded, "base64").toString("utf8"), plan.repository, plan.chart.repository, file);
    deployed.push({ cluster, version: current.version });
  }
  const moved = deployed.filter((d) => isNewerRelease(plan.version, d.version));
  const kept = deployed.filter((d) => !isNewerRelease(plan.version, d.version));
  if (moved.length === 0) {
    return { kind: "not-newer", deployed };
  }

  const branch = deployBranch(plan.version);
  const existing = await api("GET", `${repo}/git/ref/heads/${branch}`);
  let branchCreated = false;
  if (existing.status === 404) {
    const baseTree = field(await expect(api, "reading main's commit", "GET", `${repo}/git/commits/${mainSha}`), "tree", "sha");
    const files = deployFiles(input, moved);
    const tree = field(
      await expect(api, "creating the tree", "POST", `${repo}/git/trees`, {
        base_tree: baseTree,
        tree: files.map((file) => ({ path: file.path, mode: "100644", type: "blob", content: file.content })),
      }),
      "sha",
    );
    // No author or committer: GitHub makes and signs the commit as the app behind GITHUB_TOKEN.
    const commit = field(
      await expect(api, "creating the commit", "POST", `${repo}/git/commits`, {
        message: `${pullRequestTitle(plan.version)}\n\nRelease ${plan.tag}, chart ${published.chart.digest}, on ${moved.map((m) => m.cluster).join(", ")}.\n`,
        tree,
        parents: [mainSha],
      }),
      "sha",
    );
    await expect(api, `creating branch ${branch}`, "POST", `${repo}/git/refs`, { ref: `refs/heads/${branch}`, sha: commit });
    branchCreated = true;
  } else if (existing.status !== 200) {
    throw new ReleaseError(`reading branch ${branch} failed with HTTP ${existing.status}: ${apiMessage(existing)}`);
  }

  const owner = plan.repository.split("/")[0] ?? "";
  // Any PR of this branch, also a closed one: a person who closed it decided against deploying.
  const open = await expect(api, "listing pull requests", "GET", `${repo}/pulls?state=all&head=${encodeURIComponent(`${owner}:${branch}`)}&base=main`);
  const found = Array.isArray(open) ? field(open[0], "html_url") : undefined;
  if (typeof found === "string") {
    return { kind: "proposed", branch, pullRequest: found, created: false, branchCreated, ciDispatched: false };
  }
  if (!branchCreated) {
    // The PR text below is computed from main now, but the branch was made by an earlier run,
    // when main may have deployed other versions: it must change exactly what the text says.
    await requireBranchMatches(api, input, { repo, branch, mainSha, branchSha: field(existing.body, "object", "sha"), moved });
  }

  const created = await api("POST", `${repo}/pulls`, {
    title: pullRequestTitle(plan.version),
    head: branch,
    base: "main",
    body: renderPullRequestBody(input, moved, kept),
    draft: plan.prerelease,
  });
  if (created.status !== 201) {
    const compare = `${input.serverUrl ?? "https://github.com"}/${plan.repository}/compare/main...${branch}?expand=1`;
    if (PR_CREATION_DISABLED.test(apiMessage(created))) {
      // No ci dispatch: the PR that a person opens from the compare link starts ci by itself.
      return {
        kind: "branch-only",
        branch,
        compare,
        branchCreated,
        reason: 'the repository setting "Allow GitHub Actions to create and approve pull requests" is off (deploy/flux/README.md)',
        title: pullRequestTitle(plan.version),
        body: renderPullRequestBody(input, moved, kept, "person"),
        draft: plan.prerelease,
      };
    }
    throw new ReleaseError(
      `opening the deploy PR failed with HTTP ${created.status}: ${apiMessage(created)}. ` +
        `The branch ${branch} is pushed; open the PR by hand: ${compare}`,
    );
  }
  const pullRequest = field(created.body, "html_url");
  const dispatch = await api("POST", `${repo}/actions/workflows/${CI_WORKFLOW_FILE}/dispatches`, { ref: branch });
  if (dispatch.status !== 204) {
    throw new ReleaseError(
      `the deploy PR ${String(pullRequest)} is open, but starting ci.yml on ${branch} failed with HTTP ${dispatch.status}: ` +
        `${apiMessage(dispatch)}. Close and reopen the PR to run ci`,
    );
  }
  return { kind: "proposed", branch, pullRequest: String(pullRequest), created: true, branchCreated, ciDispatched: true };
}

/**
 * What the deploy-pr job reports for an outcome: the job summary (Markdown) and, when a person
 * has to act, a one-line warning for the run's annotations.
 */
export function describeOutcome(outcome: DeployOutcome, version: string): { summary: string; warning?: string } {
  switch (outcome.kind) {
    case "not-newer":
      return {
        summary: `main already deploys ${outcome.deployed.map((d) => `${d.version} on ${d.cluster}`).join(", ")}, none older than ${version}: no deploy PR.`,
      };
    case "proposed":
      return {
        summary: outcome.created
          ? `Opened the deploy PR ${outcome.pullRequest} (branch ${outcome.branch}) and started ci.yml on it.`
          : `The deploy PR for ${version} already exists: ${outcome.pullRequest}.`,
      };
    case "branch-only": {
      const pushed = outcome.branchCreated ? "Pushed" : "Kept the existing";
      const warning =
        `The deploy PR for ${version} was not opened: ${outcome.reason}. ` +
        `${pushed} branch ${outcome.branch}; open the PR by hand${outcome.draft ? " as a draft" : ""}: ${outcome.compare}`;
      // Four backticks: the body itself contains code spans.
      const summary = [
        `${pushed} branch \`${outcome.branch}\`, but did not open the deploy PR: ${outcome.reason}.`,
        "",
        `Open it by hand${outcome.draft ? " **as a draft** (a release candidate)" : ""}: ${outcome.compare}`,
        "",
        `Title: \`${outcome.title}\``,
        "",
        "<details><summary>Body</summary>",
        "",
        "````markdown",
        outcome.body.trimEnd(),
        "````",
        "",
        "</details>",
      ].join("\n");
      return { summary, warning };
    }
  }
}
