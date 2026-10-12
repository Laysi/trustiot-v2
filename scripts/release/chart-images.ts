/**
 * The images that the chart deploys, and how a release pins them to its own digests
 * (Laysi/trustiot-v2#57).
 *
 * The repository's deploy/chart keeps the digests of our images empty: their tag defaults to
 * Chart.appVersion, so a digest written there would pair every later version's tag with an old
 * image. Once publish.ts has pushed this release's images, it packages the chart from a copy of
 * deploy/chart whose values.yaml carries their digests (packagePinnedChart), so the signed chart
 * pins every image of this release that it deploys. verify.ts reads the published chart's values
 * back and checks the references again, alone and with each deployment shape file
 * (chartPinProblems).
 *
 * values.yaml is edited as text, line by line, so its comments and layout stay as they are; the
 * result is parsed again and must equal the original with exactly the pinned fields set. The
 * same commit and the same image digests always give the same file, so a re-run of publish
 * packages the same chart.
 */

import { cpSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { run } from "./exec.ts";
import { type PlannedImage, type PublishedArtifact, ReleaseError, errorMessage, isDigest, isMap } from "./lib.ts";

type Values = Record<string, unknown>;

/** The CloudNativePG operand image (deploy/images/postgres); the chart runs it as postgres.bundled.image. */
export const POSTGRES_IMAGE = "postgres";
/** The migration Job's image (deploy/images/migrate, Laysi/trustiot-v2#60); the chart runs it as migrate.image. */
export const MIGRATE_IMAGE = "migrate";
/**
 * Images a release publishes that the chart does not deploy. Every other image must be a key
 * under the chart's `services`, the Postgres image or the migration Job's image, so a new image
 * is never left unpinned.
 */
export const NOT_DEPLOYED_BY_CHART: ReadonlySet<string> = new Set(["release-selftest"]);
/** The deployment shape files shipped in the chart; each is layered over values.yaml. */
export const SHAPE_VALUES_FILES = ["values-saas.yaml", "values-onprem.yaml"] as const;

/**
 * An image the chart deploys: a service (`services.<name>`, its repository from
 * serviceDefaults.image.registry), or a component with its own `image` map and repository (the
 * bundled Postgres, the migration Job).
 */
export interface ChartImage {
  name: string;
  kind: "service" | "postgres" | "migrate";
}

/** Where a component image keeps its own `repository`, `tag` and `digest`. */
const OWN_IMAGE_PATHS: Readonly<Record<"postgres" | "migrate", readonly string[]>> = {
  postgres: ["postgres", "bundled", "image"],
  migrate: ["migrate", "image"],
};

/** How the chart deploys the release image `name`; undefined when it does not deploy it. */
export function chartImage(name: string, values: Values): ChartImage | undefined {
  if (name === POSTGRES_IMAGE) {
    return { name, kind: "postgres" };
  }
  if (name === MIGRATE_IMAGE) {
    return { name, kind: "migrate" };
  }
  if (isMap(values.services) && Object.hasOwn(values.services, name)) {
    return { name, kind: "service" };
  }
  if (NOT_DEPLOYED_BY_CHART.has(name)) {
    return undefined;
  }
  throw new ReleaseError(
    `image \`${name}\` is neither deployed by the chart (a key under services, ${POSTGRES_IMAGE} or ${MIGRATE_IMAGE}) nor listed in ` +
      "NOT_DEPLOYED_BY_CHART (scripts/release/chart-images.ts); the chart would not pin it",
  );
}

/** The values field the release writes the image's digest to. */
export function digestPath(image: ChartImage): string[] {
  return image.kind === "service" ? ["services", image.name, "image", "digest"] : [...OWN_IMAGE_PATHS[image.kind], "digest"];
}

export interface ImageReference {
  repository: string;
  tag: string;
  digest: string;
  /** What the templates render: `repository:tag@digest`, or `repository:tag` without a digest. */
  ref: string;
}

function scalarText(value: unknown): string {
  return value === undefined || value === null ? "" : String(value);
}

function getIn(values: unknown, path: readonly string[]): unknown {
  let current = values;
  for (const key of path) {
    current = isMap(current) ? current[key] : undefined;
  }
  return current;
}

function setIn(values: Values, path: readonly string[], value: unknown): void {
  let current = values;
  for (const key of path.slice(0, -1)) {
    const next = current[key];
    if (isMap(next)) {
      current = next;
    } else {
      const created: Values = {};
      current[key] = created;
      current = created;
    }
  }
  current[path.at(-1) ?? ""] = value;
}

/**
 * Layers `overlay` over `base` as Helm layers values files: maps merge key by key, lists and
 * scalars are replaced, and null removes the key.
 */
export function mergeValues(base: Values, overlay: Values): Values {
  const merged: Values = { ...base };
  for (const [key, value] of Object.entries(overlay)) {
    const current = merged[key];
    if (value === null) {
      delete merged[key];
    } else if (isMap(value) && isMap(current)) {
      merged[key] = mergeValues(current, value);
    } else {
      merged[key] = value;
    }
  }
  return merged;
}

/**
 * The reference the templates render for `image` from these values: trustiot.serviceImage in
 * _helpers.tpl (serviceDefaults.image merged with services.<name>.image), trustiot.postgres.image
 * in _postgres.tpl and trustiot.migrate.image in _migrate.tpl. chart-images.test.ts checks it
 * against helm template.
 */
export function imageReference(values: Values, image: ChartImage, appVersion: string): ImageReference {
  let settings: Values;
  let repository: string;
  if (image.kind !== "service") {
    const path = OWN_IMAGE_PATHS[image.kind];
    const own = getIn(values, path);
    if (!isMap(own)) {
      throw new ReleaseError(`the chart values have no ${path.join(".")}`);
    }
    settings = own;
    repository = scalarText(settings.repository);
  } else {
    const defaults = getIn(values, ["serviceDefaults", "image"]);
    if (!isMap(defaults)) {
      throw new ReleaseError("the chart values have no serviceDefaults.image");
    }
    const own = getIn(values, ["services", image.name, "image"]);
    settings = isMap(own) ? mergeValues(defaults, own) : defaults;
    repository = scalarText(settings.repository) || `${scalarText(settings.registry)}/${image.name}`;
  }
  const tag = scalarText(settings.tag) || appVersion;
  const digest = scalarText(settings.digest);
  return { repository, tag, digest, ref: digest === "" ? `${repository}:${tag}` : `${repository}:${tag}@${digest}` };
}

/** One image of this release that the chart deploys, with where it was pushed. */
export interface ChartPin {
  image: ChartImage;
  /** The repository the release pushed it to, without tag. */
  repository: string;
  digest: string;
}

/** The pins for the published images that the chart deploys. */
export function chartPins(planned: readonly PlannedImage[], published: readonly PublishedArtifact[], values: Values): ChartPin[] {
  const pins: ChartPin[] = [];
  for (const artifact of published) {
    const plannedImage = planned.find((candidate) => candidate.name === artifact.name);
    if (plannedImage === undefined) {
      throw new ReleaseError(`published image ${artifact.name} was not planned`);
    }
    if (!isDigest(artifact.digest)) {
      throw new ReleaseError(`published image ${artifact.name} has no sha256 digest`);
    }
    const image = chartImage(artifact.name, values);
    if (image !== undefined) {
      pins.push({ image, repository: plannedImage.repository, digest: artifact.digest });
    }
  }
  return pins;
}

/** The reference a pinned chart must render for `pin`. */
export function pinnedReference(pin: ChartPin, appVersion: string): string {
  return `${pin.repository}:${appVersion}@${pin.digest}`;
}

export function parseValues(text: string, file: string): Values {
  let doc: unknown;
  try {
    doc = Bun.YAML.parse(text);
  } catch (error) {
    throw new ReleaseError(`${file} is not YAML: ${errorMessage(error)}`);
  }
  if (doc === null || doc === undefined) {
    return {};
  }
  if (!isMap(doc)) {
    throw new ReleaseError(`${file} is not a YAML mapping`);
  }
  return doc;
}

// ---------------------------------------------------------------------------------------------
// Editing values.yaml as text

interface KeyLine {
  index: number;
  indent: number;
  /** Keys from the document root; a sequence item adds "[]", so no key inside a list matches a target. */
  path: string[];
  /** The key as written. */
  key: string;
  /** The value as written, without its comment; "" for a mapping (or empty) value. */
  value: string;
  /** Whitespace and comment after the value, kept when the value is replaced. */
  comment: string;
}

const KEY_LINE = /^( *)([^\s#'"{}[\],&*!|>%@`-][^:#]*?|"[^"\\]*"|'[^']*'):(?=\s|$)(.*)$/;

function unquote(key: string): string {
  return key.startsWith('"') || key.startsWith("'") ? key.slice(1, -1) : key;
}

function splitValue(rest: string): { value: string; comment: string } {
  const body = rest.trimStart();
  if (body === "" || body.startsWith("#")) {
    return { value: "", comment: rest };
  }
  let end = body.length;
  if (body.startsWith('"')) {
    const close = /^"(?:[^"\\]|\\.)*"/.exec(body);
    end = close === null ? body.length : close[0].length;
  } else if (body.startsWith("'")) {
    const close = /^'(?:[^']|'')*'/.exec(body);
    end = close === null ? body.length : close[0].length;
  } else {
    const hash = body.search(/\s#/);
    end = hash < 0 ? body.length : hash;
  }
  const value = body.slice(0, end).trimEnd();
  return { value, comment: body.slice(value.length) };
}

/** Every `key:` line of a block-style YAML document, with its path. */
function scanKeys(lines: readonly string[]): KeyLine[] {
  const keys: KeyLine[] = [];
  const stack: { indent: number; key: string }[] = [];
  let blockIndent = -1;
  lines.forEach((line, index) => {
    const trimmed = line.trim();
    if (trimmed === "") {
      return;
    }
    if (/^ *\t/.test(line)) {
      throw new ReleaseError(`values.yaml line ${index + 1} is indented with a tab`);
    }
    const indent = line.length - line.trimStart().length;
    if (blockIndent >= 0) {
      if (indent > blockIndent) {
        return;
      }
      blockIndent = -1;
    }
    if (trimmed.startsWith("#")) {
      return;
    }
    if (trimmed === "---" || trimmed === "...") {
      throw new ReleaseError("values.yaml must be a single YAML document");
    }
    const isItem = trimmed === "-" || trimmed.startsWith("- ");
    const match = isItem ? null : KEY_LINE.exec(line);
    if (!isItem && match === null) {
      // The continuation of a multi-line scalar.
      return;
    }
    while ((stack.at(-1)?.indent ?? -1) >= indent) {
      stack.pop();
    }
    if (isItem || match === null) {
      stack.push({ indent, key: "[]" });
      return;
    }
    const key = unquote(match[2] ?? "");
    const { value, comment } = splitValue(match[3] ?? "");
    keys.push({ index, indent, path: [...stack.map((entry) => entry.key), key], key: match[2] ?? "", value, comment });
    if (/^[|>]/.test(value)) {
      blockIndent = indent;
    }
    stack.push({ indent, key });
  });
  return keys;
}

function samePath(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((key, i) => key === b[i]);
}

/** A scalar as YAML text: plain when it is a registry path or digest, otherwise double-quoted. */
function formatScalar(value: string): string {
  return /^[a-z0-9][a-z0-9.:_/@-]*$/.test(value) ? value : JSON.stringify(value);
}

/**
 * Sets the scalar at `path` in `lines`: replaces the value of an existing key, or adds the
 * missing keys under the deepest mapping that exists.
 */
function setScalar(lines: string[], path: readonly string[], value: string): void {
  const keys = scanKeys(lines);
  const where = path.join(".");
  const found = keys.filter((entry) => samePath(entry.path, path));
  if (found.length > 1) {
    throw new ReleaseError(`values.yaml has ${where} more than once`);
  }
  const existing = found[0];
  if (existing !== undefined) {
    if (existing.value === "" || /^[|>[{]/.test(existing.value)) {
      throw new ReleaseError(`${where} in values.yaml is not a single-line scalar`);
    }
    lines[existing.index] = `${" ".repeat(existing.indent)}${existing.key}: ${formatScalar(value)}${existing.comment}`;
    return;
  }
  for (let depth = path.length - 1; depth >= 1; depth--) {
    const prefix = path.slice(0, depth);
    const parents = keys.filter((entry) => samePath(entry.path, prefix));
    if (parents.length === 0) {
      continue;
    }
    const parent = parents[0];
    if (parents.length > 1 || parent === undefined) {
      throw new ReleaseError(`values.yaml has ${prefix.join(".")} more than once`);
    }
    if (parent.value !== "") {
      throw new ReleaseError(`${prefix.join(".")} in values.yaml is not a block mapping, so ${where} cannot be added`);
    }
    const child = keys.find((entry) => entry.index > parent.index && entry.path.length === depth + 1 && samePath(entry.path.slice(0, depth), prefix));
    const indent = child?.indent ?? parent.indent + 2;
    const missing = path.slice(depth);
    const added = missing.map((key, i) => `${" ".repeat(indent + 2 * i)}${key}:${i === missing.length - 1 ? ` ${formatScalar(value)}` : ""}`);
    lines.splice(parent.index + 1, 0, `${" ".repeat(indent)}# Added by the release (scripts/release/chart-images.ts).`, ...added);
    return;
  }
  throw new ReleaseError(`values.yaml has no ${path[0] ?? "keys"}`);
}

export interface PinOptions {
  /** Chart.appVersion of the packaged chart, which is the release version. */
  appVersion: string;
  /**
   * Local test only: point the chart at the registry the plan pushed to (serviceDefaults.image.
   * registry, postgres.bundled.image.repository, migrate.image.repository). A release refuses a
   * chart whose repositories differ from where it pushed.
   */
  rewriteRepositories: boolean;
}

/**
 * values.yaml with the digest of every pinned image written in. Each image must render with
 * tag appVersion, without a digest, from the repository it was pushed to (or, with
 * rewriteRepositories, from a repository this rewrites); anything else is an error rather than
 * a silent override.
 */
export function pinChartValues(source: string, pins: readonly ChartPin[], options: PinOptions): string {
  const original = parseValues(source, "values.yaml");
  const expected = structuredClone(original);
  const lines = source.split("\n");
  const set = (path: readonly string[], value: string): void => {
    setScalar(lines, path, value);
    setIn(expected, path, value);
  };
  for (const pin of pins) {
    const name = pin.image.name;
    if (!isDigest(pin.digest)) {
      throw new ReleaseError(`the digest of ${name} is not a sha256 digest`);
    }
    const current = imageReference(original, pin.image, options.appVersion);
    if (current.digest !== "") {
      throw new ReleaseError(
        `the chart already pins ${name} to ${current.digest}: deploy/chart keeps the digests of the release's images empty ` +
          `(${digestPath(pin.image).join(".")}) and the release writes them`,
      );
    }
    if (current.tag !== options.appVersion) {
      throw new ReleaseError(`the chart deploys ${name} with tag ${current.tag}, not appVersion ${options.appVersion}: leave its tag empty`);
    }
    if (current.repository !== pin.repository) {
      if (!options.rewriteRepositories) {
        throw new ReleaseError(`the chart deploys ${name} from ${current.repository}, but the release pushed it to ${pin.repository}`);
      }
      if (pin.image.kind !== "service") {
        set([...OWN_IMAGE_PATHS[pin.image.kind], "repository"], pin.repository);
      } else {
        const suffix = `/${name}`;
        if (!pin.repository.endsWith(suffix)) {
          throw new ReleaseError(`${pin.repository} does not end with ${suffix}`);
        }
        set(["serviceDefaults", "image", "registry"], pin.repository.slice(0, -suffix.length));
      }
    }
    set(digestPath(pin.image), pin.digest);
  }
  const result = lines.join("\n");
  const parsed = parseValues(result, "the pinned values.yaml");
  if (!Bun.deepEquals(parsed, expected, true)) {
    throw new ReleaseError("pinning the image digests changed values.yaml in more places than the image fields");
  }
  for (const pin of pins) {
    const rendered = imageReference(parsed, pin.image, options.appVersion).ref;
    if (rendered !== pinnedReference(pin, options.appVersion)) {
      throw new ReleaseError(`the pinned chart deploys ${pin.image.name} as ${rendered}, not ${pinnedReference(pin, options.appVersion)}`);
    }
  }
  return result;
}

/** values.yaml and the shape files of a chart directory. */
export function readChartValues(chartDir: string): { values: Values; shapes: { file: string; values: Values }[] } {
  const read = (file: string): Values => {
    const path = join(chartDir, file);
    if (!existsSync(path)) {
      throw new ReleaseError(`the chart has no ${file}`);
    }
    return parseValues(readFileSync(path, "utf8"), file);
  };
  return { values: read("values.yaml"), shapes: SHAPE_VALUES_FILES.map((file) => ({ file, values: read(file) })) };
}

/**
 * What keeps a chart from deploying exactly this release's images: for values.yaml alone and
 * layered with each overlay (the shape files, or an install's overlays), every pinned image must
 * render as `<pushed repository>:<appVersion>@<digest>`.
 */
export function chartPinProblems(
  values: Values,
  overlays: readonly { file: string; values: Values }[],
  pins: readonly ChartPin[],
  appVersion: string,
): string[] {
  const problems: string[] = [];
  const layers = [{ file: "values.yaml", values }, ...overlays.map((overlay) => ({ file: `values.yaml + ${overlay.file}`, values: mergeValues(values, overlay.values) }))];
  for (const layer of layers) {
    for (const pin of pins) {
      const expected = pinnedReference(pin, appVersion);
      let rendered: string;
      try {
        rendered = imageReference(layer.values, pin.image, appVersion).ref;
      } catch (error) {
        problems.push(`${layer.file}: ${errorMessage(error)}`);
        continue;
      }
      if (rendered !== expected) {
        problems.push(`${layer.file}: ${pin.image.name} is ${rendered}, not ${expected}`);
      }
    }
  }
  return problems;
}

/**
 * values.yaml of `chartDir` with the pins written in, after checking that it and each shape file
 * layered over it render every pinned reference. Writes nothing.
 */
export function pinnedChartValues(chartDir: string, pins: readonly ChartPin[], options: PinOptions): string {
  const pinned = pinChartValues(readFileSync(join(chartDir, "values.yaml"), "utf8"), pins, options);
  const { shapes } = readChartValues(chartDir);
  const problems = chartPinProblems(parseValues(pinned, "the pinned values.yaml"), shapes, pins, options.appVersion);
  if (problems.length > 0) {
    throw new ReleaseError(`the pinned chart does not deploy this release's images: ${problems.join("; ")}`);
  }
  return pinned;
}

export interface PackageOptions extends PinOptions {
  helm: string;
  /** The chart directory in the checkout, which stays unchanged. */
  chartDir: string;
  /** An empty directory for the copy and the package. */
  workDir: string;
  sourceDateEpoch: number;
}

/**
 * Packages a copy of the chart with the pins written into its values.yaml, after checking that
 * the copy and both shape files render every pinned reference. Returns the .tgz path.
 */
export async function packagePinnedChart(pins: readonly ChartPin[], options: PackageOptions): Promise<string> {
  const copy = join(options.workDir, "chart-source");
  const out = join(options.workDir, "package");
  mkdirSync(options.workDir, { recursive: true });
  cpSync(options.chartDir, copy, { recursive: true });
  mkdirSync(out, { recursive: true });
  writeFileSync(join(copy, "values.yaml"), pinnedChartValues(copy, pins, options));
  // SOURCE_DATE_EPOCH fixes the timestamps in the package, so the same commit and digests give
  // the same archive.
  await run([options.helm, "package", copy, "--version", options.appVersion, "--app-version", options.appVersion, "--destination", out], {
    env: { SOURCE_DATE_EPOCH: String(options.sourceDateEpoch) },
  });
  const chartName = Bun.YAML.parse(readFileSync(join(copy, "Chart.yaml"), "utf8"));
  const name = isMap(chartName) && typeof chartName.name === "string" ? chartName.name : "";
  const tgz = join(out, `${name}-${options.appVersion}.tgz`);
  if (name === "" || !existsSync(tgz)) {
    throw new ReleaseError(`helm package did not write ${tgz}`);
  }
  return tgz;
}

/** Unpacks a chart archive into `dir` and returns the chart directory inside it. */
export async function unpackChart(tar: string, archive: string, dir: string, chartName: string): Promise<string> {
  mkdirSync(dir, { recursive: true });
  await run([tar, "-xzf", archive, "-C", dir, "--no-same-owner", "--no-same-permissions"]);
  const chartDir = join(dir, chartName);
  if (!existsSync(join(chartDir, "Chart.yaml"))) {
    throw new ReleaseError(`the chart archive has no ${chartName}/Chart.yaml`);
  }
  return chartDir;
}
