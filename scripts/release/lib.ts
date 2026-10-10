/**
 * Shared logic of the release pipeline, `.github/workflows/release.yml` (see the release.yml
 * section of docs/architecture/ci-release.md).
 *
 * The functions here only parse, validate and render, so lib.test.ts covers them without
 * tools or network. The stage scripts (plan.ts, build.ts, publish.ts, verify.ts) call the
 * tools and pass data between jobs as the JSON types defined here.
 */

import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

/** Every image is `<REGISTRY>/<name>`; equal to the chart's `serviceDefaults.image.registry`. */
export const REGISTRY = "ghcr.io/laysi/trustiot-v2";
/** The Helm chart is `<CHART_REGISTRY>/<chart name>`, pushed as `oci://<CHART_REGISTRY>`. */
export const CHART_REGISTRY = `${REGISTRY}/charts`;
export const CHART_DIR = "deploy/chart";
export const IMAGES_DIR = "deploy/images";
/** The image template shared by every Rust binary; `@BIN@` stands for the binary name. */
export const RUST_CONTAINERFILE = `${IMAGES_DIR}/rust-binary.Containerfile`;
/** Images are statically linked musl binaries for arm64 (decision 5). */
export const RUST_TARGET = "aarch64-unknown-linux-musl";
export const IMAGE_PLATFORM = "linux/arm64";
export const WORKFLOW_PATH = ".github/workflows/release.yml";
/** The OIDC issuer in the signing certificate of a GitHub Actions job. */
export const GITHUB_OIDC_ISSUER = "https://token.actions.githubusercontent.com";
/** SBOM attestation predicate type for cosign (`cosign attest --type spdxjson`). */
export const SBOM_PREDICATE_TYPE = "https://spdx.dev/Document";

/** Names that cannot be image names because something else uses the path in the registry. */
const RESERVED_IMAGE_NAMES: ReadonlySet<string> = new Set(["charts"]);
/** Lowercase words joined by single hyphens, as used for the chart's service keys. */
const IMAGE_NAME = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const IMAGE_NAME_MAX = 63;
/** Cargo package and binary target names. */
const CARGO_NAME = /^[A-Za-z0-9_-]+$/;
const RELEASE_TAG =
  /^v(0|[1-9]\d{0,8})\.(0|[1-9]\d{0,8})\.(0|[1-9]\d{0,8})(-rc\.[1-9]\d{0,8})?$/;
const SHA256_DIGEST = /^sha256:[0-9a-f]{64}$/;
const COMMIT_SHA = /^[0-9a-f]{40}$/;
const REPOSITORY = /^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/;

/** An expected failure: the message says what is wrong and how to fix it. */
export class ReleaseError extends Error {
  override name = "ReleaseError";
}

type YamlMap = Record<string, unknown>;

export function isMap(value: unknown): value is YamlMap {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// ---------------------------------------------------------------------------------------------
// Tags and versions

/** A release tag and the version it publishes. */
export interface ReleaseVersion {
  /** The git tag, `v1.2.3` or `v1.2.3-rc.4`. */
  tag: string;
  /** The tag without `v`: chart version, chart appVersion and image tag. */
  version: string;
  /** `-rc.N` tags are prereleases: GitHub Release marked as prerelease, any branch allowed. */
  prerelease: boolean;
}

/** Parses a release tag; anything else that matches the workflow's `v*` filter is rejected. */
export function parseReleaseTag(tag: string): ReleaseVersion {
  const match = RELEASE_TAG.exec(tag);
  if (match === null) {
    throw new ReleaseError(
      `tag "${tag}" is not a release tag: use vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-rc.N, ` +
        "for example v1.2.0 or v1.2.0-rc.1",
    );
  }
  return { tag, version: tag.slice(1), prerelease: match[4] !== undefined };
}

/** Major, minor, patch and release candidate number; a final release sorts after its RCs. */
function precedence(tag: string): [number, number, number, number] {
  const match = RELEASE_TAG.exec(tag);
  if (match === null) {
    throw new ReleaseError(`tag "${tag}" is not a release tag`);
  }
  const rc = match[4] === undefined ? Number.POSITIVE_INFINITY : Number(match[4].slice("-rc.".length));
  return [Number(match[1]), Number(match[2]), Number(match[3]), rc];
}

/** Orders release tags by semantic version precedence: v1.0.0-rc.1 < v1.0.0-rc.2 < v1.0.0. */
export function compareReleaseTags(a: string, b: string): number {
  const left = precedence(a);
  const right = precedence(b);
  for (let index = 0; index < left.length; index += 1) {
    // Compared, not subtracted: a final release's RC number is Infinity, and Infinity - Infinity is NaN.
    const a = left[index] ?? 0;
    const b = right[index] ?? 0;
    if (a !== b) {
      return a < b ? -1 : 1;
    }
  }
  return 0;
}

/**
 * The release that the notes of `current` start from: the highest release tag below it. A
 * final release starts from the previous final release, so its notes cover its release
 * candidates too; a release candidate starts from the previous tag of either kind. Tags that
 * are not release tags are ignored. Returns undefined for the first release.
 */
export function previousReleaseTag(tags: readonly string[], current: ReleaseVersion): string | undefined {
  let previous: string | undefined;
  for (const tag of tags) {
    if (!RELEASE_TAG.test(tag) || compareReleaseTags(tag, current.tag) >= 0) {
      continue;
    }
    if (!current.prerelease && parseReleaseTag(tag).prerelease) {
      continue;
    }
    if (previous === undefined || compareReleaseTags(tag, previous) > 0) {
      previous = tag;
    }
  }
  return previous;
}

// ---------------------------------------------------------------------------------------------
// Image declarations under deploy/images/

/** A Rust binary of a workspace package, packed with the shared rust-binary Containerfile. */
export interface RustImage {
  kind: "rust";
  name: string;
  /** The Cargo package. */
  package: string;
  /** The binary target; when absent the package must have exactly one. */
  bin?: string;
}

/** An image with its own Containerfile; the build context is its directory. */
export interface ContainerfileImage {
  kind: "containerfile";
  name: string;
  /** The directory with the Containerfile, relative to the repository root. */
  context: string;
}

export type ImageSpec = RustImage | ContainerfileImage;

/** Returns why `name` cannot be an image name, or undefined. */
export function imageNameProblem(name: string): string | undefined {
  if (!IMAGE_NAME.test(name)) {
    return "use lowercase letters and digits joined by single hyphens, for example `field-context`";
  }
  if (name.length > IMAGE_NAME_MAX) {
    return `use at most ${IMAGE_NAME_MAX} characters`;
  }
  if (RESERVED_IMAGE_NAMES.has(name)) {
    return `\`${name}\` is reserved: ${CHART_REGISTRY} holds the Helm chart`;
  }
  return undefined;
}

/**
 * Parses `deploy/images/<name>/image.toml`, which declares a Rust image:
 *
 *     [rust]
 *     package = "identity"   # the Cargo package
 *     bin = "identity"       # optional when the package has one binary target
 */
export function parseImageToml(name: string, source: string, file: string): RustImage {
  let doc: unknown;
  try {
    doc = Bun.TOML.parse(source);
  } catch (error) {
    throw new ReleaseError(`${file}: cannot parse TOML: ${errorMessage(error)}`);
  }
  if (!isMap(doc)) {
    throw new ReleaseError(`${file}: expected a TOML table`);
  }
  const extraTop = Object.keys(doc).filter((key) => key !== "rust");
  if (extraTop.length > 0) {
    throw new ReleaseError(
      `${file}: unknown key(s) ${extraTop.join(", ")}; the file holds one [rust] table`,
    );
  }
  const rust = doc.rust;
  if (!isMap(rust)) {
    throw new ReleaseError(`${file}: missing the [rust] table with package = "<Cargo package>"`);
  }
  const extra = Object.keys(rust).filter((key) => key !== "package" && key !== "bin");
  if (extra.length > 0) {
    throw new ReleaseError(
      `${file}: unknown key(s) ${extra.join(", ")} in [rust]; allowed: package, bin`,
    );
  }
  const pkg = rust.package;
  if (typeof pkg !== "string" || !CARGO_NAME.test(pkg)) {
    throw new ReleaseError(`${file}: [rust] package must be a Cargo package name`);
  }
  const image: RustImage = { kind: "rust", name, package: pkg };
  if (rust.bin !== undefined) {
    if (typeof rust.bin !== "string" || !CARGO_NAME.test(rust.bin)) {
      throw new ReleaseError(`${file}: [rust] bin must be a Cargo binary target name`);
    }
    image.bin = rust.bin;
  }
  return image;
}

/**
 * Lists the images a release publishes: every directory `deploy/images/<name>/` holds either
 * `image.toml` (a Rust binary) or a `Containerfile` (built from that directory). Files directly
 * under deploy/images/, such as the shared rust-binary.Containerfile, are not images.
 */
export function discoverImages(root: string): ImageSpec[] {
  const dir = join(root, IMAGES_DIR);
  if (!existsSync(dir)) {
    throw new ReleaseError(`${IMAGES_DIR} does not exist`);
  }
  const images: ImageSpec[] = [];
  for (const name of readdirSync(dir).sort()) {
    const path = join(dir, name);
    if (!statSync(path).isDirectory()) {
      continue;
    }
    const where = `${IMAGES_DIR}/${name}`;
    const problem = imageNameProblem(name);
    if (problem !== undefined) {
      throw new ReleaseError(`${where}: the directory name is the image name; ${problem}`);
    }
    const toml = join(path, "image.toml");
    const containerfile = join(path, "Containerfile");
    const hasToml = existsSync(toml);
    const hasContainerfile = existsSync(containerfile);
    if (hasToml && hasContainerfile) {
      throw new ReleaseError(
        `${where}: has both image.toml and Containerfile; keep image.toml for a Rust binary ` +
          "or Containerfile for a custom image",
      );
    }
    if (hasToml) {
      images.push(parseImageToml(name, readFileSync(toml, "utf8"), `${where}/image.toml`));
    } else if (hasContainerfile) {
      const problems = baseImageProblems(readFileSync(containerfile, "utf8"));
      if (problems.length > 0) {
        throw new ReleaseError(`${where}/Containerfile: ${problems.join("; ")}`);
      }
      images.push({ kind: "containerfile", name, context: where });
    } else {
      throw new ReleaseError(
        `${where}: add image.toml (a Rust binary) or a Containerfile, or remove the directory`,
      );
    }
  }
  return images;
}

/** One workspace package from `cargo metadata --no-deps --format-version 1`. */
export interface CargoPackage {
  name: string;
  targets: { name: string; kind: string[] }[];
}

/** Returns the binary target a Rust image packs, checked against the workspace. */
export function resolveBinary(image: RustImage, packages: readonly CargoPackage[]): string {
  const where = `${IMAGES_DIR}/${image.name}/image.toml`;
  const pkg = packages.find((candidate) => candidate.name === image.package);
  if (pkg === undefined) {
    throw new ReleaseError(`${where}: package \`${image.package}\` is not a workspace member`);
  }
  const bins = pkg.targets.filter((target) => target.kind.includes("bin")).map((t) => t.name);
  if (image.bin !== undefined) {
    if (!bins.includes(image.bin)) {
      throw new ReleaseError(
        `${where}: package \`${pkg.name}\` has no binary \`${image.bin}\` ` +
          `(binaries: ${bins.join(", ") || "none"})`,
      );
    }
    return image.bin;
  }
  const [only, ...rest] = bins;
  if (only === undefined || rest.length > 0) {
    throw new ReleaseError(
      `${where}: package \`${pkg.name}\` has ${bins.length} binaries ` +
        `(${bins.join(", ") || "none"}); set bin = "<name>" in [rust]`,
    );
  }
  return only;
}

/** `e_machine` of aarch64 in an ELF header. */
const ELF_MACHINE_AARCH64 = 183;

/** Returns why `bytes` is not a 64-bit little-endian aarch64 ELF file, or undefined. */
export function aarch64ElfProblem(bytes: Uint8Array): string | undefined {
  const byte = (index: number) => bytes[index] ?? -1;
  if (byte(0) !== 0x7f || byte(1) !== 0x45 || byte(2) !== 0x4c || byte(3) !== 0x46) {
    return "it is not an ELF file";
  }
  if (byte(4) !== 2 || byte(5) !== 1) {
    return "it is not a 64-bit little-endian ELF file";
  }
  const machine = byte(18) + byte(19) * 256;
  if (machine !== ELF_MACHINE_AARCH64) {
    return `its ELF machine is ${machine}, expected ${ELF_MACHINE_AARCH64} (aarch64)`;
  }
  return undefined;
}

/**
 * Returns why the `FROM` lines of a Containerfile do not pin their base images, or an empty
 * list. Every base image must be a literal reference with a sha256 digest (keep the tag in
 * front of it for readers); `scratch` and earlier build stages need none.
 */
export function baseImageProblems(containerfile: string): string[] {
  const problems: string[] = [];
  /** Stages named by earlier FROM lines. */
  const stages = new Set<string>();
  let fromLines = 0;
  // Continuation lines belong to the instruction above them.
  for (const line of containerfile.replace(/\\\r?\n/g, " ").split(/\r?\n/)) {
    const words = line.trim().split(/\s+/);
    if (words[0]?.toUpperCase() !== "FROM") {
      continue;
    }
    fromLines += 1;
    const args = words.slice(1).filter((word) => !word.startsWith("--"));
    const image = args[0] ?? "";
    if (image === "") {
      problems.push("a FROM line names no image");
    } else if (image.includes("$")) {
      problems.push(`\`FROM ${image}\` uses a variable; write the image reference with its digest`);
    } else if (image !== "scratch" && !stages.has(image.toLowerCase()) && !/@sha256:[0-9a-f]{64}$/.test(image)) {
      problems.push(`\`FROM ${image}\` is not pinned to a digest; use <image>:<tag>@sha256:<digest>`);
    }
    if (args[1]?.toUpperCase() === "AS" && args[2] !== undefined) {
      stages.add(args[2].toLowerCase());
    }
  }
  if (fromLines === 0) {
    problems.push("there is no FROM line");
  }
  return problems;
}

/** Writes the binary name into the shared Rust image template. */
export function renderRustContainerfile(template: string, bin: string): string {
  if (!CARGO_NAME.test(bin)) {
    throw new ReleaseError(`\`${bin}\` is not a Cargo binary name`);
  }
  if (!template.includes("@BIN@")) {
    throw new ReleaseError(`${RUST_CONTAINERFILE} has no @BIN@ placeholder for the binary name`);
  }
  const problems = baseImageProblems(template);
  if (problems.length > 0) {
    throw new ReleaseError(`${RUST_CONTAINERFILE}: ${problems.join("; ")}`);
  }
  return template.replaceAll("@BIN@", bin);
}

// ---------------------------------------------------------------------------------------------
// The plan that every job of the workflow receives

export interface PlannedImage {
  name: string;
  kind: ImageSpec["kind"];
  /** Rust images: the Cargo package. */
  package?: string;
  /** Rust images: the binary target, when image.toml names one. */
  bin?: string;
  /** Containerfile images: the build context. */
  context?: string;
  /** `<REGISTRY>/<name>`, without tag. */
  repository: string;
  /** `<repository>:<version>`. */
  ref: string;
}

export interface ReleasePlan {
  tag: string;
  version: string;
  prerelease: boolean;
  /** The full SHA of the tagged commit. */
  commit: string;
  /** The commit time in seconds; timestamps inside the images, for reproducible builds. */
  sourceDateEpoch: number;
  /** `owner/name` as GitHub spells it; part of the signing identity. */
  repository: string;
  images: PlannedImage[];
  chart: { name: string; repository: string; version: string };
}

/** The Rust declaration behind a planned Rust image. */
export function rustSpec(image: PlannedImage): RustImage {
  if (image.kind !== "rust" || image.package === undefined) {
    throw new ReleaseError(`${image.name} is not a Rust image`);
  }
  return {
    kind: "rust",
    name: image.name,
    package: image.package,
    ...(image.bin === undefined ? {} : { bin: image.bin }),
  };
}

export function planImage(spec: ImageSpec, version: string, registry = REGISTRY): PlannedImage {
  const repository = `${registry}/${spec.name}`;
  const planned: PlannedImage = {
    name: spec.name,
    kind: spec.kind,
    repository,
    ref: `${repository}:${version}`,
  };
  if (spec.kind === "rust") {
    planned.package = spec.package;
    if (spec.bin !== undefined) {
      planned.bin = spec.bin;
    }
  } else {
    planned.context = spec.context;
  }
  return planned;
}

/** Validates a plan read back from a job output or a file. */
export function parsePlan(text: string): ReleasePlan {
  let doc: unknown;
  try {
    doc = JSON.parse(text);
  } catch (error) {
    throw new ReleaseError(`the release plan is not JSON: ${errorMessage(error)}`);
  }
  if (!isMap(doc)) {
    throw new ReleaseError("the release plan must be a JSON object");
  }
  const version = parseReleaseTag(requireString(doc, "tag"));
  if (doc.version !== version.version || doc.prerelease !== version.prerelease) {
    throw new ReleaseError("the release plan's version does not match its tag");
  }
  const commit = requireString(doc, "commit");
  if (!COMMIT_SHA.test(commit)) {
    throw new ReleaseError("the release plan's commit is not a full commit SHA");
  }
  const sourceDateEpoch = doc.sourceDateEpoch;
  if (typeof sourceDateEpoch !== "number" || !Number.isSafeInteger(sourceDateEpoch)) {
    throw new ReleaseError("the release plan's sourceDateEpoch is not an integer");
  }
  const repository = requireString(doc, "repository");
  if (!REPOSITORY.test(repository)) {
    throw new ReleaseError("the release plan's repository is not owner/name");
  }
  if (!Array.isArray(doc.images)) {
    throw new ReleaseError("the release plan has no image list");
  }
  const names = new Set<string>();
  const images = doc.images.map((value: unknown): PlannedImage => {
    if (!isMap(value)) {
      throw new ReleaseError("a planned image must be an object");
    }
    const name = requireString(value, "name");
    const problem = imageNameProblem(name);
    if (problem !== undefined || names.has(name)) {
      throw new ReleaseError(`planned image name \`${name}\` is invalid or repeated`);
    }
    names.add(name);
    const repositoryRef = requireString(value, "repository");
    const ref = requireString(value, "ref");
    if (!repositoryRef.endsWith(`/${name}`) || ref !== `${repositoryRef}:${version.version}`) {
      throw new ReleaseError(`planned image \`${name}\` has an inconsistent reference`);
    }
    const kind = value.kind;
    if (kind === "rust") {
      const pkg = requireString(value, "package");
      const bin = value.bin;
      if (!CARGO_NAME.test(pkg) || (bin !== undefined && (typeof bin !== "string" || !CARGO_NAME.test(bin)))) {
        throw new ReleaseError(`planned image \`${name}\` has an invalid package or bin`);
      }
      return { name, kind, package: pkg, ...(bin === undefined ? {} : { bin }), repository: repositoryRef, ref };
    }
    if (kind === "containerfile") {
      const context = requireString(value, "context");
      if (context !== `${IMAGES_DIR}/${name}`) {
        throw new ReleaseError(`planned image \`${name}\` has an unexpected build context`);
      }
      return { name, kind, context, repository: repositoryRef, ref };
    }
    throw new ReleaseError(`planned image \`${name}\` has an unknown kind`);
  });
  const chart = doc.chart;
  if (!isMap(chart)) {
    throw new ReleaseError("the release plan has no chart");
  }
  const chartName = requireString(chart, "name");
  const chartRepository = requireString(chart, "repository");
  if (!IMAGE_NAME.test(chartName) || !chartRepository.endsWith(`/charts/${chartName}`)) {
    throw new ReleaseError("the release plan's chart reference is invalid");
  }
  if (chart.version !== version.version) {
    throw new ReleaseError("the release plan's chart version does not match its tag");
  }
  return {
    ...version,
    commit,
    sourceDateEpoch,
    repository,
    images,
    chart: { name: chartName, repository: chartRepository, version: version.version },
  };
}

function requireString(map: YamlMap, key: string): string {
  const value = map[key];
  if (typeof value !== "string" || value === "") {
    throw new ReleaseError(`the release plan has no ${key}`);
  }
  return value;
}

/**
 * Throws unless every image and the chart go to `registry`. A release signed in GitHub Actions
 * publishes only to REGISTRY; other registries are for local tests (plan.ts --registry).
 */
export function requireRegistry(plan: ReleasePlan, registry: string = REGISTRY): void {
  const repositories = [...plan.images.map((image) => image.repository), plan.chart.repository];
  const outside = repositories.filter((repository) => !repository.startsWith(`${registry}/`));
  if (outside.length > 0) {
    throw new ReleaseError(`releases publish only to ${registry}, but the plan names ${outside.join(", ")}`);
  }
}

/** A registry on this machine: localhost, 127.0.0.1 or [::1], with an optional port. */
const LOOPBACK_HOST = /^(?:localhost|127\.0\.0\.1|\[::1\])(?::\d{1,5})?$/;

/**
 * Throws unless every image and the chart go to a registry on this machine. Local test mode
 * (signed with a throwaway key) must never publish to GHCR, where it would take a release's
 * version tags.
 */
export function requireLoopbackRegistry(plan: ReleasePlan): void {
  const repositories = [...plan.images.map((image) => image.repository), plan.chart.repository];
  const remote = repositories.filter((repository) => !LOOPBACK_HOST.test(repository.split("/")[0] ?? ""));
  if (remote.length > 0) {
    throw new ReleaseError(
      `local tests publish only to a registry on this machine (localhost or 127.0.0.1), but the plan names ${remote.join(", ")}; ` +
        "plan with --dry-run --registry 127.0.0.1:<port>/laysi/trustiot-v2",
    );
  }
}

// ---------------------------------------------------------------------------------------------
// Signing identity

/**
 * The identity in the signing certificate of release.yml running for `tag`. Consumers verify
 * with exactly this identity and GITHUB_OIDC_ISSUER.
 */
export function signingIdentity(repository: string, tag: string): string {
  return `https://github.com/${repository}/${WORKFLOW_PATH}@refs/tags/${tag}`;
}

/** cosign verify flags that pin a signature to release.yml at this tag and commit. */
export function keylessVerifyArgs(plan: ReleasePlan): string[] {
  return [
    "--certificate-identity",
    signingIdentity(plan.repository, plan.tag),
    "--certificate-oidc-issuer",
    GITHUB_OIDC_ISSUER,
    "--certificate-github-workflow-repository",
    plan.repository,
    "--certificate-github-workflow-ref",
    `refs/tags/${plan.tag}`,
    "--certificate-github-workflow-sha",
    plan.commit,
    "--certificate-github-workflow-trigger",
    "push",
  ];
}

// ---------------------------------------------------------------------------------------------
// Registry content checks

export function sha256Digest(bytes: string | Uint8Array): string {
  const hasher = new Bun.CryptoHasher("sha256");
  hasher.update(bytes);
  return `sha256:${hasher.digest("hex")}`;
}

export function isDigest(value: unknown): value is string {
  return typeof value === "string" && SHA256_DIGEST.test(value);
}

const IMAGE_MANIFEST_TYPES: ReadonlySet<string> = new Set([
  "application/vnd.oci.image.manifest.v1+json",
  "application/vnd.docker.distribution.manifest.v2+json",
]);

/**
 * Returns what is wrong with an image's manifest and config for this release: it must be one
 * linux/arm64 image, not an index (decision 5: images are arm64 only).
 */
export function imagePlatformProblems(manifest: unknown, config: unknown): string[] {
  const problems: string[] = [];
  if (!isMap(manifest)) {
    return ["the manifest is not a JSON object"];
  }
  const mediaType = manifest.mediaType;
  if (typeof mediaType !== "string" || !IMAGE_MANIFEST_TYPES.has(mediaType)) {
    problems.push(
      `the manifest is ${String(mediaType)}, expected a single-platform image manifest`,
    );
  }
  if (!isMap(config)) {
    problems.push("the image config is not a JSON object");
    return problems;
  }
  if (config.os !== "linux" || config.architecture !== "arm64") {
    problems.push(
      `the image is ${String(config.os)}/${String(config.architecture)}, expected ${IMAGE_PLATFORM}`,
    );
  }
  return problems;
}

/** Summarizes an SPDX JSON SBOM; throws when it is not one or lists no package. */
export function summarizeSpdx(doc: unknown): { spdxVersion: string; packages: string[] } {
  if (!isMap(doc) || typeof doc.spdxVersion !== "string" || !doc.spdxVersion.startsWith("SPDX-")) {
    throw new ReleaseError("the SBOM is not an SPDX JSON document");
  }
  if (!Array.isArray(doc.packages) || doc.packages.length === 0) {
    throw new ReleaseError("the SBOM lists no packages");
  }
  const packages = doc.packages.map((pkg: unknown) => {
    if (!isMap(pkg) || typeof pkg.name !== "string") {
      throw new ReleaseError("an SBOM package has no name");
    }
    return typeof pkg.versionInfo === "string" ? `${pkg.name}@${pkg.versionInfo}` : pkg.name;
  });
  return { spdxVersion: doc.spdxVersion, packages };
}

/** Reads the `digest` line that `helm push` prints. */
export function helmPushDigest(output: string): string {
  const match = /^Digest:\s*(sha256:[0-9a-f]{64})\s*$/m.exec(output);
  if (match?.[1] === undefined) {
    throw new ReleaseError("`helm push` did not print the chart digest");
  }
  return match[1];
}

// ---------------------------------------------------------------------------------------------
// What publish hands to verify, and the GitHub Release text

export interface PublishedArtifact {
  /** Image name, or the chart name. */
  name: string;
  /** Tagged reference, `<repository>:<version>`. */
  ref: string;
  digest: string;
}

export interface PublishedRelease {
  images: (PublishedArtifact & { sbomAsset: string })[];
  chart: PublishedArtifact;
  releaseUrl: string;
}

export function parsePublished(text: string): PublishedRelease {
  let doc: unknown;
  try {
    doc = JSON.parse(text);
  } catch (error) {
    throw new ReleaseError(`the publish result is not JSON: ${errorMessage(error)}`);
  }
  if (!isMap(doc) || !Array.isArray(doc.images) || !isMap(doc.chart)) {
    throw new ReleaseError("the publish result has no images or chart");
  }
  const artifact = (value: unknown): PublishedArtifact => {
    if (!isMap(value) || typeof value.name !== "string" || typeof value.ref !== "string" || !isDigest(value.digest)) {
      throw new ReleaseError("a published artifact has no name, reference or digest");
    }
    return { name: value.name, ref: value.ref, digest: value.digest };
  };
  const images = doc.images.map((value: unknown) => {
    const image = artifact(value);
    const sbomAsset = isMap(value) ? value.sbomAsset : undefined;
    if (typeof sbomAsset !== "string" || sbomAsset === "") {
      throw new ReleaseError(`published image ${image.name} has no SBOM asset`);
    }
    return { ...image, sbomAsset };
  });
  const releaseUrl = typeof doc.releaseUrl === "string" ? doc.releaseUrl : "";
  return { images, chart: artifact(doc.chart), releaseUrl };
}

/**
 * Throws unless a publish result names exactly the planned images and chart. verify.ts calls it
 * before it uses any credential, so a forged result cannot point a registry login elsewhere.
 */
export function requirePublishedAsPlanned(plan: ReleasePlan, published: Omit<PublishedRelease, "releaseUrl">): void {
  const planned = new Map(plan.images.map((image) => [image.name, image.ref]));
  const names = published.images.map((image) => image.name);
  if (names.length !== planned.size || new Set(names).size !== names.length) {
    throw new ReleaseError("the published images differ from the planned ones");
  }
  for (const image of published.images) {
    const ref = planned.get(image.name);
    if (ref === undefined || image.ref !== ref) {
      throw new ReleaseError(`published image ${image.name} (${image.ref}) is not the planned ${ref ?? "image"}`);
    }
  }
  const chart = `${plan.chart.repository}:${plan.version}`;
  if (published.chart.name !== plan.chart.name || published.chart.ref !== chart) {
    throw new ReleaseError(`the published chart ${published.chart.ref} is not the planned ${chart}`);
  }
}

/** The SBOM file name attached to the GitHub Release. */
export function sbomAssetName(imageName: string, version: string): string {
  return `${imageName}-${version}.spdx.json`;
}

/** The release page: the notes, then what was published and how to verify it (zh-TW). */
export function renderReleaseBody(
  notes: string,
  plan: ReleasePlan,
  published: Omit<PublishedRelease, "releaseUrl">,
): string {
  const identity = signingIdentity(plan.repository, plan.tag);
  const rows = published.images.map(
    (image) => `| 映像 \`${image.name}\` | \`${image.ref}\` | \`${image.digest}\` |`,
  );
  rows.push(
    `| Helm chart \`${published.chart.name}\` | \`oci://${plan.chart.repository}\` 版本 \`${plan.version}\` | \`${published.chart.digest}\` |`,
  );
  const example = published.images[0]?.ref ?? `${REGISTRY}/<映像>:${plan.version}`;
  const flags = `--certificate-identity '${identity}' \\\n  --certificate-oidc-issuer ${GITHUB_OIDC_ISSUER}`;
  return [
    notes.trim(),
    "",
    "## 發佈內容",
    "",
    `映像只有 ${IMAGE_PLATFORM}；chart 的 \`appVersion\` 與映像的 tag 都是 \`${plan.version}\`，來源 commit 為 \`${plan.commit}\`。部署時以 digest 為準。`,
    "",
    "| 產物 | 參照 | digest |",
    "| --- | --- | --- |",
    ...rows,
    "",
    "每個映像的 SBOM（SPDX JSON）附在本頁的 assets，也以簽章過的 attestation 存在 registry。",
    "",
    "## 驗證",
    "",
    "以 GitHub OIDC 的 keyless cosign 簽章，簽章身分是這個 tag 上的 release workflow。套件是私有的，先以有讀取權限的帳號 `cosign login ghcr.io`：",
    "",
    "```sh",
    `cosign verify ${example} \\\n  ${flags}`,
    `cosign verify ${plan.chart.repository}:${plan.version} \\\n  ${flags}`,
    "# SBOM：驗證 attestation 後取出 predicate",
    `cosign verify-attestation --type spdxjson ${example} \\\n  ${flags} \\\n  | jq -r .payload | base64 -d | jq .predicate`,
    "```",
    "",
  ].join("\n");
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
