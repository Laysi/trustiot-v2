// The bundled Postgres image of the nightly (Laysi/trustiot-v2#1126; docs/architecture/ci-release.md,
// nightly.yml, 「做什麼」 item 5). The nightly deploys the image that release.yml published, pulled
// from GHCR by the digest pinned in postgres-image.json, instead of building deploy/images/postgres
// (apt from snapshot.debian.org) on every run. It builds from source only when the Containerfile
// differs from the one the pinned release built: then the published image is not what the next
// release will build.
//
//   bun scripts/nightly/postgres-image.ts pull
//   bun scripts/nightly/postgres-image.ts update-pin --version <version>
//
// pull: nightly.yml's step "Pull the published Postgres image", the only step that gets the job's
// token. It logs in with a throwaway DOCKER_CONFIG (the token on stdin only), pulls the pinned
// digest with retries, removes the configuration, checks the platform and tags the image
// POSTGRES_IMAGE; run.ts then imports that image (loadPostgresImage). When the Containerfile
// differs it pulls nothing and says why in the log, a notice and the job summary. A pull that keeps
// failing fails the step: building instead would bring snapshot.debian.org back and hide a broken
// pin.
//   GITHUB_TOKEN          the job's token (packages: read); required in GitHub Actions. Elsewhere,
//                         without it the pull is anonymous, which works while the package is public.
//   GITHUB_ACTOR          the user name of the login (GitHub Actions sets it)
//   GITHUB_STEP_SUMMARY   when set, the decision is appended to it
//
// update-pin: after a release whose deploy/images/postgres changed. Verifies the cosign signature
// of ghcr.io/laysi/trustiot-v2/postgres:<version> against release.yml's identity at the tag v<version>
// and its commit (as release.yml's verify job does), takes the digest from the verified signature,
// and the Containerfile's git blob from the tag, and rewrites postgres-image.json. Needs the tag
// (`git fetch --tags origin`), cosign (`proto install cosign`) and, once the package is private,
// `cosign login ghcr.io` with an account that can read it.
//
// Exit status: 0 on success (for pull also when the nightly is to build the image), 1 when the pull
// or the update failed, 2 on a usage error.
import { appendFileSync, chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { containerBuildArgs } from "../lib/build-context.ts";
import { REGISTRY, isMap, keylessVerifyArgs, parseReleaseTag, signedManifestDigests } from "../release/lib.ts";
import { exec, pinnedTool, retry, run, RunLog } from "./exec.ts";
import { importImageIntoCluster, POSTGRES_IMAGE } from "./platform.ts";
import type { ImportCommands, Platform } from "./platform.ts";

const REPO_ROOT = resolve(import.meta.dir, "..", "..");

/** The pin, relative to the repository root. */
export const PIN_PATH = "scripts/nightly/postgres-image.json";
/** The image's build context and its Containerfile, the only file the build reads from it. */
export const POSTGRES_CONTEXT = "deploy/images/postgres";
export const CONTAINERFILE_PATH = `${POSTGRES_CONTEXT}/Containerfile`;
/** Where release.yml publishes the image, and the repository whose release.yml signs it. */
export const POSTGRES_REPOSITORY = `${REGISTRY}/postgres`;
export const RELEASE_REPOSITORY = "Laysi/trustiot-v2";
export const REGISTRY_HOST = REGISTRY.split("/")[0] ?? "";
/** The platform of every image we ship (decision 5). */
export const POSTGRES_PLATFORM = "linux/arm64";
/** The command that rewrites the pin; `<version>` is the release's version without the `v`. */
export const UPDATE_PIN_COMMAND = "bun scripts/nightly/postgres-image.ts update-pin --version <version>";

export const PULL_ATTEMPTS = 4;
export const LOGIN_ATTEMPTS = 3;
const PAUSE_MS = 15_000;

export interface PostgresPin {
  /** The release that published the image, without the `v`, for example `0.0.0-rc.15`. */
  version: string;
  /** `ghcr.io/laysi/trustiot-v2/postgres@sha256:<digest>`. */
  image: string;
  /** The git blob of deploy/images/postgres/Containerfile at that release's tag. */
  containerfile: string;
}

const PIN_IMAGE = new RegExp(`^${POSTGRES_REPOSITORY.replace(/[.]/g, "\\.")}@sha256:[0-9a-f]{64}$`);
const GIT_OBJECT = /^[0-9a-f]{40}$/;

/** Reads and checks the pin; every field must be exact, so a loose value fails here and not in the cluster. */
export function parsePostgresPin(text: string, where = PIN_PATH): PostgresPin {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    throw new Error(`${where} is not JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (!isMap(value)) throw new Error(`${where} must be a JSON object`);
  const keys = Object.keys(value).sort();
  if (keys.join(",") !== "containerfile,image,version") {
    throw new Error(`${where} must have exactly the keys containerfile, image and version; it has ${keys.join(", ") || "none"}`);
  }
  const { version, image, containerfile } = value;
  if (typeof version !== "string") throw new Error(`${where}: version must be a string`);
  try {
    parseReleaseTag(`v${version}`);
  } catch (error) {
    throw new Error(`${where}: version ${JSON.stringify(version)} is not a release version: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (typeof image !== "string" || !PIN_IMAGE.test(image)) {
    throw new Error(`${where}: image must be ${POSTGRES_REPOSITORY}@sha256:<64 lowercase hex digits>; got ${JSON.stringify(image)}`);
  }
  if (typeof containerfile !== "string" || !GIT_OBJECT.test(containerfile)) {
    throw new Error(`${where}: containerfile must be a git blob id (40 lowercase hex digits); got ${JSON.stringify(containerfile)}`);
  }
  return { version, image, containerfile };
}

export function renderPostgresPin(pin: PostgresPin): string {
  return `${JSON.stringify({ version: pin.version, image: pin.image, containerfile: pin.containerfile }, null, 2)}\n`;
}

/** The id git gives a file's content (`git hash-object`), without running git. */
export function gitBlobId(content: Uint8Array): string {
  return new Bun.CryptoHasher("sha1").update(`blob ${content.byteLength}\0`).update(content).digest("hex");
}

/**
 * What a Containerfile reads besides its own text: COPY without `--from`, ADD (a local file or a
 * URL), and RUN bind mounts without `from` (their default source is the build context). The pin
 * compares the Containerfile alone, which is sound only while this list is empty.
 */
export function inputsOutsideContainerfile(containerfile: string): string[] {
  const instructions = containerfile
    .replace(/\\\r?\n/g, " ")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("#"));
  const found: string[] = [];
  for (const instruction of instructions) {
    const keyword = (/^(\S+)/.exec(instruction)?.[1] ?? "").toUpperCase();
    const options = instruction.split(/\s+/).filter((word) => word.startsWith("--"));
    if (keyword === "ADD") {
      found.push(instruction);
    } else if (keyword === "COPY" && !options.some((option) => option.startsWith("--from="))) {
      found.push(instruction);
    } else if (keyword === "RUN") {
      for (const mount of options.filter((option) => option.startsWith("--mount="))) {
        const fields = mount.slice("--mount=".length).split(",");
        const type = fields.find((field) => field.startsWith("type="))?.slice("type=".length) ?? "bind";
        if (type === "bind" && !fields.some((field) => field.startsWith("from="))) found.push(instruction);
      }
    }
  }
  return found;
}

export type PostgresSource = { pull: true } | { pull: false; reason: string };

/** Pull when the Containerfile is the one the pinned release built and it reads nothing else. */
export function postgresSource(pin: PostgresPin, containerfile: Uint8Array): PostgresSource {
  const update = `after the next release, run \`${UPDATE_PIN_COMMAND}\` with its version and commit ${PIN_PATH}`;
  const outside = inputsOutsideContainerfile(new TextDecoder().decode(containerfile));
  if (outside.length > 0) {
    return {
      pull: false,
      reason:
        `building the Postgres image from source: ${CONTAINERFILE_PATH} reads files besides itself (${outside.join("; ")}), ` +
        `so its git blob no longer identifies the image; compare the inputs it reads in scripts/nightly/postgres-image.ts first`,
    };
  }
  const blob = gitBlobId(containerfile);
  if (blob === pin.containerfile) return { pull: true };
  return {
    pull: false,
    reason:
      `building the Postgres image from source: ${CONTAINERFILE_PATH} (git blob ${blob}) is not the one release ${pin.version} built ` +
      `(${pin.containerfile}, ${PIN_PATH}), so the published image is not what the next release will build; ${update}`,
  };
}

/** The pin and the decision for this checkout. */
export function readPostgresSource(repoRoot: string): { pin: PostgresPin; source: PostgresSource } {
  const pin = parsePostgresPin(readFileSync(join(repoRoot, PIN_PATH), "utf8"));
  return { pin, source: postgresSource(pin, new Uint8Array(readFileSync(join(repoRoot, CONTAINERFILE_PATH)))) };
}

interface InspectedImage {
  id: string;
  platform: string;
}

/** `docker image inspect` of one reference, or the reason it cannot be read. Its output is not logged. */
async function inspectImage(ref: string, commands: Pick<ImportCommands, "exec">): Promise<InspectedImage | string> {
  const result = await commands.exec(["docker", "image", "inspect", ref], { timeoutMs: 60_000 });
  if (result.code !== 0) {
    const why = result.timedOut ? "timed out" : (result.stderr.trim() || `exited with ${result.code}`).split("\n").at(-1);
    return `docker image inspect ${ref}: ${why}`;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(result.stdout);
  } catch {
    return `docker image inspect ${ref} did not print JSON`;
  }
  const image: unknown = Array.isArray(parsed) && parsed.length === 1 ? parsed[0] : undefined;
  if (!isMap(image) || typeof image.Id !== "string" || image.Id === "") return `docker image inspect ${ref} did not describe one image`;
  return { id: image.Id, platform: `${String(image.Os)}/${String(image.Architecture)}` };
}

/**
 * Why the local POSTGRES_IMAGE is not the pinned image, or undefined when it is: it must be the
 * very image the pinned digest reference resolves to (the same image ID; docker resolves a
 * digest reference only to content with that manifest digest), and linux/arm64.
 */
export async function localImageProblem(pin: PostgresPin, commands: Pick<ImportCommands, "exec">): Promise<string | undefined> {
  const pinned = await inspectImage(pin.image, commands);
  if (typeof pinned === "string") return `the pinned image is not here (${pinned})`;
  const local = await inspectImage(POSTGRES_IMAGE, commands);
  if (typeof local === "string") return `${POSTGRES_IMAGE} is not here (${local})`;
  if (local.id !== pinned.id) return `${POSTGRES_IMAGE} is image ${local.id}, not ${pin.image} (${pinned.id})`;
  if (local.platform !== POSTGRES_PLATFORM) return `${pin.image} is ${local.platform}, expected ${POSTGRES_PLATFORM}`;
  return undefined;
}

/** Replaces every occurrence of the token, in case a tool ever echoes it into an error. */
export function redact(text: string, secret: string | undefined): string {
  return secret === undefined || secret === "" ? text : text.split(secret).join("***");
}

export interface PullOptions {
  log: RunLog;
  commands?: ImportCommands;
  /** Where the throwaway DOCKER_CONFIG directory is made; RUNNER_TEMP in the nightly. */
  tempRoot?: string;
  attempts?: number;
  pauseMs?: number;
}

/**
 * The login must have stored the credential in the throwaway configuration, not in a credential
 * helper (pass, secretservice) that outlives it. The pre-seeded entry makes docker use the file;
 * this checks that it did.
 */
function requireCredentialInConfig(configDir: string): void {
  const config: unknown = JSON.parse(readFileSync(join(configDir, "config.json"), "utf8"));
  const auths = isMap(config) && isMap(config.auths) ? config.auths : undefined;
  const entry = auths !== undefined && isMap(auths[REGISTRY_HOST]) ? auths[REGISTRY_HOST] : undefined;
  const helpers = isMap(config) && (config.credsStore !== undefined || config.credHelpers !== undefined);
  if (helpers || typeof entry?.auth !== "string" || entry.auth === "") {
    throw new Error(`docker login did not store the ${REGISTRY_HOST} credential in the throwaway DOCKER_CONFIG; refusing to go on`);
  }
}

/**
 * Pulls the pinned image and tags it POSTGRES_IMAGE (see the header). The credential lives only in
 * a 0700 directory made for this call, which is removed before the function returns, also when it
 * fails; the runner's default docker configuration is never read or written.
 */
export async function pullPinnedImage(
  pin: PostgresPin,
  credentials: { user: string; token: string } | undefined,
  { log, commands = { run, exec }, tempRoot = tmpdir(), attempts = PULL_ATTEMPTS, pauseMs = PAUSE_MS }: PullOptions,
): Promise<void> {
  const failure = (reason: string) =>
    new Error(
      `could not pull the Postgres image ${pin.image} of release ${pin.version} (pinned in ${PIN_PATH}): ${redact(reason, credentials?.token)}. ` +
        "The nightly does not build it instead; check the package and the pin",
    );
  const configDir = mkdtempSync(join(tempRoot, "nightly-docker-config-"));
  try {
    chmodSync(configDir, 0o700);
    // An entry for the host makes docker keep the login in this file instead of a credential
    // helper it would otherwise look for (docker's config.json documentation, credentials store).
    writeFileSync(join(configDir, "config.json"), `${JSON.stringify({ auths: { [REGISTRY_HOST]: {} } })}\n`, { mode: 0o600 });
    const docker = { env: { DOCKER_CONFIG: configDir } };
    if (credentials === undefined) {
      log.line(`no GITHUB_TOKEN: pulling ${pin.image} anonymously, which works only while the package is public`);
    } else {
      await retry(log, LOGIN_ATTEMPTS, pauseMs, () =>
        commands.run(log, ["docker", "login", REGISTRY_HOST, "--username", credentials.user, "--password-stdin"], {
          ...docker,
          stdin: credentials.token,
          timeoutMs: 120_000,
        }),
      );
      requireCredentialInConfig(configDir);
    }
    await retry(log, attempts, pauseMs, () => commands.run(log, ["docker", "pull", pin.image], { ...docker, timeoutMs: 900_000 }));
  } catch (error) {
    throw failure(error instanceof Error ? error.message : String(error));
  } finally {
    if (credentials !== undefined) {
      // Only this configuration holds the login; logging out first also clears a credential
      // helper, should docker ever have used one despite the check above.
      await commands.exec(["docker", "logout", REGISTRY_HOST], { env: { DOCKER_CONFIG: configDir }, timeoutMs: 60_000 }).catch(() => undefined);
    }
    rmSync(configDir, { recursive: true, force: true });
  }
  const pulled = await inspectImage(pin.image, commands);
  if (typeof pulled === "string") throw failure(pulled);
  if (pulled.platform !== POSTGRES_PLATFORM) throw failure(`the pulled image is ${pulled.platform}, expected ${POSTGRES_PLATFORM}`);
  await commands.run(log, ["docker", "tag", pin.image, POSTGRES_IMAGE], { timeoutMs: 60_000 });
  const problem = await localImageProblem(pin, commands);
  if (problem !== undefined) throw failure(problem);
  log.line(`pulled ${pin.image} (release ${pin.version}, ${pulled.platform}) and tagged it ${POSTGRES_IMAGE}`);
}

/**
 * The image run.ts deploys as the bundled Postgres: the pinned image that the pull step left as
 * POSTGRES_IMAGE when the Containerfile is the pinned release's, otherwise a build from source as
 * before (a local run may also hold an older image under that name). Either way it is imported into
 * every node and confirmed there (importImageIntoCluster). Returns which it was.
 */
export async function loadPostgresImage(
  platform: Pick<Platform, "repoRoot" | "k3d" | "cluster" | "log">,
  commands: ImportCommands = { run, exec },
  importOptions: { attempts?: number; pauseMs?: number } = {},
): Promise<"pulled" | "built"> {
  const { log } = platform;
  const { pin, source } = readPostgresSource(platform.repoRoot);
  let pulled = false;
  if (source.pull) {
    const problem = await localImageProblem(pin, commands);
    if (problem === undefined) {
      log.line(`using the published Postgres image ${pin.image} (release ${pin.version}) as ${POSTGRES_IMAGE}; not building ${POSTGRES_CONTEXT}`);
      pulled = true;
    } else {
      log.line(`building the Postgres image from source: ${problem}`);
    }
  } else {
    log.line(source.reason);
  }
  if (!pulled) {
    const context = join(platform.repoRoot, POSTGRES_CONTEXT);
    // Natively on the arm64 runner; the Containerfile's build stage compiles AGE.
    await retry(log, 2, 10_000, () =>
      // containerBuildArgs checks the context and the Containerfile first (docs/architecture/secrets.md).
      commands.run(log, ["docker", ...containerBuildArgs(context, ["--tag", POSTGRES_IMAGE, "--file", join(context, "Containerfile")])], {
        timeoutMs: 900_000,
      }),
    );
  }
  await importImageIntoCluster(platform, POSTGRES_IMAGE, commands, importOptions);
  return pulled ? "pulled" : "built";
}

/**
 * Rewrites the pin from a published release (see the header): the digest comes from the
 * signature that cosign verified, never from the tag alone.
 */
export async function updatePin(
  version: string,
  { repoRoot, cosign, log, commands = { run, exec } }: { repoRoot: string; cosign: string; log: RunLog; commands?: Pick<ImportCommands, "run"> },
): Promise<PostgresPin> {
  const tag = parseReleaseTag(`v${version}`).tag;
  const git = async (spec: string, what: string) => {
    try {
      const value = (await commands.run(log, ["git", "-C", repoRoot, "rev-parse", "--verify", spec], { timeoutMs: 60_000 })).stdout.trim();
      if (!GIT_OBJECT.test(value)) throw new Error(`git printed ${JSON.stringify(value)}`);
      return value;
    } catch (error) {
      throw new Error(`cannot read ${what} of ${tag} (run \`git fetch --tags origin\`): ${error instanceof Error ? error.message : String(error)}`);
    }
  };
  const commit = await git(`${tag}^{commit}`, "the commit");
  const containerfile = await git(`${tag}:${CONTAINERFILE_PATH}`, CONTAINERFILE_PATH);
  const ref = `${POSTGRES_REPOSITORY}:${version}`;
  const verified = await commands.run(
    log,
    [cosign, "verify", ...keylessVerifyArgs({ repository: RELEASE_REPOSITORY, tag, commit }), "--output", "json", ref],
    { timeoutMs: 300_000, quiet: true },
  );
  let claims: unknown;
  try {
    claims = JSON.parse(verified.stdout);
  } catch {
    throw new Error(`cosign verify ${ref} did not print JSON`);
  }
  const digests = [...new Set(signedManifestDigests(claims))];
  if (digests.length !== 1) {
    throw new Error(`cosign verify ${ref} must vouch for exactly one image digest; it vouched for ${digests.length === 0 ? "none" : digests.join(", ")}`);
  }
  const pin = parsePostgresPin(JSON.stringify({ version, image: `${POSTGRES_REPOSITORY}@${digests[0]}`, containerfile }), "the new pin");
  writeFileSync(join(repoRoot, PIN_PATH), renderPostgresPin(pin));
  log.line(`verified ${ref} against ${tag} (${commit}); wrote ${PIN_PATH}: ${pin.image}, Containerfile blob ${pin.containerfile}`);
  return pin;
}

/** A GitHub Actions workflow command's message is one line; `%`, CR and LF are escaped. */
export function workflowCommandMessage(text: string): string {
  return text.replace(/%/g, "%25").replace(/\r/g, "%0D").replace(/\n/g, "%0A");
}

/** Takes the token out of the environment, so no command this process starts inherits it. */
export function takeToken(env: Record<string, string | undefined>): string | undefined {
  const token = env.GITHUB_TOKEN;
  delete env.GITHUB_TOKEN;
  return token === undefined || token === "" ? undefined : token;
}

async function pullCommand(env: Record<string, string | undefined>): Promise<number> {
  const token = takeToken(env);
  const log = new RunLog();
  const summary = env.GITHUB_STEP_SUMMARY;
  const report = (markdown: string) => {
    if (summary !== undefined && summary !== "") appendFileSync(summary, `${markdown}\n\n`);
  };
  try {
    const { pin, source } = readPostgresSource(REPO_ROOT);
    if (!source.pull) {
      log.line(source.reason);
      if (env.GITHUB_ACTIONS === "true") console.log(`::notice title=Postgres image built from source::${workflowCommandMessage(source.reason)}`);
      report(`**Postgres image:** ${source.reason}`);
      return 0;
    }
    const user = env.GITHUB_ACTOR ?? "";
    if (env.GITHUB_ACTIONS === "true" && (token === undefined || user === "")) {
      console.error("GITHUB_TOKEN and GITHUB_ACTOR must be set to pull the Postgres image in GitHub Actions");
      return 2;
    }
    const credentials = token === undefined ? undefined : { user: user || "token", token };
    await pullPinnedImage(pin, credentials, { log, tempRoot: env.RUNNER_TEMP || tmpdir() });
    report(`**Postgres image:** pulled \`${pin.image}\` (release ${pin.version}) from the pin in \`${PIN_PATH}\`; not built.`);
    return 0;
  } catch (error) {
    console.error(redact(error instanceof Error ? error.message : String(error), token));
    return 1;
  }
}

async function updatePinCommand(args: string[]): Promise<number> {
  const index = args.indexOf("--version");
  const version = index === -1 ? undefined : args[index + 1];
  if (version === undefined || args.length !== 2) {
    console.error(`usage: ${UPDATE_PIN_COMMAND}`);
    return 2;
  }
  try {
    const log = new RunLog();
    await updatePin(version, { repoRoot: REPO_ROOT, cosign: await pinnedTool("cosign", REPO_ROOT), log });
    return 0;
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    return 1;
  }
}

if (import.meta.main) {
  const [command, ...args] = Bun.argv.slice(2);
  if (command === "pull" && args.length === 0) {
    process.exit(await pullCommand(process.env));
  } else if (command === "update-pin") {
    process.exit(await updatePinCommand(args));
  } else {
    console.error(`usage: bun scripts/nightly/postgres-image.ts pull\n       ${UPDATE_PIN_COMMAND}`);
    process.exit(2);
  }
}
