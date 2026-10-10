// Build-time guard for container build contexts (docs/architecture/secrets.md, rule 2).
// The static check (scripts/ci/check-secret-handling.ts) cannot know which directory a build
// command passes as the context, so every image build in the scripts takes its podman
// arguments from containerBuildArgs: it checks the very directory that podman then builds.
// The static check rejects container builds written anywhere else, and the podman runner
// (scripts/lib/podman.ts) refuses at run time a build whose arguments did not come from here.
import { readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import { checkBuildFile, secretFilesInContext } from "../ci/check-secret-handling.ts";
import { LABEL_KEY } from "./stack.ts";
import { INSTANCE_PATTERN } from "./state.ts";

/**
 * Throws when the build context holds a key or credential file (private key, keystore, `.env`,
 * kubeconfig, `.git`, ...) that its `.containerignore` or `.dockerignore` does not leave out:
 * podman and Docker send every such file to the builder, whether or not the image copies it
 * (Laysi/trustiot#120).
 */
export function assertBuildContext(context: string): void {
  const leaked = secretFilesInContext(context);
  if (leaked.length > 0) {
    throw new Error(
      `the build context ${context} would send ${leaked.join(", ")} to the builder; ` +
        "build from a directory that holds only what the image needs, or leave these files out with the context's ignore file",
    );
  }
}

/** Argument arrays returned by containerBuildArgs; frozen, so nobody adds a second context. */
const checkedBuilds = new WeakSet<readonly string[]>();

/**
 * The `podman build` options a build may use, and whether each takes a value as the next
 * argument (a `--name=value` form is read the same way). Anything else is refused, because
 * some options make the builder read more than the checked context: `--build-context` adds
 * another directory, `--volume` mounts one into RUN steps, `--ignorefile` swaps the ignore
 * file, `--build-arg-file` reads build arguments from a file the checks never see.
 * `--build-arg` is refused too: its value stays in the image history, and a value computed at
 * run time cannot be shown not to be a secret. Build-time secrets go through `--secret`.
 */
const BUILD_OPTIONS: ReadonlyMap<string, boolean> = new Map([
  ["--arch", true],
  ["--file", true],
  ["-f", true],
  ["--format", true],
  ["--jobs", true],
  ["--label", true],
  ["--os", true],
  ["--platform", true],
  ["--secret", true],
  ["--tag", true],
  ["-t", true],
  ["--target", true],
  ["--timestamp", true],
  ["--force-rm", false],
  ["--layers", false],
  ["--no-cache", false],
  ["--pull", false],
  ["--quiet", false],
  ["-q", false],
  ["--rm", false],
]);

/**
 * The shape each option's value must have. Labels, the platform, the timestamp and the like
 * end up in the image configuration, so a value that could be anything could carry a secret
 * into the image; these shapes leave no room for one. `--file` is checked as a build file,
 * and `--secret` mounts a file only for RUN steps without storing it. Options that write
 * files on the host (`--iidfile`) are not allowed. Options without `=value` take no value.
 */
const BOOLEAN = /^(?:true|false)$/;
/**
 * The tags the builds give their images: the dev environment's
 * `localhost/trustiot-dev-<instance>-<service>:dev` and the release build's
 * `localhost/trustiot-release/<image>:<version>` (Laysi/trustiot-v2#68), and the test doubles'
 * `localhost/trustiot-e2e/<image>:<tag>` (the nightly's identity double). Nothing is pushed
 * under these names; release images are pushed by digest from an OCI archive.
 */
const IMAGE_TAG =
  /^localhost\/(?:trustiot-dev-[a-z][a-z0-9-]{0,23}-[a-z][a-z0-9-]{0,40}:dev|trustiot-release\/[a-z][a-z0-9-]{0,63}:\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?|trustiot-e2e\/[a-z][a-z0-9-]{0,63}:[a-z0-9][a-z0-9.-]{0,31})$/;

/**
 * Whether a value holds a run of 20 or more letters and digits mixed, between separators: the
 * shape of a token or key, which no tag, label or name of the builds has. The commit SHA in
 * the revision label is the one exception, checked by its own shape.
 */
function secretShaped(value: string): boolean {
  return value.split(/[-_./:=,@+]/).some((run) => run.length >= 20 && /[A-Za-z]/.test(run) && /\d/.test(run));
}
const VALUE_SHAPES: ReadonlyMap<string, RegExp> = new Map([
  ["--arch", /^[a-z0-9_]+$/],
  ["--os", /^[a-z0-9_]+$/],
  ["--format", /^(?:oci|docker)$/],
  ["--jobs", /^\d{1,3}$/],
  ["--platform", /^[a-z0-9_]+\/[a-z0-9_]+(?:\/v\d)?$/],
  ["--tag", IMAGE_TAG],
  ["-t", IMAGE_TAG],
  ["--target", /^[A-Za-z0-9_.-]{1,64}$/],
  ["--timestamp", /^\d{1,12}$/],
  ["--force-rm", BOOLEAN],
  ["--layers", BOOLEAN],
  ["--no-cache", BOOLEAN],
  ["--pull", /^(?:true|false|always|missing|never|newer)$/],
  ["--quiet", BOOLEAN],
  ["-q", BOOLEAN],
  ["--rm", BOOLEAN],
]);

/**
 * The labels a build may set, each with the shape of its value: the OCI labels of the release
 * images (Laysi/trustiot-v2#68) and the dev environment's instance label.
 */
const LABEL_SHAPES: ReadonlyMap<string, RegExp> = new Map([
  ["org.opencontainers.image.source", /^https:\/\/github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/],
  ["org.opencontainers.image.revision", /^[0-9a-f]{40}$/],
  ["org.opencontainers.image.version", /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?$/],
  ["org.opencontainers.image.title", /^[a-z0-9][a-z0-9-]{0,63}$/],
  [LABEL_KEY, INSTANCE_PATTERN],
]);

function valueProblem(name: string, value: string): string | undefined {
  if (name === "--label") {
    const equals = value.indexOf("=");
    const key = equals < 0 ? value : value.slice(0, equals);
    const shape = LABEL_SHAPES.get(key);
    if (shape === undefined) {
      return `label ${key} is not one the builds set (${[...LABEL_SHAPES.keys()].join(", ")}); labels stay in the image configuration`;
    }
    const labelValue = value.slice(equals + 1);
    if (equals < 0 || !shape.test(labelValue) || (key !== "org.opencontainers.image.revision" && secretShaped(labelValue))) {
      return `label ${key} needs a value of the form ${shape}`;
    }
    return undefined;
  }
  const shape = VALUE_SHAPES.get(name);
  if (shape !== undefined && (!shape.test(value) || secretShaped(value))) {
    return `podman build option ${name} needs a value of the form ${shape}`;
  }
  return undefined;
}

/**
 * Throws unless every option is allowed and carries its value; returns the build file that
 * `--file` names, if any.
 */
function assertBuildOptions(options: readonly string[]): string | undefined {
  const files: string[] = [];
  for (let index = 0; index < options.length; index += 1) {
    const option = options[index] ?? "";
    const equals = option.indexOf("=");
    const name = option.startsWith("-") && equals > 0 ? option.slice(0, equals) : option;
    const takesValue = BUILD_OPTIONS.get(name);
    if (takesValue === undefined) {
      throw new Error(
        option.startsWith("-")
          ? `podman build option ${name} is not allowed: it can make the builder read files the checks never see (allowed: ${[...BUILD_OPTIONS.keys()].join(", ")}); see docs/architecture/secrets.md`
          : `"${option}" is not an option; containerBuildArgs adds the context itself, so options must not name another one`,
      );
    }
    let value: string | undefined;
    if (name !== option) {
      value = option.slice(equals + 1);
    } else if (takesValue) {
      index += 1;
      value = options[index];
      if (value === undefined) {
        throw new Error(`podman build option ${name} needs a value`);
      }
    }
    if (name === "--file" || name === "-f") {
      files.push(value ?? "");
    }
    const problem = value === undefined ? undefined : valueProblem(name, value);
    if (problem !== undefined) {
      throw new Error(problem);
    }
  }
  if (files.length > 1) {
    throw new Error("podman build takes one --file here, so that the file it reads is the file that was checked");
  }
  return files[0];
}

/**
 * Throws unless the build file podman will read (`file`, or the context's Containerfile or
 * Dockerfile) is a regular file that passes the same rules as the repository's build files,
 * applied to the actual context: no key or credential file copied, no secret ARG or ENV, a
 * whole-context copy only behind a deny-all ignore file. A generated or out-of-context build
 * file is therefore checked too.
 */
export function assertBuildFile(context: string, file: string | undefined): void {
  const path = file ?? ["Containerfile", "Dockerfile"].map((name) => join(context, name)).find((candidate) => isFile(candidate));
  if (path === undefined || !isFile(path)) {
    throw new Error(
      file === undefined
        ? `the build context ${context} has no Containerfile or Dockerfile`
        : `--file ${file} is not a regular file; podman must read a build file that can be checked (not stdin or a URL)`,
    );
  }
  const problems = checkBuildFile(context, "Containerfile", readFileSync(path, "utf8"));
  if (problems.length > 0) {
    throw new Error(`the build file ${path} breaks the secret rules: ${problems.map((problem) => `line ${problem.line}: ${problem.message}`).join("; ")}`);
  }
}

function isFile(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

/** `options` with the `--file` path made absolute; stdin (`-`) and URLs stay as they are and fail later. */
function absoluteFileOptions(options: readonly string[]): string[] {
  const absolute = (path: string) => (path === "-" || /^[a-z][a-z0-9+.-]*:\/\//i.test(path) ? path : resolve(path));
  const result: string[] = [];
  for (let index = 0; index < options.length; index += 1) {
    const option = options[index] ?? "";
    const inline = /^(--file|-f)=(.*)$/.exec(option);
    if (inline !== null) {
      result.push(`${inline[1]}=${absolute(inline[2] ?? "")}`);
    } else if (option === "--file" || option === "-f") {
      result.push(option, absolute(options[index + 1] ?? ""));
      index += 1;
    } else {
      result.push(option);
    }
  }
  return result;
}

/**
 * The arguments of `podman build` for `context`: checks the options, the context and the build
 * file first, then returns `["build", ...options, context]`, so the directory and the file
 * that were checked are the only ones podman builds from.
 */
export function containerBuildArgs(context: string, options: readonly string[]): readonly string[] {
  assertBuildOptions(options);
  // Absolute paths, so that podman reads the checked directory and file even when it runs in
  // another working directory.
  const absoluteContext = resolve(context);
  const absoluteOptions = absoluteFileOptions(options);
  const file = assertBuildOptions(absoluteOptions);
  assertBuildContext(absoluteContext);
  assertBuildFile(absoluteContext, file);
  const args = Object.freeze(["build", ...absoluteOptions, absoluteContext]);
  checkedBuilds.add(args);
  return args;
}

/** Command groups whose own subcommand can be `build` (`podman image build`, `docker buildx build`). */
const BUILD_GROUPS = new Set(["image", "buildx", "builder"]);
/** Subcommands that build images: `build`, buildah's `bud`, buildx's `bake`. */
const BUILD_VERBS = new Set(["build", "bud", "bake"]);

/**
 * Throws when `args` would make podman build an image and did not come from containerBuildArgs.
 * The podman runner calls it before every command, so a build is refused even when its
 * subcommand was computed at run time. Only the command position counts: `build` as the
 * subcommand, or as the subcommand of `image`, `buildx` or `builder`. A `build` later on, such
 * as the command a `podman exec` runs in a container, is not a podman build. Global options
 * before the subcommand are refused, because some take a value (`--log-level info build`)
 * and would hide where the subcommand starts; the scripts never use them.
 */
export function assertCheckedBuild(args: readonly string[]): void {
  if (checkedBuilds.has(args)) {
    return;
  }
  const [command = "", ...rest] = args;
  if (command.startsWith("-")) {
    throw new Error(`podman global option ${command} is not used by the scripts; put the subcommand first`);
  }
  if (command === "compose") {
    // `compose build`, and also `up`, `run` or `create`, build images from a compose file's
    // `build:` sections, whose contexts nothing here checks.
    throw new Error("podman compose can build images from a compose file without checking their contexts; the scripts do not use it");
  }
  if (BUILD_GROUPS.has(command) && (rest[0] ?? "").startsWith("-")) {
    // An option value could stand where the subcommand seems to be (`image --log-level info build`).
    throw new Error(`podman ${command} option ${rest[0]} is not used by the scripts; put the ${command} subcommand first`);
  }
  const subcommand = BUILD_GROUPS.has(command) ? rest[0] : command;
  if (BUILD_VERBS.has(subcommand ?? "")) {
    throw new Error(
      "this podman command builds an image without checking its context; take the arguments from containerBuildArgs(context, options) in scripts/lib/build-context.ts",
    );
  }
}
