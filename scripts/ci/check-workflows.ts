/**
 * Workflow policy check for GitHub Actions (docs/architecture/ci-release.md).
 *
 * Rules, applied to every `.github/workflows/*.yml` (or `.yaml`) file and every local action
 * (`action.yml` or `action.yaml` under `.github/actions/`):
 *
 * 1. Every job sets `permissions` to an explicit map of scopes; `{}` grants nothing. The
 *    `read-all` and `write-all` shorthands are rejected, and every level must be `read`,
 *    `write` or `none`. A workflow-level `permissions` must take the same form.
 * 2. Every action or reusable workflow named in `uses:` is pinned to a full 40-character
 *    commit SHA. Container images (`docker://` actions, job containers and service
 *    containers) are pinned to a sha256 digest. Local references (`./...`) need no pin
 *    because they are reviewed together with the change that uses them, but they must point
 *    where this check reads: a local action under `.github/actions/` (symbolic links are
 *    followed), a local reusable workflow directly in `.github/workflows/`.
 * 3. No workflow is triggered by `pull_request_target` or `workflow_run`: both run with the
 *    base repository's secrets and a write token on behalf of code that can come from a fork.
 *
 * Usage: bun scripts/ci/check-workflows.ts [repository-root]
 * Exit status: 0 when every file passes, 1 when a rule is violated, 2 when the files cannot
 * be found or read.
 */

import { existsSync, readdirSync, readFileSync, realpathSync, statSync } from "node:fs";
import { join, posix, relative, resolve } from "node:path";

/** One broken rule, located precisely enough to fix without searching. */
export interface Violation {
  /** The file, relative to the repository root. */
  file: string;
  /** The key path in the file, for example `jobs.rust.steps[2].uses`; empty for the whole file. */
  path: string;
  /** What is wrong and how to fix it. */
  message: string;
}

/** The workflow and action files under a repository root, as absolute paths. */
export interface PolicyFiles {
  workflows: string[];
  actions: string[];
}

type YamlMap = Record<string, unknown>;

const FULL_COMMIT_SHA = /^[0-9a-f]{40}$/;
const IMAGE_DIGEST = /@sha256:[0-9a-f]{64}$/;
const PERMISSION_LEVELS: ReadonlySet<string> = new Set(["read", "write", "none"]);
const FORBIDDEN_TRIGGERS: ReadonlyMap<string, string> = new Map([
  [
    "pull_request_target",
    "it runs with the base repository's secrets and a write token for pull requests from forks",
  ],
  [
    "workflow_run",
    "it runs with the base repository's secrets after workflows that fork pull requests can start",
  ],
]);

function isMap(value: unknown): value is YamlMap {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function show(value: unknown): string {
  return typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
}

/** Appends `key` to a dotted key path; the workflow's top level has the empty path. */
function child(path: string, key: string): string {
  return path === "" ? key : `${path}.${key}`;
}

/**
 * What a `uses:` names: an action (in a step) or a reusable workflow (in a job). It decides
 * where a local reference may point.
 */
export type UsesKind = "action" | "workflow";

/**
 * Returns why a local `uses: ./...` reference points outside the files this check reads, or
 * `undefined`. GitHub resolves the path from the repository root.
 */
function localUsesProblem(reference: string, kind: UsesKind): string | undefined {
  const path = posix.normalize(reference);
  if (kind === "workflow") {
    return /^\.github\/workflows\/[^/]+\.ya?ml$/.test(path)
      ? undefined
      : `\`${reference}\` is not a workflow file directly in \`.github/workflows/\`, ` +
          "the only place a local reusable workflow can be checked";
  }
  return path === ".github/actions" || path.startsWith(".github/actions/")
    ? undefined
    : `\`${reference}\` is outside \`.github/actions/\`; keep local actions there so that ` +
        "this check reviews them";
}

/**
 * Returns why a `uses:` value is not acceptable, or `undefined` when it is a local action
 * under `.github/actions/` (or, for a job, a local reusable workflow), a remote action or
 * reusable workflow pinned to a full commit SHA, or a `docker://` image pinned to a digest.
 */
export function usesProblem(reference: unknown, kind: UsesKind = "action"): string | undefined {
  if (typeof reference !== "string" || reference.trim() === "") {
    return "`uses` must name an action, for example `owner/repo@<40-character commit SHA>`";
  }
  if (reference.startsWith("./")) {
    return localUsesProblem(reference, kind);
  }
  if (reference.startsWith("docker://")) {
    return IMAGE_DIGEST.test(reference)
      ? undefined
      : `\`${reference}\` is not pinned to an image digest; ` +
          "use `docker://<image>@sha256:<digest>`";
  }
  const at = reference.lastIndexOf("@");
  const name = at === -1 ? reference : reference.slice(0, at);
  if (!name.includes("/")) {
    return (
      `\`${reference}\` is neither a remote action (\`owner/repo@<sha>\`) ` +
      "nor a local action (`./path`)"
    );
  }
  const ref = at === -1 ? "" : reference.slice(at + 1);
  if (!FULL_COMMIT_SHA.test(ref)) {
    return (
      `\`${reference}\` is not pinned to a full 40-character commit SHA; ` +
      "use `owner/repo@<sha> # <tag>` so the code that runs cannot change under the same reference"
    );
  }
  return undefined;
}

/** Returns why a container image reference is not pinned to a digest, or `undefined`. */
export function imageProblem(image: unknown): string | undefined {
  if (typeof image !== "string" || image.trim() === "") {
    return "the container `image` must be a literal image reference pinned to a digest";
  }
  return IMAGE_DIGEST.test(image)
    ? undefined
    : `image \`${image}\` is not pinned to a digest; use \`<image>:<tag>@sha256:<digest>\``;
}

/**
 * Returns the problems with the `permissions` of the job (or workflow, for the empty path)
 * at `owner`, or an empty list.
 */
export function permissionsProblems(value: unknown, owner: string): Omit<Violation, "file">[] {
  const path = child(owner, "permissions");
  if (value === undefined) {
    return [
      {
        path: owner,
        message:
          "missing `permissions`; list every scope the job needs, for example " +
          "`permissions: { contents: read }`, or `permissions: {}` for none",
      },
    ];
  }
  if (typeof value === "string") {
    return [
      {
        path,
        message: `\`permissions: ${value}\` is a shorthand; list each scope explicitly instead`,
      },
    ];
  }
  if (!isMap(value)) {
    return [
      {
        path,
        message: "`permissions` must be a map from scope to `read`, `write` or `none`",
      },
    ];
  }
  return Object.entries(value)
    .filter(([, level]) => typeof level !== "string" || !PERMISSION_LEVELS.has(level))
    .map(([scope, level]) => ({
      path: child(path, scope),
      message: `\`${scope}: ${show(level)}\` must be \`read\`, \`write\` or \`none\``,
    }));
}

function triggerNames(on: unknown): string[] {
  if (typeof on === "string") {
    return [on];
  }
  if (Array.isArray(on)) {
    return on.filter((trigger): trigger is string => typeof trigger === "string");
  }
  return isMap(on) ? Object.keys(on) : [];
}

function parseYaml(file: string, source: string): { doc?: unknown; violation?: Violation } {
  try {
    return { doc: Bun.YAML.parse(source) };
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    return { violation: { file, path: "", message: `cannot parse YAML: ${reason}` } };
  }
}

function stepsProblems(steps: unknown, path: string): Omit<Violation, "file">[] {
  if (steps === undefined) {
    return [];
  }
  if (!Array.isArray(steps)) {
    return [{ path, message: "`steps` must be a list" }];
  }
  const problems: Omit<Violation, "file">[] = [];
  steps.forEach((step, index) => {
    if (isMap(step) && "uses" in step) {
      const problem = usesProblem(step.uses);
      if (problem !== undefined) {
        problems.push({ path: `${path}[${index}].uses`, message: problem });
      }
    }
  });
  return problems;
}

function jobProblems(jobId: string, job: unknown): Omit<Violation, "file">[] {
  const path = `jobs.${jobId}`;
  if (!isMap(job)) {
    return [{ path, message: "a job must be a map" }];
  }
  const problems = permissionsProblems(job.permissions, path);
  if ("uses" in job) {
    const problem = usesProblem(job.uses, "workflow");
    if (problem !== undefined) {
      problems.push({ path: `${path}.uses`, message: problem });
    }
  }
  if (job.container !== undefined) {
    const image = isMap(job.container) ? job.container.image : job.container;
    const problem = imageProblem(image);
    if (problem !== undefined) {
      problems.push({ path: `${path}.container`, message: problem });
    }
  }
  if (job.services !== undefined) {
    if (!isMap(job.services)) {
      problems.push({ path: `${path}.services`, message: "`services` must be a map" });
    } else {
      for (const [name, service] of Object.entries(job.services)) {
        const problem = imageProblem(isMap(service) ? service.image : undefined);
        if (problem !== undefined) {
          problems.push({ path: `${path}.services.${name}.image`, message: problem });
        }
      }
    }
  }
  problems.push(...stepsProblems(job.steps, `${path}.steps`));
  return problems;
}

/** Checks one workflow file. `file` is only used to label the violations. */
export function checkWorkflow(file: string, source: string): Violation[] {
  const { doc, violation } = parseYaml(file, source);
  if (violation !== undefined) {
    return [violation];
  }
  if (!isMap(doc)) {
    return [{ file, path: "", message: "a workflow must be a YAML map" }];
  }
  const problems: Omit<Violation, "file">[] = [];

  if (doc.on === undefined) {
    problems.push({ path: "on", message: "the workflow has no `on` triggers" });
  }
  for (const trigger of triggerNames(doc.on)) {
    const reason = FORBIDDEN_TRIGGERS.get(trigger);
    if (reason !== undefined) {
      problems.push({ path: `on.${trigger}`, message: `\`${trigger}\` is not allowed: ${reason}` });
    }
  }

  // Optional at the top level: every job must set its own `permissions` anyway.
  if (doc.permissions !== undefined) {
    problems.push(...permissionsProblems(doc.permissions, ""));
  }

  if (!isMap(doc.jobs) || Object.keys(doc.jobs).length === 0) {
    problems.push({ path: "jobs", message: "the workflow must define at least one job" });
  } else {
    for (const [jobId, job] of Object.entries(doc.jobs)) {
      problems.push(...jobProblems(jobId, job));
    }
  }
  return problems.map((problem) => ({ file, ...problem }));
}

/** Checks one local action (`action.yml`). `file` is only used to label the violations. */
export function checkAction(file: string, source: string): Violation[] {
  const { doc, violation } = parseYaml(file, source);
  if (violation !== undefined) {
    return [violation];
  }
  if (!isMap(doc) || !isMap(doc.runs)) {
    return [{ file, path: "runs", message: "an action must define a `runs` map" }];
  }
  const runs = doc.runs;
  const problems: Omit<Violation, "file">[] = [];
  if (runs.using === "composite") {
    problems.push(...stepsProblems(runs.steps, "runs.steps"));
  } else if (
    runs.using === "docker" &&
    typeof runs.image === "string" &&
    runs.image.startsWith("docker://")
  ) {
    const problem = usesProblem(runs.image);
    if (problem !== undefined) {
      problems.push({ path: "runs.image", message: problem });
    }
  }
  return problems.map((problem) => ({ file, ...problem }));
}

function yamlFilesIn(directory: string): string[] {
  if (!existsSync(directory)) {
    return [];
  }
  return readdirSync(directory)
    .filter((name) => /\.ya?ml$/.test(name))
    .map((name) => join(directory, name))
    .filter((path) => statSync(path).isFile())
    .sort();
}

/**
 * Finds every `action.yml` or `action.yaml` under `directory`. Symbolic links are followed,
 * because GitHub follows them too, so a linked action is checked where it points; `seen`
 * stops link cycles.
 */
function actionFilesUnder(directory: string, seen: Set<string> = new Set()): string[] {
  if (!existsSync(directory)) {
    return [];
  }
  const real = realpathSync(directory);
  if (seen.has(real)) {
    return [];
  }
  seen.add(real);
  const found: string[] = [];
  for (const name of readdirSync(directory)) {
    const path = join(directory, name);
    // Undefined for a dangling link, which GitHub cannot run either.
    const stats = statSync(path, { throwIfNoEntry: false });
    if (stats?.isDirectory()) {
      found.push(...actionFilesUnder(path, seen));
    } else if (stats?.isFile() && (name === "action.yml" || name === "action.yaml")) {
      found.push(path);
    }
  }
  return found.sort();
}

/**
 * Lists the files the policy applies to. GitHub only runs workflow files that sit directly in
 * `.github/workflows`, so that directory is not searched recursively.
 */
export function findPolicyFiles(root: string): PolicyFiles {
  return {
    workflows: yamlFilesIn(join(root, ".github", "workflows")),
    actions: actionFilesUnder(join(root, ".github", "actions")),
  };
}

/** Checks every workflow and local action under `root`. */
export function checkRepository(root: string): { files: number; violations: Violation[] } {
  const { workflows, actions } = findPolicyFiles(root);
  const violations: Violation[] = [];
  for (const path of workflows) {
    violations.push(...checkWorkflow(relative(root, path), readFileSync(path, "utf8")));
  }
  for (const path of actions) {
    violations.push(...checkAction(relative(root, path), readFileSync(path, "utf8")));
  }
  return { files: workflows.length + actions.length, violations };
}

/** Formats a violation as a GitHub Actions error annotation. */
export function annotation(violation: Violation): string {
  const escapeData = (text: string) =>
    text.replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A");
  const escapeProperty = (text: string) =>
    escapeData(text).replaceAll(":", "%3A").replaceAll(",", "%2C");
  const where = violation.path === "" ? "" : `${violation.path}: `;
  return (
    `::error file=${escapeProperty(violation.file)},title=${escapeProperty("workflow policy")}::` +
    escapeData(`${where}${violation.message}`)
  );
}

/** Runs the check for the command line and returns the exit status. */
export function main(args: readonly string[], env: Record<string, string | undefined>): number {
  const root = resolve(args[0] ?? join(import.meta.dir, "..", ".."));
  let result: { files: number; violations: Violation[] };
  try {
    result = checkRepository(root);
  } catch (error) {
    console.error(`workflow policy: cannot read the workflows under ${root}: ${String(error)}`);
    return 2;
  }
  if (result.files === 0) {
    console.error(`workflow policy: no workflow or action files under ${join(root, ".github")}`);
    return 2;
  }
  for (const violation of result.violations) {
    const where = violation.path === "" ? "" : ` ${violation.path}:`;
    console.log(`${violation.file}:${where} ${violation.message}`);
    if (env.GITHUB_ACTIONS === "true") {
      console.log(annotation(violation));
    }
  }
  if (result.violations.length > 0) {
    console.log(
      `workflow policy: ${result.violations.length} violation(s) in ${result.files} file(s)`,
    );
    return 1;
  }
  console.log(`workflow policy: ${result.files} file(s) checked, no violations`);
  return 0;
}

if (import.meta.main) {
  process.exit(main(Bun.argv.slice(2), process.env));
}
