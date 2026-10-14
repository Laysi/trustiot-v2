/**
 * Secret handling check (docs/architecture/secrets.md, checklist M3).
 *
 * Rules:
 *
 * 1. Container build files (`Dockerfile`, `Containerfile`, `*.Dockerfile`, `*.Containerfile`,
 *    `Dockerfile.*`, `Containerfile.*`) anywhere in the repository:
 *    a. `COPY` and `ADD` never name a key or credential file (private keys, keystores, `.env`,
 *       kubeconfig, `.npmrc`, `.ssh/`, `.git`, ...), whatever stage they copy from, and never
 *       take one that exists in the repository through a directory or wildcard source. The
 *       build command picks the context, so the build file's directory and every directory
 *       above it up to the repository root count as possible contexts, each with its own
 *       `.containerignore` or `.dockerignore`. Sources never use a variable (`$FILE`), whose
 *       value this check cannot see.
 *    b. `COPY` and `ADD` of the whole build context (`.`, `*`, `/`) need, in every one of those
 *       possible context directories, an ignore file (`.containerignore` or `.dockerignore`)
 *       that starts by ignoring everything, so only allow-listed files reach the build; no `!`
 *       line may name a key or credential file or use a wildcard that could match one.
 *    c. `ARG` never declares a secret-looking name: build arguments end up in the image
 *       history. Build-time secrets go through `RUN --mount=type=secret`.
 *    d. `ENV` never sets a secret-looking name: it stays in the image configuration.
 *    e. `ADD` never fetches a URL with credentials in it.
 * 2. The repository root is never a build context: its `.dockerignore` (and `.containerignore`,
 *    when there is one) holds only the ignore-everything pattern, with no `!` exceptions.
 *    Nothing may replace a context's own ignore file: Dockerfile-specific ignore files
 *    (`<file>.dockerignore`) and podman's `--ignorefile` are rejected.
 * 3. Build commands never pass a secret as a build argument. In workflows, local actions and
 *    scripts (`.ts`, `.js`, `.sh`; tests excluded), every `--build-arg` must be one literal
 *    `NAME=value` (a bare `NAME` takes its value from the builder's environment, which this
 *    check cannot see): the name must not look like a secret, and the value must be literal text
 *    or an allow-listed public fact of the run (`${{ github.sha }}`, `github.ref_name`, ...),
 *    because any other value computed at run time (`github.token`, `env`, step outputs)
 *    could carry a secret this check cannot see.
 *    `--build-arg-file` is rejected because its content cannot be checked. The `build-args`
 *    input of a workflow step follows the same rules.
 * 4. Workflows and local actions hand secrets to commands through `env`, never by writing an
 *    expression that reads the `secrets` context (in any letter case) into a `run` script.
 * 5. Kubernetes Secret manifests in the repository are SOPS-encrypted: every value under
 *    `data` and `stringData` has the shape SOPS writes (AES256_GCM with a 32-byte IV and a
 *    16-byte tag), and the `sops` block has a version, a modification time, an encrypted MAC
 *    and age recipients with their wrapped data keys. Kustomize `secretGenerator` is not
 *    used: its literals, files and env files are plain text this check cannot verify. Templates
 *    (Helm `.yaml` and `.tpl` files with `{{ }}`) may not declare a Secret or a templated kind,
 *    except an empty Secret the migration Job fills: labelled `trustiot.io/filled-by: migrate`,
 *    with no key but apiVersion, kind, metadata and type, and no template action but the few
 *    lines `FILLED_SECRET_ACTIONS` names word for word (docs/architecture/secrets.md). No template
 *    may use a function that makes or reads secret material (`lookup`, `rand*`, `gen*`,
 *    `derivePassword`, ...; `checkTemplateFunctions`).
 *    Manifests may also be JSON files.
 * 6. The build context is chosen at build time, which a static check cannot see, so every
 *    container build takes its arguments from `containerBuildArgs` (scripts/lib/build-context.ts),
 *    which checks the directory it then builds and fails when that context would send a key or
 *    credential file to the builder. Build commands written anywhere else (other scripts,
 *    shell scripts, workflow steps) are rejected, and so are container commands whose
 *    subcommand is not fixed text (`[podman, op]`, `podman ${op}`), because the check cannot
 *    tell whether they build. The dev environment's podman runner (scripts/lib/podman.ts)
 *    passes its callers' arguments along, so it refuses at run time any build whose arguments
 *    did not come from containerBuildArgs. These rules catch the ways a build is normally
 *    written; code written to hide a build from them (aliasing the runner, computing the tool
 *    name) is left to review.
 *
 * Secret-looking names use the chart's rule (deploy/chart/values.schema.json): SECRET, PASSW,
 * PASSPHRASE, PWD, TOKEN, API_KEY or APIKEY, DSN, PRIVATE, CREDENTIAL, or a name ending in
 * `_KEY` or equal to `KEY`, case-insensitive.
 *
 * Usage: bun scripts/ci/check-secret-handling.ts [repository-root]
 * Exit status: 0 when every file passes, 1 when a rule is violated, 2 when the repository
 * cannot be read.
 */

import { existsSync, lstatSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, posix, relative, resolve, sep } from "node:path";

/** One broken rule, located precisely enough to fix without searching. */
export interface Violation {
  /** The file, relative to the repository root. */
  file: string;
  /** 1-based line number; 0 when the violation concerns the whole file. */
  line: number;
  /** What is wrong and how to fix it. */
  message: string;
}

type YamlMap = Record<string, unknown>;

/** The chart's secret-looking name rule (values.schema.json, `trustiot.credentialCatalog`). */
export const SECRET_LIKE_NAME =
  /secret|passw|passphrase|pwd|token|api_?key|(^|_)key$|dsn|private|credential/i;

/** Directories never searched: VCS metadata, dependencies and build output. */
const SKIPPED_DIRECTORIES: ReadonlySet<string> = new Set([".git", "node_modules", "target"]);

const BUILD_FILE = /^(dockerfile|containerfile)(\..+)?$|\.(dockerfile|containerfile)$/i;
const IGNORE_FILE = /\.(dockerignore|containerignore)$/i;
const SCRIPT_FILE = /\.(ts|mts|cts|js|mjs|cjs|sh|bash)$/;
const TEST_FILE = /\.test\.[cm]?[jt]s$/;
/**
 * Recorded test evidence (docs/test-reports/README.md): scripts there are test doubles kept
 * with a report, such as a podman wrapper that passes its arguments along; CI and the release
 * never run them. Like test files, they are not scanned as scripts.
 */
const EVIDENCE_DIR = "docs/test-reports/data/";
const YAML_FILE = /\.ya?ml$/;
/** Kubernetes manifests can also be JSON; YAML parsing reads both. */
const JSON_FILE = /\.json$/;
/** This file spells out the patterns it looks for, so it is not scanned for them. */
const SELF = "scripts/ci/check-secret-handling.ts";

/**
 * Path components that name a key or credential file. Each entry is tested against every
 * component of a `COPY`/`ADD` source, lower-cased.
 */
const SECRET_PATH_COMPONENTS: readonly [RegExp, string][] = [
  // `.env`, `.env.production`, `app.env`, `production.env.local` and direnv's `.envrc`;
  // templates (`.env.example`, `app.env.sample`) are fine.
  [/^(\.|.+\.)env(\.(?!example$|sample$|template$|dist$).+)?$/, "a dotenv file"],
  [/^\.envrc$/, "a dotenv file"],
  [/\.(pem|key|p8|p12|pfx|jks|keystore|ppk|kdbx|agekey)$/, "a private key or keystore"],
  [/^id_(rsa|dsa|ecdsa|ed25519)(_sk)?$/, "an SSH private key"],
  [
    /^(_netrc|\.netrc|\.npmrc|\.pypirc|\.pgpass|\.git-credentials|\.htpasswd|\.vault-token)$/,
    "a credential file",
  ],
  [/^(.+\.)?kubeconfig$/, "a kubeconfig"],
  [/^(credentials?|secrets?)(\..+)?$/, "a credentials file"],
  // A word of the file name, between separators: `service-account.json`, `client_secret.json`,
  // `gcp-credentials.json`, `prod.token`, `api_key.txt`, `signing-private-key.bin`.
  [
    /(^|[-_.])(client[-_]?secrets?|service[-_]?account|sa[-_]key|credentials?|secrets?|tokens?|api[-_]?keys?|private[-_]?keys?)([-_.]|$)/,
    "a credentials file",
  ],
  [/^\.(ssh|gnupg|aws|azure|kube|docker)$/, "a credential directory"],
  [/^\.git$/, "the git metadata"],
];

/** First pattern of an ignore file that ignores everything. */
const DENY_ALL: ReadonlySet<string> = new Set(["*", "**", "/*", "/**"]);

function isMap(value: unknown): value is YamlMap {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Converts a path relative to the root to the forward-slash form used in messages. */
function display(root: string, path: string): string {
  return relative(root, path).split(sep).join("/");
}

// --- File discovery -------------------------------------------------------------------------

/**
 * Lists the files under `root`, relative to it. Skips `.git`, `node_modules` and `target`,
 * symbolic links to directories (no cycles), and nested repositories or git worktrees (a
 * directory that holds its own `.git`), which belong to another checkout.
 */
export function listFiles(root: string): string[] {
  const found: string[] = [];
  const visit = (directory: string) => {
    for (const name of readdirSync(directory).sort()) {
      const path = join(directory, name);
      const link = lstatSync(path);
      if (link.isDirectory()) {
        if (SKIPPED_DIRECTORIES.has(name) || existsSync(join(path, ".git"))) {
          continue;
        }
        visit(path);
      } else if (link.isFile()) {
        found.push(path);
      } else if (link.isSymbolicLink()) {
        // A linked file is checked where it points; dangling links and linked directories
        // are skipped.
        if (statSync(path, { throwIfNoEntry: false })?.isFile()) {
          found.push(path);
        }
      }
    }
  };
  visit(root);
  return found.map((path) => display(root, path));
}

export function isBuildFile(path: string): boolean {
  const name = posix.basename(path);
  return BUILD_FILE.test(name) && !IGNORE_FILE.test(name);
}

// --- Container build files ------------------------------------------------------------------

/** One instruction of a build file, with its continuation lines joined. */
export interface Instruction {
  /** 1-based line of the instruction keyword. */
  line: number;
  /** Upper-cased keyword, for example `COPY`. */
  keyword: string;
  /** Everything after the keyword, continuation lines joined with a space. */
  args: string;
}

/** A heredoc marker: a word of its own that starts with `<<` (`<<EOF`, `<<-"EOF"`). */
const HEREDOC = /(?:^|\s)<<(-?)(["']?)([A-Za-z_][A-Za-z0-9_]*)\2(?=\s|$)/g;
/** The only instructions whose heredocs Docker reads; elsewhere `<<EOF` is ordinary text. */
const HEREDOC_KEYWORDS: ReadonlySet<string> = new Set(["RUN", "COPY", "ADD"]);

/**
 * Splits a Dockerfile or Containerfile into instructions. Handles the `escape` parser
 * directive, comment lines (also inside continuations), line continuations and heredoc
 * bodies, which are skipped: they are inline content, not files from the build context.
 */
export function parseBuildFile(source: string): Instruction[] {
  const lines = source.split(/\r?\n/);
  let escape = "\\";
  // Parser directives are comment lines at the very top, before anything else.
  for (const line of lines) {
    const directive = /^#\s*([a-zA-Z]+)\s*=\s*(\S+)\s*$/.exec(line);
    if (!directive) {
      break;
    }
    if (directive[1]?.toLowerCase() === "escape" && (directive[2] === "`" || directive[2] === "\\")) {
      escape = directive[2];
    }
  }

  const instructions: Instruction[] = [];
  let index = 0;
  while (index < lines.length) {
    const first = lines[index] ?? "";
    if (first.trim() === "" || first.trimStart().startsWith("#")) {
      index += 1;
      continue;
    }
    const start = index + 1;
    let text = "";
    // Join continuation lines; comment lines inside a continuation are dropped.
    for (;;) {
      const line = lines[index] ?? "";
      index += 1;
      const trimmed = line.trimEnd();
      if (trimmed.endsWith(escape) && index < lines.length) {
        text += `${trimmed.slice(0, -1)} `;
        while (index < lines.length && (lines[index] ?? "").trimStart().startsWith("#")) {
          index += 1;
        }
        continue;
      }
      text += trimmed;
      break;
    }
    const match = /^\s*(\S+)\s*(.*)$/.exec(text);
    const keyword = (match?.[1] ?? "").toUpperCase();
    const args = match?.[2] ?? "";
    instructions.push({ line: start, keyword, args });
    // Skip heredoc bodies (`RUN <<EOF`, `COPY <<EOF /file`), one after another.
    const trigger = keyword === "ONBUILD" ? /^(\S+)\s*(.*)$/.exec(args) : null;
    const heredocKeyword = trigger === null ? keyword : (trigger[1] ?? "").toUpperCase();
    const triggerArgs = trigger === null ? args : (trigger[2] ?? "");
    for (const heredoc of HEREDOC_KEYWORDS.has(heredocKeyword) ? triggerArgs.matchAll(HEREDOC) : []) {
      const stripTabs = heredoc[1] === "-";
      const terminator = heredoc[3] ?? "";
      while (index < lines.length) {
        const line = lines[index] ?? "";
        index += 1;
        if ((stripTabs ? line.replace(/^\t+/, "") : line) === terminator) {
          break;
        }
      }
    }
  }
  return instructions;
}

/** Splits the arguments of a shell-form or JSON-form instruction into words. */
function words(args: string): string[] {
  const trimmed = args.trim();
  if (trimmed.startsWith("[")) {
    try {
      const parsed: unknown = JSON.parse(trimmed);
      if (Array.isArray(parsed) && parsed.every((item) => typeof item === "string")) {
        return parsed as string[];
      }
    } catch {
      // Not valid JSON: Docker treats it as shell form too.
    }
  }
  return shellWords(trimmed);
}

/**
 * Splits shell-form arguments into words the way the Dockerfile word parser does: whitespace
 * separates words except inside double or single quotes, which are removed, so
 * `"cert folder/server.key"` is one word. A backslash (or a backtick, the other escape
 * character) keeps a following space, quote or escape character literal; before anything
 * else it stays as it is, so Windows paths such as `C:\app` are unchanged. An unclosed quote
 * runs to the end.
 */
export function shellWords(text: string): string[] {
  const result: string[] = [];
  let word = "";
  let inWord = false;
  let quote: string | undefined;
  for (let index = 0; index < text.length; index += 1) {
    const char = text[index] ?? "";
    const next = text[index + 1] ?? "";
    if (quote === undefined && /\s/.test(char)) {
      if (inWord) {
        result.push(word);
      }
      word = "";
      inWord = false;
      continue;
    }
    inWord = true;
    if (quote === "'") {
      if (char === "'") {
        quote = undefined;
      } else {
        word += char;
      }
      continue;
    }
    if ((char === "\\" || char === "`") && next !== "" && (quote === undefined ? /[\s"'\\`]/ : /["\\`]/).test(next)) {
      word += next;
      index += 1;
      continue;
    }
    if (char === '"') {
      quote = quote === '"' ? undefined : '"';
      continue;
    }
    if (char === "'" && quote === undefined) {
      quote = "'";
      continue;
    }
    word += char;
  }
  if (inWord) {
    result.push(word);
  }
  return result;
}

/** The variable names an `ARG` or `ENV` instruction declares. */
function declaredNames(keyword: string, args: string): string[] {
  const parts = words(args);
  if (keyword === "ENV" && parts.length >= 2 && !(parts[0] ?? "").includes("=")) {
    // Legacy form: `ENV NAME value with spaces`.
    return [parts[0] ?? ""];
  }
  return parts.map((part) => part.split("=")[0] ?? "").filter((name) => /^[A-Za-z_]/.test(name));
}

function secretPathReason(source: string): string | undefined {
  for (const component of source.toLowerCase().split("/")) {
    for (const [pattern, reason] of SECRET_PATH_COMPONENTS) {
      if (pattern.test(component)) {
        return reason;
      }
    }
  }
  return undefined;
}

/**
 * A `COPY`/`ADD` source as the path inside the context that the builder reads: cleaned like
 * BuildKit does, with `..` clamped at the context root, so `foo/..` is the whole context and
 * `foo/../*` is `*`. URLs are returned as they are.
 */
export function contextPath(source: string): string {
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(source)) {
    return source;
  }
  let clean = posix.normalize(source.replace(/^\/+/, "") || ".");
  while (clean === ".." || clean.startsWith("../")) {
    clean = clean === ".." ? "." : clean.slice(3) || ".";
  }
  return clean.replace(/\/+$/, "") || ".";
}

/** Whether a `COPY`/`ADD` source names the whole build context. */
function isWholeContext(source: string): boolean {
  const normalized = source.replace(/^(\.\/)+/, "").replace(/\/+$/, "");
  return ["", ".", "*", "./*", "/", "/*", "**"].includes(normalized);
}

const URL_WITH_USERINFO = /^[a-z][a-z0-9+.-]*:\/\/[^/@\s]*@/i;

/**
 * The patterns of an ignore file, without blank lines and comments. A pattern that names the
 * context itself (`.`, `./`, `/`, also after `!`) matches nothing for Docker and podman, so it
 * is dropped instead of being read as "everything".
 */
function ignorePatterns(source: string): string[] {
  return source
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("#") && !namesContext(line));
}

/** Whether an ignore pattern names the context itself (`.`, `./`, `/`, `!.`), which matches nothing. */
function namesContext(pattern: string): boolean {
  const clean = posix.normalize(pattern.replace(/^!/, "").replace(/^\/+/, "") || ".").replace(/\/+$/, "");
  return clean === "." || clean === "";
}

/** Whether an ignore file's first pattern ignores everything. */
export function ignoresEverything(source: string): boolean {
  const first = ignorePatterns(source)[0];
  return first !== undefined && DENY_ALL.has(first);
}

/** Whether an ignore file has no pattern but "ignore everything", so nothing is re-included. */
export function ignoresOnlyEverything(source: string): boolean {
  const patterns = ignorePatterns(source);
  return patterns.length > 0 && patterns.every((pattern) => DENY_ALL.has(pattern));
}

/**
 * Converts a path pattern, as `.dockerignore` and `COPY` sources write it, to a regular
 * expression over slash-separated paths relative to the build context: `*` and `?` stay
 * within one path segment, `**` spans segments, `[...]` is a character class.
 */
export function globRegExp(pattern: string): RegExp {
  const clean = posix.normalize(pattern.replace(/^(\.\/)+/, "").replace(/^\/+/, "")).replace(/\/+$/, "");
  if (clean === "" || clean === ".") {
    return /^.*$/;
  }
  let out = "";
  for (let i = 0; i < clean.length; i += 1) {
    const char = clean[i] ?? "";
    if (char === "*") {
      if (clean[i + 1] === "*") {
        i += 1;
        if (clean[i + 1] === "/") {
          i += 1;
          out += "(?:.*/)?";
        } else {
          out += ".*";
        }
      } else {
        out += "[^/]*";
      }
    } else if (char === "?") {
      out += "[^/]";
    } else if (char === "[" && clean.indexOf("]", i + 1) > i) {
      const end = clean.indexOf("]", i + 1);
      out += `[${clean.slice(i + 1, end).replace(/^!/, "^").replaceAll("\\", "\\\\")}]`;
      i = end;
    } else if (char === "\\" && i + 1 < clean.length) {
      i += 1;
      out += (clean[i] ?? "").replace(/[.*+?^${}()|[\]\\/]/g, "\\$&");
    } else {
      out += char.replace(/[.+^${}()|[\]\\/]/g, "\\$&");
    }
  }
  return new RegExp(`^${out}$`);
}

/** Whether a pattern matches `path` or one of its parent directories. */
function matchesOrParent(pattern: RegExp, path: string): boolean {
  const parts = path.split("/");
  return parts.some((_, index) => pattern.test(parts.slice(0, index + 1).join("/")));
}

/**
 * Whether an ignore file's patterns leave `path` out of the build context: the last pattern
 * that matches the path or one of its parent directories wins, and `!` patterns re-include.
 */
export function isIgnored(patterns: readonly string[], path: string): boolean {
  let ignored = false;
  for (const pattern of patterns) {
    if (namesContext(pattern)) {
      continue;
    }
    const negated = pattern.startsWith("!");
    if (matchesOrParent(globRegExp(negated ? pattern.slice(1) : pattern), path)) {
      ignored = !negated;
    }
  }
  return ignored;
}

/** Key and credential file names that a wildcard exception must not be able to match. */
const SAMPLE_SECRET_FILES = [
  "server.key",
  "tls.pem",
  "keystore.p12",
  ".env",
  ".env.local",
  "id_ed25519",
  "kubeconfig",
  "credentials.json",
  ".npmrc",
  ".git-credentials",
];

/**
 * The `!` patterns of an ignore file that can bring back everything or a key or credential
 * file: by naming one, or through a wildcard in the last segment that matches one (for example
 * `!certs/*`). Exceptions are judged by their text, so they also cover files that only exist
 * on a developer's machine; files that exist in the repository are checked separately.
 */
export function reincludedSecrets(source: string): string[] {
  return ignorePatterns(source)
    .filter((pattern) => pattern.startsWith("!"))
    .filter((pattern) => {
      const path = pattern.slice(1).replace(/^\/+/, "").replace(/\/+$/, "");
      if (DENY_ALL.has(path) || isWholeContext(path) || secretPathReason(path) !== undefined) {
        return true;
      }
      const last = globRegExp(path.split("/").at(-1) ?? path);
      return /[*?[]/.test(path.split("/").at(-1) ?? "") && SAMPLE_SECRET_FILES.some((name) => last.test(name));
    });
}

/**
 * The ignore files that can apply when `directory` is the build context: podman reads
 * `.containerignore` first and falls back to `.dockerignore`; Docker reads `.dockerignore`.
 * Dockerfile-specific ignore files (`<file>.dockerignore`) and podman's `--ignorefile` are
 * rejected elsewhere, because they replace the context's own ignore file.
 */
function ignoreFilesIn(directory: string): string[] {
  return [".containerignore", ".dockerignore"]
    .map((name) => join(directory, name))
    .filter((path) => existsSync(path));
}

/**
 * The directories a build command could use as the context of `buildFile`: its own directory
 * and every directory above it up to the repository root.
 */
function contextDirectories(root: string, buildFile: string): string[] {
  const directories: string[] = [];
  let directory = dirname(join(root, buildFile));
  for (;;) {
    directories.push(directory);
    const parent = dirname(directory);
    if (resolve(directory) === resolve(root) || parent === directory) {
      return directories;
    }
    directory = parent;
  }
}

const contextListings = new Map<string, string[]>();

/**
 * Every file a build with `directory` as the context could copy, relative to it and listed once
 * per run. Unlike `listFiles`, nothing is skipped (`node_modules`, `target`, `.git`, nested
 * checkouts can all be copied); only symbolic links to directories are not followed, as builders
 * copy the link itself.
 */
function contextFiles(directory: string, cache = true): string[] {
  let files = cache ? contextListings.get(directory) : undefined;
  if (files === undefined) {
    const found: string[] = [];
    const visit = (current: string) => {
      for (const name of readdirSync(current)) {
        const path = join(current, name);
        if (lstatSync(path).isDirectory()) {
          visit(path);
        } else {
          found.push(path);
        }
      }
    };
    visit(directory);
    files = found.map((path) => relative(directory, path).split(sep).join("/")).sort();
    if (cache) {
      contextListings.set(directory, files);
    }
  }
  return files;
}

/**
 * The key and credential files that a build with `directory` as its context would send to the
 * builder: every such file that the context's `.containerignore` or `.dockerignore` (either
 * one, as different builders pick different files) does not leave out. Lists the directory
 * afresh, for the build-time guard in scripts/lib/build-context.ts.
 */
export function secretFilesInContext(directory: string): string[] {
  const ignoreSets = ignoreFilesIn(directory).map((path) => ignorePatterns(readFileSync(path, "utf8")));
  if (ignoreSets.length === 0) {
    ignoreSets.push([]);
  }
  return contextFiles(directory, false).filter(
    (path) => secretPathReason(path) !== undefined && ignoreSets.some((patterns) => !isIgnored(patterns, path)),
  );
}

/**
 * Key and credential files of the repository that `COPY <source>` would put into the image.
 * The build command chooses the context, so every directory from the build file's own up to
 * the repository root counts as a possible context, each with the ignore files a builder may
 * pick there (or none). Returns paths relative to the repository root.
 */
function secretFilesCopied(root: string, buildFile: string, source: string): string[] {
  const found = new Set<string>();
  const pattern = globRegExp(source);
  for (const directory of contextDirectories(root, buildFile)) {
    const ignoreSets = ignoreFilesIn(directory).map((path) => ignorePatterns(readFileSync(path, "utf8")));
    if (ignoreSets.length === 0) {
      ignoreSets.push([]);
    }
    // An ignore file that holds only "*" lets nothing in, so there is nothing to list.
    if (!ignoreSets.every((patterns) => patterns.length > 0 && patterns.every((p) => DENY_ALL.has(p)))) {
      for (const path of contextFiles(directory)) {
        if (
          secretPathReason(path) !== undefined &&
          matchesOrParent(pattern, path) &&
          ignoreSets.some((patterns) => !isIgnored(patterns, path))
        ) {
          found.add(display(root, join(directory, path)));
        }
      }
    }
  }
  return [...found].sort();
}

function copyProblems(
  root: string,
  file: string,
  instruction: Instruction,
  args: string,
): Omit<Violation, "file">[] {
  const problems: Omit<Violation, "file">[] = [];
  // Flags come first in both forms (`COPY --chown=1:1 ["a", "b"]`), so take them off before
  // reading the sources as shell words or as a JSON array.
  let rest = args.trim();
  let fromStage = false;
  for (let flag = /^(--\S+)\s*/.exec(rest); flag !== null; flag = /^(--\S+)\s*/.exec(rest)) {
    if (/^--from=/.test(flag[1] ?? "")) {
      fromStage = true;
    }
    rest = rest.slice(flag[0].length);
  }
  const parts = words(rest);
  // The last word is the destination; heredoc sources are inline content.
  const sources = parts.slice(0, -1).filter((source) => !source.startsWith("<<"));
  for (const source of sources) {
    if (URL_WITH_USERINFO.test(source)) {
      problems.push({
        line: instruction.line,
        message: `${instruction.keyword} fetches a URL with credentials in it; fetch it in a RUN step with --mount=type=secret instead`,
      });
      continue;
    }
    if (source.includes("$")) {
      problems.push({
        line: instruction.line,
        message: `${instruction.keyword} ${source} uses a variable, so the copied path cannot be checked; write the path literally`,
      });
      continue;
    }
    const path = contextPath(source);
    const reason = secretPathReason(path);
    if (reason !== undefined) {
      problems.push({
        line: instruction.line,
        message: `${instruction.keyword} ${source} copies ${reason} into the image; build-time secrets go through RUN --mount=type=secret and runtime secrets through a Kubernetes Secret`,
      });
      continue;
    }
    if (fromStage) {
      continue;
    }
    const copied = secretFilesCopied(root, file, path);
    if (copied.length > 0) {
      problems.push({
        line: instruction.line,
        message: `${instruction.keyword} ${source} copies ${copied.join(", ")} from the build context into the image; leave key and credential files out with the ignore file, or move them out of the context`,
      });
    }
    if (isWholeContext(path)) {
      // Any of these directories can be the context, so each needs a deny-all ignore file;
      // otherwise files that git ignores (a local .env, a key) reach the build.
      const reasons: string[] = [];
      for (const directory of contextDirectories(root, file)) {
        const ignoreFiles = ignoreFilesIn(directory);
        if (ignoreFiles.length === 0) {
          const where = display(root, directory);
          reasons.push(`${where === "" ? "the repository root" : `${where}/`} has no ignore file`);
        }
        for (const path of ignoreFiles) {
          const text = readFileSync(path, "utf8");
          if (!ignoresEverything(text)) {
            reasons.push(`${display(root, path)} does not start with "*"`);
          }
          const reincluded = reincludedSecrets(text);
          if (reincluded.length > 0) {
            reasons.push(`${display(root, path)} brings back ${reincluded.join(", ")}`);
          }
        }
      }
      if (reasons.length > 0) {
        problems.push({
          line: instruction.line,
          message: `${instruction.keyword} ${source} copies the whole build context, but ${reasons.join("; ")}; start the ignore file with "*" and allow only the files the image needs with "!" lines, never key or credential files`,
        });
      }
    }
  }
  return problems;
}

function instructionProblems(
  root: string,
  file: string,
  instruction: Instruction,
): Omit<Violation, "file">[] {
  let { keyword, args } = instruction;
  if (keyword === "ONBUILD") {
    const match = /^(\S+)\s*(.*)$/.exec(args);
    keyword = (match?.[1] ?? "").toUpperCase();
    args = match?.[2] ?? "";
  }
  switch (keyword) {
    case "COPY":
    case "ADD":
      return copyProblems(root, file, { ...instruction, keyword }, args);
    case "ARG":
      return declaredNames(keyword, args)
        .filter((name) => SECRET_LIKE_NAME.test(name))
        .map((name) => ({
          line: instruction.line,
          message: `ARG ${name} looks like a secret; build arguments stay in the image history, so pass it with RUN --mount=type=secret,id=... instead`,
        }));
    case "ENV":
      return declaredNames(keyword, args)
        .filter((name) => SECRET_LIKE_NAME.test(name))
        .map((name) => ({
          line: instruction.line,
          message: `ENV ${name} looks like a secret and would stay in the image configuration; inject it at run time from a Kubernetes Secret`,
        }));
    default:
      return [];
  }
}

/** Checks one Dockerfile or Containerfile; `file` is relative to `root`. */
export function checkBuildFile(root: string, file: string, source: string): Violation[] {
  return parseBuildFile(source)
    .flatMap((instruction) => instructionProblems(root, file, instruction))
    .map((problem) => ({ file, ...problem }));
}

/** Rule 2: the repository root ignores everything when used as a build context. */
export function checkRootIgnore(root: string): Violation[] {
  const violations: Violation[] = [];
  const dockerignore = join(root, ".dockerignore");
  if (!existsSync(dockerignore)) {
    violations.push({
      file: ".dockerignore",
      line: 0,
      message: 'missing: the repository root must never be a build context, so .dockerignore must hold only "*"',
    });
  }
  for (const name of [".dockerignore", ".containerignore"]) {
    const path = join(root, name);
    if (existsSync(path) && !ignoresOnlyEverything(readFileSync(path, "utf8"))) {
      violations.push({
        file: name,
        line: 1,
        message: 'must hold only "*", with no "!" exceptions: the repository root is never a build context (it holds .env files, keys and .git)',
      });
    }
  }
  return violations;
}

// --- Build commands -------------------------------------------------------------------------

/** Container tools, including the standalone compose commands (`docker-compose`, `podman-compose`). */
const TOOL = String.raw`(?:podman|docker|buildah|nerdctl)(?:-compose)?`;
/** Command groups whose own subcommand can build: `image build`, `buildx build`, `buildx bake`, `compose build`. */
const BUILD_GROUP = String.raw`(?:image|buildx|builder|compose)`;
/** Subcommands that build images: `build`, buildah's `bud`, and buildx's `bake`. */
const BUILD_VERB = String.raw`(?:build|bud|bake)`;
/**
 * `podman build`, `podman image build`, `docker buildx build`, `docker builder build`,
 * `buildah bud` and the like, written as a shell command.
 */
const SHELL_BUILD = shellCommand(String.raw`${BUILD_VERB}\b`);
/**
 * `"build"` or `"bud"` in a command array right after a container tool, with an optional
 * `"image"`, `"buildx"` or `"builder"` in between (`[podman, "build"`,
 * `["docker", "image", "build"`), or first in an array handed to a function named after a
 * tool (`podmanOk(["build"`, `podmanOk(["image", "build"`). `["cargo", "build"]` is not a
 * container build.
 */
const ARRAY_BUILD = new RegExp(
  String.raw`\b\w*${TOOL}\w*["'\`]?\s*(?:,\s*|\(\s*\[\s*)(?:["'\`]${BUILD_GROUP}["'\`]\s*,\s*)?["'\`]${BUILD_VERB}["'\`]`,
  "i",
);

/**
 * A container tool on a shell command line, then any options (each with or without a value,
 * across `\` line continuations), an optional `image`, `buildx` or `builder` group with its own
 * options, and then `subcommand`: `docker --context prod build`, `podman --log-level info $OP`.
 * Shell quotes around the group or the subcommand do not change the command, so they are
 * allowed (`podman "build" .`, `docker "${OP}" .`).
 */
function shellCommand(subcommand: string): RegExp {
  const space = String.raw`[\s\\]+`;
  const options = String.raw`(?:-[^\s"'\`]*(?:${space}[^\s\\$"'\`-][^\s"'\`]*)?${space})*`;
  const group = String.raw`(?:(["']?)${BUILD_GROUP}\1${space}${options})?`;
  return new RegExp(String.raw`\b${TOOL}${space}${options}${group}["']?${subcommand}`, "i");
}
/** A string literal opening that is fixed text: a quote, or a backtick without `${`. */
const LITERAL = String.raw`(?:["']|\`(?![^\`]*\$\{))`;
/**
 * What may follow a container tool in a command array: fixed text that is not an option, or
 * `...containerBuildArgs(`. A computed element could be "build"; an option first (global
 * options such as `--context prod`) hides where the subcommand starts, and the scripts never
 * use them.
 */
const ALLOWED_NEXT = String.raw`(?!${LITERAL}(?!-)|\.\.\.\s*containerBuildArgs\s*\()`;
const SUBCOMMAND_GROUP = String.raw`(?:["'\`]${BUILD_GROUP}["'\`]\s*,\s*)?`;
/**
 * A command array that starts with a container tool (`[podman, op]`, `["docker", ...args]`,
 * `["sudo", "/usr/bin/podman", sub]`) and goes on with a subcommand that is not fixed text:
 * the check cannot tell whether it is "build". `...containerBuildArgs(` is the one allowed
 * non-literal.
 */
const DYNAMIC_ARRAY = new RegExp(
  String.raw`(?:\[\s*${TOOL}(?:Path|Bin|Binary|Cmd|Command|Exe)?\b|[\[,]\s*["'\`][^"'\`\n]*\b${TOOL}["'\`])\s*,\s*${SUBCOMMAND_GROUP}${ALLOWED_NEXT}[^\s\]]`,
  "i",
);
/** An array handed to a function named after a tool (`podmanOk([op, ...])`) that starts with a non-literal. */
const DYNAMIC_CALL = new RegExp(String.raw`\b\w*${TOOL}\w*\s*\(\s*\[\s*${SUBCOMMAND_GROUP}${ALLOWED_NEXT}[^\s\]]`, "i");
/**
 * Compose in any form (`docker compose up`, `podman compose run`, `docker-compose build`,
 * `["docker", "compose", ...]`): `up`, `run` and `create` build images from a compose file's
 * `build:` sections as well as `build` does, with contexts nothing here checks.
 */
const SHELL_COMPOSE = new RegExp(String.raw`\b(?:docker|podman|nerdctl)-compose\b|${shellCommand(String.raw`compose\b`).source}`, "i");
const ARRAY_COMPOSE = new RegExp(
  String.raw`\b\w*${TOOL}\w*["'\`]?\s*(?:,\s*|\(\s*\[\s*)["'\`]compose["'\`]|["'\`][^"'\`\n]*\b(?:docker|podman|nerdctl)-compose["'\`]`,
  "i",
);

/** A shell or template command whose subcommand comes from a variable (`podman ${op}`, `docker $CMD`). */
const SHELL_DYNAMIC = shellCommand(String.raw`[$\`]`);
/**
 * The same inside a script, where only text handed to a shell runs as a command: a Bun shell
 * template (`` $`podman ${op}` ``), the argument after `-c`, or `exec`/`execSync`. Other
 * strings, such as error messages, only mention the tool.
 */
const SCRIPT_SHELL_DYNAMIC = new RegExp(
  String.raw`(?:\$\s*|["'\`]-c["'\`]\s*,\s*|\bexec(?:Sync)?\s*\(\s*)["'\`][^"'\`]*${SHELL_DYNAMIC.source}`,
  "i",
);

/** The only file that may write a container build command: it checks the context it builds. */
const BUILD_WRAPPER = "scripts/lib/build-context.ts";
/**
 * The podman runner of the dev environment: it runs whatever arguments its callers pass, so
 * it may start a command array with `...args`, and in return it must refuse a build whose
 * arguments did not come from containerBuildArgs (`assertCheckedBuild(args)`).
 */
const TOOL_RUNNER = "scripts/lib/podman.ts";
const RUNNER_GUARD = /\bassertCheckedBuild\(args\)/;

/** Blanks out comments (keeping line breaks) so prose about commands is not read as code. */
function blankComments(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\/|^[ \t]*\/\/.*$/gm, (comment) => comment.replace(/[^\n]/g, " "));
}

/**
 * Rule 6: the context is chosen at build time and the static rules cannot see it, so every
 * container build in the scripts takes its arguments from `containerBuildArgs`
 * (scripts/lib/build-context.ts), which checks the very directory that podman then builds.
 * A build command written anywhere else (a command array with "build", a shell `podman build`,
 * a workflow step) is rejected.
 */
export function checkBuildInvocations(file: string, original: string): Violation[] {
  if (file === BUILD_WRAPPER) {
    return [];
  }
  const typescript = /\.[cm]?[jt]s$/.test(file);
  const source = typescript ? blankComments(original) : original;
  const violations: Violation[] = [];
  const build = typescript
    ? "writes a container build command; take the arguments from containerBuildArgs(context, options) in scripts/lib/build-context.ts, which checks the context it builds"
    : "builds a container image directly; call a script that builds through containerBuildArgs (scripts/lib/build-context.ts) instead";
  const dynamic =
    "runs a container tool whose subcommand is not fixed text right after the tool (it is computed at run time, or follows global options), so this check cannot tell whether it builds; write the subcommand first as a string literal, or build through containerBuildArgs (scripts/lib/build-context.ts)";
  const compose =
    "runs compose, which builds images from a compose file's build sections (with up, run and create too) without checking their contexts; build through containerBuildArgs (scripts/lib/build-context.ts) and run containers without compose";
  const rules: [RegExp, string][] = [
    [SHELL_BUILD, build],
    [typescript ? SCRIPT_SHELL_DYNAMIC : SHELL_DYNAMIC, dynamic],
  ];
  if (!typescript) {
    rules.push([SHELL_COMPOSE, compose]);
  }
  // Every array pattern names a container tool itself, so they run on every script.
  if (typescript) {
    rules.push([ARRAY_BUILD, build], [DYNAMIC_CALL, dynamic], [ARRAY_COMPOSE, compose]);
    if (file !== TOOL_RUNNER) {
      rules.push([DYNAMIC_ARRAY, dynamic]);
    }
  }
  for (const [pattern, message] of rules) {
    for (const found of source.matchAll(new RegExp(pattern.source, "g" + pattern.flags.replace("g", "")))) {
      const line = lineAt(source, found.index ?? 0);
      // One finding per line: `docker compose build` is a build and a compose command at once.
      if (!violations.some((violation) => violation.line === line)) {
        violations.push({ file, line, message });
      }
    }
  }
  if (file === TOOL_RUNNER && !RUNNER_GUARD.test(source)) {
    violations.push({
      file,
      line: 1,
      message: "runs podman with its callers' arguments but does not call assertCheckedBuild(args) (scripts/lib/build-context.ts) first, so a build that skipped containerBuildArgs would run",
    });
  }
  return violations.sort((a, b) => a.line - b.line);
}

// --- Build arguments in commands ------------------------------------------------------------

function lineAt(source: string, offset: number): number {
  let line = 1;
  for (let i = 0; i < offset; i += 1) {
    if (source.charCodeAt(i) === 10) {
      line += 1;
    }
  }
  return line;
}

/**
 * The `${{ ... }}` expressions in a text. An expression ends at the first `}}` outside a
 * single-quoted string (GitHub's only string syntax, with `''` as an escaped quote), so
 * braces inside strings, as in `format('{0}', ...)`, do not end it early. An unterminated
 * expression runs to the end of the text.
 */
export function githubExpressions(text: string): string[] {
  const found: string[] = [];
  let start = text.indexOf("${{");
  while (start >= 0) {
    let index = start + 3;
    let quoted = false;
    while (index < text.length) {
      if (text[index] === "'") {
        if (quoted && text[index + 1] === "'") {
          index += 2;
          continue;
        }
        quoted = !quoted;
      } else if (!quoted && text.startsWith("}}", index)) {
        index += 2;
        break;
      }
      index += 1;
    }
    found.push(text.slice(start, index));
    start = text.indexOf("${{", index);
  }
  return found;
}

/** The first expression in `text` that reads the secrets context (in any letter case). */
function secretsExpression(text: string): string | undefined {
  return githubExpressions(text).find((expression) => /\bsecrets\b/i.test(expression));
}
/**
 * The only GitHub expressions a build argument may use: public facts about the run. Everything
 * else can carry a secret (`github.token`, `env`, step outputs, inputs that relay one), so it
 * counts as a computed value.
 */
const SAFE_EXPRESSION =
  /\$\{\{\s*github\.(?:sha|ref|ref_name|run_id|run_number|run_attempt|repository|repository_owner|server_url)\s*\}\}/gi;
const COMPUTED =
  "only literal values and the public run facts github.sha, ref, ref_name, run_id, run_number, run_attempt, repository, repository_owner and server_url can be checked, so a computed value could carry a secret";
const NOT_LITERAL = "the build argument name must be written as one literal so that it can be checked";

/**
 * Checks one `NAME=value` (or bare `NAME`) build argument. `complete` is false when the
 * argument continues past the text that was read, for example `"NAME=" + value`.
 */
function buildArgProblem(argument: string, complete = true): string | undefined {
  const name = /^[A-Za-z_][A-Za-z0-9_]*/.exec(argument)?.[0];
  // The name must end inside the literal: `"NAME" + "_SUFFIX=..."` builds it at run time.
  if (name === undefined || !/^(=|$)/.test(argument.slice(name.length)) || (!complete && !argument.includes("="))) {
    return NOT_LITERAL;
  }
  if (SECRET_LIKE_NAME.test(name)) {
    return `build argument ${name} looks like a secret; build arguments stay in the image history, so use a secret mount (--secret, RUN --mount=type=secret) instead`;
  }
  if (secretsExpression(argument) !== undefined) {
    return `build argument ${name} reads a secret; use a secret mount (--secret, RUN --mount=type=secret) instead`;
  }
  if (!argument.includes("=")) {
    return `build argument ${name} has no value, so the builder takes it from the environment, which this check cannot see; write ${name}=value`;
  }
  const value = argument.slice(name.length + 1).replace(SAFE_EXPRESSION, "");
  if (!complete || /[$`]/.test(value)) {
    return `the value of build argument ${name} is computed; ${COMPUTED}`;
  }
  return undefined;
}

/**
 * Reads the argument that follows `--build-arg` in command text: a quoted literal (also the
 * next element of a command array, `"--build-arg", "NAME=value"`) or a shell word. Returns
 * undefined when the argument is an expression rather than text.
 */
function readBuildArg(rest: string): { argument: string; complete: boolean } | undefined {
  // Skip the separator: the quote that closes a `"--build-arg"` literal, then `=`, spaces, an
  // array comma or a line continuation.
  const separator = /^["'`]?(?:\\?\s|[,=])*/.exec(rest)?.[0] ?? "";
  const body = rest.slice(separator.length);
  const quote = /^["'`]/.exec(body)?.[0];
  // A shell word with its quotes removed (`NAME="value"`), `${{ ... }}` expressions included.
  const shellWord = (text: string) => /^(?:\$\{\{[^}]*\}\}|[^\s,\\])*/.exec(text)?.[0] ?? "";
  let argument: string;
  let after: string;
  if (quote !== undefined) {
    let end = 1;
    while (end < body.length && body[end] !== quote) {
      end += body[end] === "\\" ? 2 : 1;
    }
    argument = body.slice(1, end);
    after = body.slice(end + 1);
    // Shell text glued to the quoted part belongs to the same word: `"NAME"_X=value`.
    if (/^[^\s,\])\\;+]/.test(after)) {
      const glued = shellWord(after);
      argument += glued.replace(/["']/g, "");
      after = after.slice(glued.length);
    }
  } else {
    if (separator.includes(",")) {
      // The next element of a command array is an expression, not a string literal.
      return undefined;
    }
    const word = shellWord(body);
    argument = word.replace(/["']/g, "");
    after = body.slice(word.length);
  }
  // Concatenation (`"NAME=" + value`) continues the argument past the literal.
  const complete = !/^[ \t]*\+/.test(after);
  return { argument, complete };
}

/**
 * Rule 3 for command text (a script or a workflow file): finds every `--build-arg` and checks
 * the argument that follows it.
 */
export function checkBuildArgsInText(file: string, source: string): Violation[] {
  // The wrapper names these options only to refuse them, and checks every --build-arg it is
  // given at run time with the same rules.
  if (file === BUILD_WRAPPER) {
    return [];
  }
  const violations: Violation[] = [];
  for (const match of source.matchAll(/--build-arg(-file)?\b|--ignorefile\b/g)) {
    const offset = match.index ?? 0;
    const line = lineAt(source, offset);
    if (match[0] === "--ignorefile") {
      violations.push({
        file,
        line,
        message: "--ignorefile replaces the build context's own ignore file, which this check reads; keep the rules in the context's .containerignore or .dockerignore",
      });
      continue;
    }
    if (match[1] !== undefined) {
      violations.push({
        file,
        line,
        message: "--build-arg-file hides the build arguments from this check; pass each one with --build-arg NAME=value",
      });
      continue;
    }
    const read = readBuildArg(source.slice(offset + match[0].length));
    const problem = read === undefined ? NOT_LITERAL : buildArgProblem(read.argument, read.complete);
    if (problem !== undefined) {
      violations.push({ file, line, message: problem });
    }
  }
  return violations;
}

/** Steps of every job in a workflow, or of a composite action, with their key paths. */
function stepsOf(doc: unknown): [string, YamlMap][] {
  if (!isMap(doc)) {
    return [];
  }
  const found: [string, YamlMap][] = [];
  const collect = (steps: unknown, path: string) => {
    if (Array.isArray(steps)) {
      steps.forEach((step, index) => {
        if (isMap(step)) {
          found.push([`${path}[${index}]`, step]);
        }
      });
    }
  };
  if (isMap(doc.jobs)) {
    for (const [id, job] of Object.entries(doc.jobs)) {
      if (isMap(job)) {
        collect(job.steps, `jobs.${id}.steps`);
      }
    }
  }
  if (isMap(doc.runs)) {
    collect(doc.runs.steps, "runs.steps");
  }
  return found;
}

/** Finds the line of the first occurrence of `needle` in `source`, or 0. */
function lineOf(source: string, needle: string): number {
  const offset = source.indexOf(needle);
  return offset < 0 ? 0 : lineAt(source, offset);
}

/**
 * Where a workflow may read the secrets context: a step's `env` and `with`, the `secrets` a job
 * hands to a reusable workflow, and registry credentials of a job container or service.
 * Workflow- and job-level `env` would hand the secret to every step in scope.
 */
const SECRET_PLACES = [
  /^jobs\.[^.]+\.steps\[\d+\]\.(?:env|with)\./,
  /^jobs\.[^.]+\.secrets\./,
  /^jobs\.[^.]+\.container\.credentials\./,
  /^jobs\.[^.]+\.services\.[^.]+\.credentials\./,
  /^runs\.steps\[\d+\]\.(?:env|with)\./,
];
/**
 * Actions that build container images (`docker/build-push-action`, `docker/bake-action`,
 * `redhat-actions/buildah-build`, kaniko, ...). They send a context the checks cannot see,
 * possibly with files an earlier step wrote, so workflows build images through a script that
 * uses containerBuildArgs instead.
 */
const BUILD_ACTION = /^[^@\s]*(?:build-push|bake-action|buildah-build|podman-build|docker-build|image-build|kaniko|buildpacks)[^@\s]*(?:@|$)/i;

/** Step `run` scripts get their own message. */
const RUN_PLACE = /^(?:jobs\.[^.]+\.)?(?:runs\.)?steps\[\d+\]\.run$/;

/** Every string in a parsed YAML tree with its dotted path (`jobs.build.steps[0].env.X`). */
function stringLeaves(value: unknown, path: string): [string, string][] {
  if (typeof value === "string") {
    return [[path, value]];
  }
  if (Array.isArray(value)) {
    return value.flatMap((item, index) => stringLeaves(item, `${path}[${index}]`));
  }
  if (isMap(value)) {
    return Object.entries(value).flatMap(([key, item]) => stringLeaves(item, path === "" ? key : `${path}.${key}`));
  }
  return [];
}

/** Rules 3 and 4 for a workflow or local action file. */
export function checkWorkflowSecrets(file: string, source: string): Violation[] {
  let doc: unknown;
  try {
    doc = Bun.YAML.parse(source);
  } catch {
    // check-workflows.ts reports files that do not parse.
    return [];
  }
  const violations: Violation[] = [];
  for (const [path, text] of stringLeaves(doc, "")) {
    const expression = secretsExpression(text);
    if (expression === undefined || RUN_PLACE.test(path) || SECRET_PLACES.some((place) => place.test(path))) {
      continue;
    }
    violations.push({
      file,
      line: lineOf(source, expression),
      message: `${path} reads a secret outside a step's env or with; workflow- and job-level env hand it to every step in scope, so pass it to the one step that needs it`,
    });
  }
  for (const [path, step] of stepsOf(doc)) {
    const uses = typeof step.uses === "string" ? step.uses : "";
    if (BUILD_ACTION.test(uses)) {
      violations.push({
        file,
        line: lineOf(source, uses),
        message: `${path} builds an image with ${uses.split("@")[0]}, which sends a build context the checks cannot see; build through a script that takes its arguments from containerBuildArgs (scripts/lib/build-context.ts)`,
      });
    }
    const run = typeof step.run === "string" ? step.run : "";
    const expression = secretsExpression(run);
    if (expression !== undefined) {
      const runLine = run.split("\n").find((line) => line.includes(expression))?.trim() ?? expression;
      violations.push({
        file,
        line: lineOf(source, runLine),
        message: `${path}.run writes a secret into the script; pass it through the step's env and read the variable instead`,
      });
    }
    const buildArgs = isMap(step.with) ? step.with["build-args"] : undefined;
    if (typeof buildArgs === "string") {
      for (const argument of buildArgs.split("\n").map((entry) => entry.trim())) {
        const problem = argument === "" ? undefined : buildArgProblem(argument);
        if (problem !== undefined) {
          violations.push({ file, line: lineOf(source, argument), message: `${path}.with.build-args: ${problem}` });
        }
      }
    }
  }
  return violations;
}

// --- Secret manifests -----------------------------------------------------------------------

function documentsOf(source: string): unknown[] | undefined {
  try {
    // Bun returns an array for a multi-document stream; a `kind: List` carries its own items.
    const parsed: unknown = Bun.YAML.parse(source);
    const documents = Array.isArray(parsed) ? parsed : [parsed];
    return documents.flatMap((doc) => (isMap(doc) && Array.isArray(doc.items) ? [doc, ...doc.items] : [doc]));
  } catch {
    return undefined;
  }
}

/*
 * What the SOPS checks can and cannot prove. CI holds no decryption key, by design (GitHub
 * holds no deployment credential, docs/architecture/ci-release.md), so it cannot verify the
 * ciphertext cryptographically; Flux does that when it decrypts, and fails closed on a bad MAC.
 * These checks catch the realistic mistake: a Secret committed in plain text, base64-encoded,
 * or with a hand-made `ENC[...]` wrapper around readable text. Every value, IV, tag and MAC
 * must have SOPS's exact shape and lengths and decode to bytes that are not readable text, and
 * every age recipient must carry a wrapped data key in age's format. A person with write
 * access who deliberately forges random-looking bytes is outside what a lint can stop; review
 * and gitleaks cover that.
 */

/** A value encrypted by SOPS: AES-256-GCM with a 32-byte IV and a 16-byte tag. */
const SOPS_VALUE =
  /^ENC\[AES256_GCM,data:((?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?),iv:([A-Za-z0-9+/]{43}=),tag:([A-Za-z0-9+/]{22}==),type:(?:str|int|float|bool|bytes|comment)\]$/;
const AGE_RECIPIENT = /^age1[02-9ac-hj-np-z]{58}$/;
/** Ciphertext this long is almost never all printable by chance (below 0.3 % at 6 bytes). */
const MIN_TEXT_CHECK_BYTES = 6;

/** Whether every byte is printable ASCII or common whitespace, as plain text would be. */
function isReadableText(bytes: Uint8Array): boolean {
  return bytes.every((byte) => (byte >= 0x20 && byte < 0x7f) || byte === 0x09 || byte === 0x0a || byte === 0x0d);
}

/**
 * Whether a string is a SOPS-encrypted value: the exact `ENC[AES256_GCM,...]` shape, and data,
 * IV and tag that decode to bytes that do not read as text (data shorter than six bytes is too
 * short to judge).
 */
export function sopsValueValid(value: unknown): boolean {
  const match = typeof value === "string" ? SOPS_VALUE.exec(value) : null;
  if (match === null) {
    return false;
  }
  const [data, iv, tag] = [match[1] ?? "", match[2] ?? "", match[3] ?? ""].map((part) => Buffer.from(part, "base64"));
  if (data === undefined || iv === undefined || tag === undefined || iv.length !== 32 || tag.length !== 16) {
    return false;
  }
  return (
    !isReadableText(iv) && !isReadableText(tag) && (data.length < MIN_TEXT_CHECK_BYTES || !isReadableText(data))
  );
}

/** 32 bytes in age's unpadded base64. */
const AGE_32_BYTES = /^[A-Za-z0-9+/]{43}$/;

/**
 * Whether an armored age file is what age writes (age-encryption.org/v1): the version line;
 * one or more X25519 stanzas, each with a 32-byte ephemeral share and a body of exactly one
 * 32-byte wrapped file key that does not read as text (grease stanzas, `-> <x>-grease`, may
 * sit between them); the `---` line with a 32-byte header MAC; and a payload of at least a
 * nonce and a tag that does not read as text. A data key written in plain text, or any
 * stanza or MAC of the wrong shape, fails.
 */
function ageFileValid(armored: string): boolean {
  const body = /-----BEGIN AGE ENCRYPTED FILE-----\s*([A-Za-z0-9+/=\s]+?)\s*-----END AGE ENCRYPTED FILE-----/.exec(armored)?.[1];
  if (body === undefined) {
    return false;
  }
  const bytes = Buffer.from(body.replace(/\s+/g, ""), "base64");
  const text = bytes.toString("latin1");
  const macLine = text.indexOf("\n--- ");
  const headerEnd = macLine < 0 ? -1 : text.indexOf("\n", macLine + 1);
  if (headerEnd < 0 || !AGE_32_BYTES.test(text.slice(macLine + 5, headerEnd))) {
    return false;
  }
  const header = text.slice(0, macLine).split("\n");
  if (header[0] !== "age-encryption.org/v1") {
    return false;
  }
  let wrappedKeys = 0;
  let index = 1;
  while (index < header.length) {
    const stanza = /^-> (\S+)((?: \S+)*)$/.exec(header[index] ?? "");
    if (stanza === null) {
      return false;
    }
    index += 1;
    const lines: string[] = [];
    while (index < header.length && !(header[index] ?? "").startsWith("-> ")) {
      lines.push(header[index] ?? "");
      index += 1;
    }
    const type = stanza[1] ?? "";
    if (type.endsWith("-grease")) {
      continue;
    }
    const args = (stanza[2] ?? "").trim().split(" ");
    const wrapped = Buffer.from(lines[0] ?? "", "base64");
    if (
      type !== "X25519" ||
      args.length !== 1 ||
      !AGE_32_BYTES.test(args[0] ?? "") ||
      lines.length !== 1 ||
      !AGE_32_BYTES.test(lines[0] ?? "") ||
      wrapped.length !== 32 ||
      isReadableText(wrapped)
    ) {
      return false;
    }
    wrappedKeys += 1;
  }
  const payload = bytes.subarray(headerEnd + 1);
  return wrappedKeys > 0 && payload.length >= 32 && !isReadableText(payload);
}

/**
 * Whether a document's `sops` block is what SOPS writes for an age-encrypted file: a version,
 * a modification time, an encrypted MAC, and at least one age recipient with its wrapped data
 * key. A hand-written block does not pass.
 */
export function sopsMetadataValid(sops: unknown): boolean {
  if (!isMap(sops) || !Array.isArray(sops.age) || sops.age.length === 0) {
    return false;
  }
  const recipients = sops.age.every(
    (entry) =>
      isMap(entry) &&
      typeof entry.recipient === "string" &&
      AGE_RECIPIENT.test(entry.recipient) &&
      typeof entry.enc === "string" &&
      ageFileValid(entry.enc),
  );
  return (
    recipients &&
    sopsValueValid(sops.mac) &&
    typeof sops.version === "string" &&
    typeof sops.lastmodified === "string"
  );
}

function leafStrings(value: unknown): unknown[] {
  if (isMap(value)) {
    return Object.values(value).flatMap(leafStrings);
  }
  if (Array.isArray(value)) {
    return value.flatMap(leafStrings);
  }
  return [value];
}

/** A `kind:` line, possibly a list item, with an optional trailing comment. */
const KIND_LINE = /^([ \t]*)(-[ \t]+)?kind:[ \t]*(.*?)[ \t]*(?:#.*)?$/;
/** Lines that hold only template actions or comments; they do not end a YAML mapping. */
const NON_YAML_LINE = /^[ \t]*(?:\{\{.*\}\}[ \t]*|#.*)?$/;

/**
 * The keys of the mapping that holds the key at `column` on line `index`, found by indentation
 * the way YAML reads them. Template action lines are skipped, since text cannot show which of
 * their branches renders.
 */
function siblingKeys(lines: readonly string[], index: number, column: number, dashed: boolean): string[] {
  const keys: string[] = [];
  const keyAt = (line: string): { indent: number; key: string | undefined; dash: boolean } => {
    const match = /^([ \t]*)(-[ \t]+)?([^\s:#][^:#]*?)?[ \t]*(?::|$)/.exec(line);
    const indent = (match?.[1] ?? "").length;
    const dash = match?.[2] !== undefined;
    return { indent: indent + (match?.[2] ?? "").length, key: match?.[3]?.trim(), dash };
  };
  // Forward: the rest of this mapping.
  for (let i = index + 1; i < lines.length; i += 1) {
    const line = lines[i] ?? "";
    if (NON_YAML_LINE.test(line)) {
      continue;
    }
    const { indent, key, dash } = keyAt(line);
    if (indent < column || (indent === column && dash)) {
      break;
    }
    if (indent === column && key !== undefined) {
      keys.push(key);
    }
  }
  // Backward: the keys before it, up to the start of the mapping or list item.
  if (!dashed) {
    for (let i = index - 1; i >= 0; i -= 1) {
      const line = lines[i] ?? "";
      if (NON_YAML_LINE.test(line)) {
        continue;
      }
      const { indent, key, dash } = keyAt(line);
      if (indent < column) {
        break;
      }
      if (indent === column && key !== undefined) {
        keys.push(key);
        if (dash) {
          break;
        }
      }
    }
  }
  return keys;
}

/** The label that marks a Secret the chart declares empty for the migration Job to fill. */
const FILLED_BY_LINE = /^[ \t]*trustiot\.io\/filled-by:[ \t]*migrate[ \t]*$/;
/** The only keys such a Secret may have: no `data`, no `stringData`, nothing that could carry them. */
const EMPTY_SECRET_KEYS = new Set(["apiVersion", "kind", "metadata", "type"]);
/**
 * The only lines with a template action that such a Secret may hold, compared whole (trimmed):
 * its name from the helper's list, the chart's standard labels, and the end of the range that
 * declares one per consumer. Anything else (a templated annotation or label, a value from
 * `.Values`, a function call, text after an action on the same line) is refused: a heuristic
 * for "renders nothing" was bypassed in Laysi/trustiot-v2#60 round 4 (R1), so the exception is
 * exact. The chart tests check the rendered Secrets key for key.
 */
const FILLED_SECRET_ACTIONS = new Set([
  "name: {{ .name }}",
  '{{- include "trustiot.componentLabels" (dict "root" $ "name" .component) | nindent 4 }}',
  "{{- end }}",
]);

/**
 * The lines of the mapping that holds the key at `column` on line `index`, up to a document
 * separator, a list item, or a line indented less.
 */
function mappingLines(lines: readonly string[], index: number, column: number, dashed: boolean): number[] {
  const indentOf = (line: string) => {
    const match = /^([ \t]*)(-[ \t]+)?/.exec(line);
    return { indent: (match?.[1] ?? "").length + (match?.[2] ?? "").length, dash: match?.[2] !== undefined };
  };
  const separator = (line: string) => /^(---|\.\.\.)([ \t]|$)/.test(line);
  const inside: number[] = [index];
  for (let i = index + 1; i < lines.length; i += 1) {
    const line = lines[i] ?? "";
    if (separator(line)) break;
    if (!NON_YAML_LINE.test(line)) {
      const { indent, dash } = indentOf(line);
      if (indent < column || (indent === column && dash)) break;
    }
    inside.push(i);
  }
  if (!dashed) {
    for (let i = index - 1; i >= 0; i -= 1) {
      const line = lines[i] ?? "";
      if (separator(line)) break;
      if (!NON_YAML_LINE.test(line)) {
        const { indent, dash } = indentOf(line);
        if (indent < column) break;
        inside.push(i);
        if (indent === column && dash) break;
        continue;
      }
      inside.push(i);
    }
  }
  return inside.sort((a, b) => a - b);
}

/**
 * Whether the Secret declared on line `index` is one the chart may declare: empty, for the
 * migration Job to fill (docs/architecture/secrets.md). It carries the label
 * `trustiot.io/filled-by: migrate`; its keys are only apiVersion, kind, metadata and type (no
 * `data` or `stringData`); and every line with a template action is one of
 * `FILLED_SECRET_ACTIONS`, word for word. The chart tests check the rendered Secrets too.
 */
function isEmptyFilledSecret(lines: readonly string[], index: number, column: number, dashed: boolean): boolean {
  const inside = mappingLines(lines, index, column, dashed);
  if (!inside.some((i) => FILLED_BY_LINE.test(lines[i] ?? ""))) return false;
  for (const i of inside) {
    const line = lines[i] ?? "";
    if (line.includes("{{")) {
      if (!FILLED_SECRET_ACTIONS.has(line.trim())) return false;
      continue;
    }
    // A key of the Secret itself (at the column of its kind), or anything deeper.
    const key = /^([ \t]*)(?:-[ \t]+)?([^\s:#][^:#]*?)[ \t]*:/.exec(line);
    const indent = /^([ \t]*(?:-[ \t]+)?)/.exec(line)?.[1]?.length ?? 0;
    if (!NON_YAML_LINE.test(line) && indent === column && (key === null || !EMPTY_SECRET_KEYS.has(key[2] ?? ""))) {
      return false;
    }
  }
  return true;
}

/**
 * Rule 5 for a template that is not plain YAML (a Helm `.yaml` template or `.tpl` helper): it
 * fails when it declares a Secret, or a kind that is itself templated and could render one,
 * because what it renders cannot be verified here. The chart tests check rendered output.
 * A `kind: Secret` in a reference to a Secret by name (a Gateway's `certificateRefs`, with
 * `name` or `group` beside it and no `apiVersion`) is not a declaration. The one exception is an
 * empty Secret the migration Job fills (`isEmptyFilledSecret`): it carries no value to verify.
 */
export function checkTemplateText(file: string, source: string): Violation[] {
  const lines = source.split(/\r?\n/);
  const violations: Violation[] = [...checkTemplateFunctions(file, source)];
  lines.forEach((line, index) => {
    const match = KIND_LINE.exec(line);
    if (match === null) {
      return;
    }
    const value = (match[3] ?? "").replace(/^(["'])(.*)\1$/, "$2");
    if (!/^secret$/i.test(value) && !value.includes("{{")) {
      return;
    }
    const dashed = match[2] !== undefined;
    const siblings = siblingKeys(lines, index, (match[1] ?? "").length + (match[2] ?? "").length, dashed);
    if (!siblings.includes("apiVersion") && (siblings.includes("name") || siblings.includes("group"))) {
      return;
    }
    const column = (match[1] ?? "").length + (match[2] ?? "").length;
    if (/^secret$/i.test(value) && isEmptyFilledSecret(lines, index, column, dashed)) {
      return;
    }
    violations.push({
      file,
      line: index + 1,
      message: "a template that declares a Secret (or a templated kind) cannot be verified as encrypted; keep Secrets in SOPS-encrypted YAML files, and let the chart reference Secrets by name only",
    });
  });
  return violations;
}

/**
 * Template functions that make secret material or read it from the cluster (rule 5): `lookup`
 * (reads any object the installing identity can read, Secrets included), the `rand*` and
 * `uuid*` functions and `shuffle` (random values a template would put into a manifest), the
 * `gen*` certificate and key functions, `derivePassword`, `htpasswd`, `bcrypt`, the AES
 * functions, and `getHostByName` (renders what the network answers). A chart template has no
 * business with any of them: passwords come from the migration Job, everything else from
 * Secrets the chart only names. The rule covers every Helm template and helper in the repository
 * and lives here, in the check that runs on every pull request, rather than in the chart's own
 * tests, which run only when the chart changes.
 */
const FORBIDDEN_TEMPLATE_FUNCTION =
  /(?<![.$\w])(lookup|rand[A-Za-z]*|uuidv[0-9]|shuffle|gen(?:CA|CAWithKey|PrivateKey|SelfSignedCert|SelfSignedCertWithKey|SignedCert|SignedCertWithKey)|derivePassword|htpasswd|bcrypt|encryptAES|decryptAES|getHostByName)\b/;

/**
 * The code of every template action, with its offset: string literals ("…" with escapes, `…`,
 * '…' runes) and comments blanked, the way Go's template lexer reads them, so a `}}` inside a
 * string does not end the action and a name inside a string or comment is not a call. An action
 * left open at the end of the file runs to the end.
 */
export function templateActions(source: string): { offset: number; code: string }[] {
  const actions: { offset: number; code: string }[] = [];
  let i = 0;
  while (true) {
    const start = source.indexOf("{{", i);
    if (start < 0) break;
    let code = "";
    let j = start + 2;
    while (j < source.length) {
      const c = source[j] ?? "";
      if (source.startsWith("}}", j)) {
        j += 2;
        break;
      }
      if (source.startsWith("/*", j)) {
        const end = source.indexOf("*/", j + 2);
        j = end < 0 ? source.length : end + 2;
        code += " ";
        continue;
      }
      if (c === '"' || c === "'") {
        j += 1;
        while (j < source.length && source[j] !== c) j += source[j] === "\\" ? 2 : 1;
        j += 1;
        code += '""';
        continue;
      }
      if (c === "`") {
        const end = source.indexOf("`", j + 1);
        j = end < 0 ? source.length : end + 1;
        code += '""';
        continue;
      }
      code += c;
      j += 1;
    }
    actions.push({ offset: start, code });
    i = j;
  }
  return actions;
}

/** Every use of a forbidden function in a template action (strings and comments excluded). */
export function checkTemplateFunctions(file: string, source: string): Violation[] {
  const violations: Violation[] = [];
  for (const action of templateActions(source)) {
    // Function names only: not a field (.Values.lookup) or a variable.
    const used = FORBIDDEN_TEMPLATE_FUNCTION.exec(action.code);
    if (used === null) continue;
    violations.push({
      file,
      line: lineAt(source, action.offset),
      message: `template function ${used[1]} makes or reads secret material at render time; a chart template may not use lookup, rand*, uuid*, shuffle, gen* keys and certificates, derivePassword, htpasswd, bcrypt, the AES functions or getHostByName (docs/architecture/secrets.md)`,
    });
  }
  return violations;
}

/** Rule 5 for one YAML file; files that do not parse go through `checkTemplateText`. */
export function checkSecretManifests(file: string, source: string): Violation[] {
  // A Helm template can parse as YAML (`kind: "{{ .Values.kind }}"`), so templated files are
  // judged by their text as well. Workflows use `${{ }}` and are not Kubernetes manifests.
  if (source.includes("{{") && !file.startsWith(".github/")) {
    const templated = checkTemplateText(file, source);
    if (templated.length > 0) {
      return templated;
    }
  }
  const documents = documentsOf(source);
  if (documents === undefined) {
    return checkTemplateText(file, source);
  }
  const violations: Violation[] = [];
  for (const doc of documents) {
    if (!isMap(doc)) {
      continue;
    }
    if (doc.kind === "Secret") {
      const values = [doc.data, doc.stringData].flatMap((block) => (block === undefined ? [] : leafStrings(block)));
      const encrypted = sopsMetadataValid(doc.sops) && values.every(sopsValueValid);
      if (values.length > 0 && !encrypted) {
        const name = isMap(doc.metadata) ? String(doc.metadata.name) : "(unnamed)";
        violations.push({
          file,
          line: lineAt(source, /kind["']?\s*:\s*["']?Secret\b/.exec(source)?.index ?? 0),
          message: `Secret ${name} is not SOPS-encrypted; encrypt it with sops (docs/architecture/secrets.md) or create it outside the repository`,
        });
      }
    }
    // Its literals, files and env files are plain text this check cannot verify.
    if (doc.secretGenerator !== undefined) {
      violations.push({
        file,
        line: lineOf(source, "secretGenerator"),
        message: "secretGenerator builds Secrets from literals or files that are not encrypted; commit a SOPS-encrypted Secret manifest instead",
      });
    }
  }
  return violations;
}

// --- Repository -----------------------------------------------------------------------------

export interface Result {
  /** Container build files found. */
  buildFiles: string[];
  /** Files checked in total. */
  files: number;
  violations: Violation[];
}

/** Applies every rule to the repository under `root`. */
export function checkRepository(root: string): Result {
  const files = listFiles(root);
  const violations: Violation[] = [...checkRootIgnore(root)];
  const buildFiles: string[] = [];
  let checked = 0;
  for (const file of files) {
    const name = posix.basename(file);
    if (IGNORE_FILE.test(name) && name !== ".dockerignore" && name !== ".containerignore") {
      violations.push({
        file,
        line: 0,
        message: "a Dockerfile-specific ignore file replaces the build context's own ignore file, so a build from another context could pick up files this check never sees; use .containerignore or .dockerignore in the image's own directory",
      });
    }
    const inGithub = file.startsWith(".github/");
    const build = isBuildFile(file);
    const script = SCRIPT_FILE.test(file) && !TEST_FILE.test(file) && !file.startsWith(EVIDENCE_DIR) && file !== SELF;
    const yaml = YAML_FILE.test(file);
    const template = file.endsWith(".tpl");
    const json = JSON_FILE.test(file);
    if (!build && !script && !yaml && !template && !json) {
      continue;
    }
    const source = readFileSync(join(root, file), "utf8");
    checked += 1;
    if (build) {
      buildFiles.push(file);
      violations.push(...checkBuildFile(root, file, source));
    }
    if (script || (yaml && inGithub)) {
      violations.push(...checkBuildArgsInText(file, source));
      violations.push(...checkBuildInvocations(file, source));
    }
    if (yaml && inGithub) {
      violations.push(...checkWorkflowSecrets(file, source));
    }
    if (yaml || json) {
      violations.push(...checkSecretManifests(file, source));
    }
    if (template) {
      violations.push(...checkTemplateText(file, source));
    }
  }
  return { buildFiles, files: checked, violations };
}

/** Formats a violation as a GitHub Actions error annotation. */
export function annotation(violation: Violation): string {
  const escapeData = (text: string) =>
    text.replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A");
  const escapeProperty = (text: string) =>
    escapeData(text).replaceAll(":", "%3A").replaceAll(",", "%2C");
  const line = violation.line > 0 ? `,line=${violation.line}` : "";
  return (
    `::error file=${escapeProperty(violation.file)}${line},title=${escapeProperty("secret handling")}::` +
    escapeData(violation.message)
  );
}

/** Runs the check for the command line and returns the exit status. */
export function main(args: readonly string[], env: Record<string, string | undefined>): number {
  const root = resolve(args[0] ?? join(import.meta.dir, "..", ".."));
  let result: Result;
  try {
    result = checkRepository(root);
  } catch (error) {
    console.error(`secret handling: cannot read the repository under ${root}: ${String(error)}`);
    return 2;
  }
  for (const violation of result.violations) {
    const where = violation.line > 0 ? `${violation.file}:${violation.line}` : violation.file;
    console.log(`${where}: ${violation.message}`);
    if (env.GITHUB_ACTIONS === "true") {
      console.log(annotation(violation));
    }
  }
  const builds = `${result.buildFiles.length} container build file(s)`;
  if (result.violations.length > 0) {
    console.log(`secret handling: ${result.violations.length} violation(s); checked ${result.files} file(s), ${builds}`);
    return 1;
  }
  console.log(`secret handling: ${result.files} file(s) checked, ${builds}, no violations`);
  return 0;
}

if (import.meta.main) {
  process.exit(main(Bun.argv.slice(2), process.env));
}
