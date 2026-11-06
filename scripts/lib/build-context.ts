// Build-time guard for container build contexts (docs/architecture/secrets.md, rule 2).
// The static check (scripts/ci/check-secret-handling.ts) cannot know which directory a build
// command passes as the context, so every image build in the scripts takes its podman
// arguments from containerBuildArgs: it checks the very directory that podman then builds.
// The static check rejects container builds written anywhere else, and the podman runner
// (scripts/lib/podman.ts) refuses at run time a build whose arguments did not come from here.
import { readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";
import { checkBuildFile, parseBuildFile, secretFilesInContext } from "../ci/check-secret-handling.ts";
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
 * some options make the builder read more than the checked context: `--volume` mounts a
 * directory into RUN steps, `--ignorefile` swaps the ignore file, `--build-arg-file` reads
 * build arguments from a file the checks never see. `--build-context` is allowed only in the
 * one shape NAMED_CONTEXT describes, a stage of the checked build file replaced by a local
 * image the nightly built or loaded, never a directory (assertNamedContexts).
 * `--build-arg` is refused too: its value stays in the image history, and a value computed at
 * run time cannot be shown not to be a secret. Build-time secrets go through `--secret`.
 */
const BUILD_OPTIONS: ReadonlyMap<string, boolean> = new Map([
  ["--arch", true],
  ["--build-context", true],
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
 * The one `--build-context` a build may pass (Laysi/trustiot-v2#1127): `<stage>=docker-image://`
 * and a local image of the nightly's `localhost/trustiot-e2e/<image>:<tag>` names, the same
 * shape IMAGE_TAG accepts for them. It replaces the build file's stage `<stage>` with that image
 * (https://docs.docker.com/reference/cli/docker/buildx/build/#build-context: the context
 * "overrides a stage called `name` when used as `FROM ... AS name`"), so the builder reads an
 * image, never a directory of the host. A local directory, `oci-layout://`, a Git or HTTP URL,
 * `target:`, an image of any registry (`localhost:5000/...` included), a digest, uppercase or
 * whitespace do not fit. The name must be a stage of the checked build file
 * (assertNamedContexts).
 */
const NAMED_CONTEXT = /^[a-z][a-z0-9-]{0,63}=docker-image:\/\/localhost\/trustiot-e2e\/[a-z][a-z0-9-]{0,63}:[a-z0-9][a-z0-9.-]{0,31}$/;

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
  ["--build-context", NAMED_CONTEXT],
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

/** What the options name besides themselves: the build file and the named build contexts. */
interface CheckedOptions {
  /** The build file that `--file` names, if any. */
  file: string | undefined;
  /** The name of each `--build-context` (the stage it replaces), in order. */
  contexts: string[];
  /** Whether a `--pull` makes the builder ask the registry even for an image it holds. */
  pulls: boolean;
}

/**
 * The `--pull` values that ask the registry even when the builder holds the image: a bare
 * `--pull` means always, for podman and docker alike. `false`, `missing` and `never` use a local
 * image, as the default does.
 */
const PULLING = /^(?:true|always|newer)$/;

/**
 * Throws unless every option is allowed and carries its value; returns the build file that
 * `--file` names, if any, the names of the build contexts and whether a `--pull` pulls.
 */
function assertBuildOptions(options: readonly string[]): CheckedOptions {
  const files: string[] = [];
  const contexts: string[] = [];
  let pulls = false;
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
    if (name === "--build-context") {
      // The shape checked above has exactly one `=` before the image, after the name.
      contexts.push((value ?? "").slice(0, (value ?? "").indexOf("=")));
    }
    if (name === "--pull" && (value === undefined || PULLING.test(value))) {
      pulls = true;
    }
  }
  if (files.length > 1) {
    throw new Error("podman build takes one --file here, so that the file it reads is the file that was checked");
  }
  return { file: files[0], contexts, pulls };
}

/** The stage names a build file defines (`FROM ... AS <name>`), lower-cased as the builder reads them. */
export function buildFileStages(text: string): string[] {
  return parseBuildFile(text)
    .filter((instruction) => instruction.keyword === "FROM")
    .map((instruction) => /\sAS\s+(\S+)\s*$/i.exec(` ${instruction.args}`)?.[1]?.toLowerCase())
    .filter((stage): stage is string => stage !== undefined);
}

/**
 * The names under which the builder itself reads the build context and the build file, which a
 * named context could stand in for.
 */
const RESERVED_CONTEXT_NAMES: ReadonlySet<string> = new Set(["context", "dockerfile"]);
/**
 * A `syntax` parser directive in either comment form BuildKit reads, `# syntax=` and
 * `// syntax=` (frontend/dockerfile/parser/directives.go, DetectSyntax), anywhere in the file and
 * with any blank between the parts: wider than BuildKit, which reads only the top of the file. A
 * named context of the directive's name would replace the frontend, which the builder then runs.
 */
const SYNTAX_DIRECTIVE = /^[^\S\n]*(?:#|\/\/)[^\S\n]*syntax[^\S\n]*=/im;

/**
 * Whether the build file is a JSON object after an optional `#!` line, the third form in which
 * BuildKit reads a `syntax` directive (`{"syntax": "..."}`, DetectSyntax). No build file of a
 * stage is one, so any JSON object counts.
 */
function isJsonDirectiveFile(text: string): boolean {
  const body = text.replace(/^﻿/, "");
  const withoutShebang = body.startsWith("#!") ? body.slice(body.indexOf("\n") + 1 || body.length) : body;
  try {
    const parsed: unknown = JSON.parse(withoutShebang);
    return typeof parsed === "object" && parsed !== null;
  } catch {
    return false;
  }
}

/**
 * Throws unless each named context (`--build-context <name>=...`) replaces a stage that the
 * checked build file `text` defines (`FROM ... AS <name>`), once: then the builder reads the
 * named image only where that stage would have been used. A build file with a `syntax`
 * directive takes no named context, and neither do the names of the context and the build file.
 * Nor does a build that pulls (`--pull`, `--pull=always`): the builder would then ask a registry
 * named `localhost` for the image instead of using the local one.
 */
function assertNamedContexts(path: string, text: string, contexts: readonly string[], pulls: boolean): void {
  if (contexts.length === 0) {
    return;
  }
  if (SYNTAX_DIRECTIVE.test(text) || isJsonDirectiveFile(text)) {
    throw new Error(`the build file ${path} has a syntax directive, so it takes no --build-context: a context of the frontend's name would replace the frontend`);
  }
  if (pulls) {
    throw new Error("--build-context takes no --pull that pulls (bare, true, always or newer): the named image is a local one, which a pull would ask a registry named localhost for");
  }
  const stages = buildFileStages(text);
  const seen = new Set<string>();
  for (const name of contexts) {
    if (RESERVED_CONTEXT_NAMES.has(name)) {
      throw new Error(`--build-context ${name} would stand in for the ${name} the builder reads; only a stage of the build file may be replaced`);
    }
    if (seen.has(name)) {
      throw new Error(`--build-context ${name} is given twice; one image replaces a stage`);
    }
    seen.add(name);
    if (!stages.includes(name)) {
      throw new Error(
        `--build-context ${name} replaces no stage of the build file ${path} (stages: ${stages.join(", ") || "none"}); a named context may only replace a stage of the checked build file`,
      );
    }
  }
}

/**
 * Throws unless the build file podman will read (`file`, or the context's Containerfile or
 * Dockerfile) is a regular file that passes the same rules as the repository's build files,
 * applied to the actual context: no key or credential file copied, no secret ARG or ENV, a
 * whole-context copy only behind a deny-all ignore file. A generated or out-of-context build
 * file is therefore checked too. Returns the path and the text that were checked.
 */
export function assertBuildFile(context: string, file: string | undefined): { path: string; text: string } {
  const path = file ?? ["Containerfile", "Dockerfile"].map((name) => join(context, name)).find((candidate) => isFile(candidate));
  if (path === undefined || !isFile(path)) {
    throw new Error(
      file === undefined
        ? `the build context ${context} has no Containerfile or Dockerfile`
        : `--file ${file} is not a regular file; podman must read a build file that can be checked (not stdin or a URL)`,
    );
  }
  const text = readFileSync(path, "utf8");
  const problems = checkBuildFile(context, "Containerfile", text);
  if (problems.length > 0) {
    throw new Error(`the build file ${path} breaks the secret rules: ${problems.map((problem) => `line ${problem.line}: ${problem.message}`).join("; ")}`);
  }
  return { path, text };
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
 * that were checked are the only ones podman builds from. A named build context may only
 * replace a stage of that build file with a local e2e image (NAMED_CONTEXT).
 */
export function containerBuildArgs(context: string, options: readonly string[]): readonly string[] {
  assertBuildOptions(options);
  // Absolute paths, so that podman reads the checked directory and file even when it runs in
  // another working directory.
  const absoluteContext = resolve(context);
  const absoluteOptions = absoluteFileOptions(options);
  const { file, contexts, pulls } = assertBuildOptions(absoluteOptions);
  assertBuildContext(absoluteContext);
  const checked = assertBuildFile(absoluteContext, file);
  assertNamedContexts(checked.path, checked.text, contexts, pulls);
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
