import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { CommandError, exec, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import { POSTGRES_IMAGE } from "./platform.ts";
import type { ImportCommands } from "./platform.ts";
import {
  CONTAINERFILE_PATH,
  LOGIN_ATTEMPTS,
  PIN_PATH,
  POSTGRES_REPOSITORY,
  PULL_ATTEMPTS,
  REGISTRY_HOST,
  UPDATE_PIN_COMMAND,
  gitBlobId,
  inputsOutsideContainerfile,
  loadPostgresImage,
  localImageProblem,
  parsePostgresPin,
  postgresSource,
  pullPinnedImage,
  redact,
  renderPostgresPin,
  takeToken,
  unexpectedContextFiles,
  updatePin,
  workflowCommandMessage,
} from "./postgres-image.ts";
import type { PostgresPin } from "./postgres-image.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const DIGEST = `sha256:${"d".repeat(64)}`;
const OTHER_DIGEST = `sha256:${"e".repeat(64)}`;
const CONTAINERFILE = new TextEncoder().encode("FROM scratch\n");
const PIN: PostgresPin = { version: "0.0.0-rc.15", image: `${POSTGRES_REPOSITORY}@${DIGEST}`, containerfile: gitBlobId(CONTAINERFILE) };
// Built at run time, so no token-shaped literal sits in the repository.
const TOKEN = ["ghs", "x".repeat(8), "test", "y".repeat(12)].join("_");

const ok = (stdout = ""): ExecResult => ({ code: 0, stdout, stderr: "", timedOut: false });
const failed = (stderr: string): ExecResult => ({ code: 1, stdout: "", stderr, timedOut: false });

let dir: string;
let log: RunLog;
let logPath: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "nightly-postgres-"));
  logPath = join(dir, "run.log");
  log = new RunLog(logPath);
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

describe("the pin", () => {
  test("the committed pin is exact and names the published image by digest", () => {
    const pin = parsePostgresPin(readFileSync(join(REPO_ROOT, PIN_PATH), "utf8"));
    expect(pin.image).toStartWith(`${POSTGRES_REPOSITORY}@sha256:`);
    // The file is written the way update-pin writes it.
    expect(renderPostgresPin(pin)).toBe(readFileSync(join(REPO_ROOT, PIN_PATH), "utf8"));
  });

  test("rejects a loose or malformed pin", () => {
    const text = (change: Record<string, unknown>) => JSON.stringify({ ...PIN, ...change });
    expect(() => parsePostgresPin("not json")).toThrow("is not JSON");
    expect(() => parsePostgresPin("[]")).toThrow("JSON object");
    expect(() => parsePostgresPin(text({ tree: "a".repeat(40) }))).toThrow("exactly the keys");
    expect(() => parsePostgresPin(JSON.stringify({ version: PIN.version, image: PIN.image }))).toThrow("exactly the keys");
    expect(() => parsePostgresPin(text({ version: "v0.0.0-rc.15" }))).toThrow("not a release version");
    expect(() => parsePostgresPin(text({ version: "latest" }))).toThrow("not a release version");
    // A tag is not a pin, and nothing but the release's repository is accepted.
    expect(() => parsePostgresPin(text({ image: `${POSTGRES_REPOSITORY}:0.0.0-rc.15` }))).toThrow("image must be");
    expect(() => parsePostgresPin(text({ image: `${POSTGRES_REPOSITORY}:0.0.0-rc.15@${DIGEST}` }))).toThrow("image must be");
    expect(() => parsePostgresPin(text({ image: `docker.io/library/postgres@${DIGEST}` }))).toThrow("image must be");
    expect(() => parsePostgresPin(text({ image: `ghcrXio/laysi/trustiot-v2/postgres@${DIGEST}` }))).toThrow("image must be");
    expect(() => parsePostgresPin(text({ image: `${POSTGRES_REPOSITORY}@sha256:${"D".repeat(64)}` }))).toThrow("image must be");
    expect(() => parsePostgresPin(text({ containerfile: "abc" }))).toThrow("containerfile must be");
    expect(parsePostgresPin(text({}))).toEqual(PIN);
  });

  test("the blob id is git's", () => {
    // `git hash-object /dev/null` and `printf 'hello\n' | git hash-object --stdin`.
    expect(gitBlobId(new Uint8Array())).toBe("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    expect(gitBlobId(new TextEncoder().encode("hello\n"))).toBe("ce013625030ba8dba906f756967f9e9ca394464a");
  });
});

describe("pull or build", () => {
  test("the real Containerfile reads nothing besides itself, so its blob identifies the image", () => {
    // If this fails, the build reads files from deploy/images/postgres: compare them too before
    // the nightly may pull instead of building (postgresSource refuses to pull meanwhile).
    expect(inputsOutsideContainerfile(readFileSync(join(REPO_ROOT, CONTAINERFILE_PATH), "utf8"))).toEqual([]);
  });

  test("finds every way a Containerfile reads the build context or a URL", () => {
    expect(inputsOutsideContainerfile("FROM a AS base\nCOPY --from=base /x /y\nRUN --mount=type=cache,target=/c true\n")).toEqual([]);
    expect(inputsOutsideContainerfile("COPY --chown=26 age.so /lib/\n")).toEqual(["COPY --chown=26 age.so /lib/"]);
    expect(inputsOutsideContainerfile("copy . /src\n")).toEqual(["copy . /src"]);
    expect(inputsOutsideContainerfile("ADD https://example.org/a.tgz /a\n")).toHaveLength(1);
    expect(inputsOutsideContainerfile("RUN --mount=target=/src make\n")).toHaveLength(1);
    expect(inputsOutsideContainerfile("RUN --mount=type=bind,target=/src make\n")).toHaveLength(1);
    expect(inputsOutsideContainerfile("RUN --mount=type=bind,from=build,target=/src make\n")).toEqual([]);
    // A continued line is one instruction; a comment is none.
    expect(inputsOutsideContainerfile("RUN set -eu; \\\n  true\nCOPY \\\n  . /src\n# COPY . /x\n")).toEqual(["COPY . /src"]);
    // BuildKit's line rules (security review of Laysi/trustiot-v2#1126): a comment ending in a
    // backslash continues nothing, comment and blank lines inside a continuation are skipped,
    // spaces after the backslash still continue, option values are case-insensitive.
    expect(inputsOutsideContainerfile("# note \\\nCOPY . /x\n")).toEqual(["COPY . /x"]);
    expect(inputsOutsideContainerfile("RUN \\\n# c\n\n  --mount=type=bind,target=/s make\n")).toEqual(["RUN --mount=type=bind,target=/s make"]);
    expect(inputsOutsideContainerfile("COPY \\  \n . /x\n")).toEqual(["COPY . /x"]);
    expect(inputsOutsideContainerfile("RUN --mount=type=BIND,target=/s make\n")).toHaveLength(1);
    expect(inputsOutsideContainerfile("RUN --mount=type=bind,from=,target=/s make\n")).toHaveLength(1);
    expect(inputsOutsideContainerfile("COPY --from= . /x\n")).toHaveLength(1);
    // Another continuation character is not followed, so it counts as an input.
    expect(inputsOutsideContainerfile("# escape=`\nFROM scratch\n")).toEqual(["an escape parser directive"]);
  });

  test("the real build context holds nothing but the Containerfile and the files it never reads", () => {
    expect(unexpectedContextFiles(join(REPO_ROOT, "deploy", "images", "postgres"))).toEqual([]);
  });

  test("builds when the build context holds a file the pin does not cover", () => {
    const context = join(dir, "context");
    mkdirSync(join(context, "files"), { recursive: true });
    for (const name of ["Containerfile", "README.md", "image.test.ts", "files/age.conf"]) writeFileSync(join(context, name), "");
    expect(unexpectedContextFiles(context)).toEqual(["files/age.conf"]);
    const source = postgresSource(PIN, CONTAINERFILE, unexpectedContextFiles(context));
    expect(source.pull).toBe(false);
    expect(source.pull ? "" : source.reason).toContain("holds files the pin does not cover (files/age.conf)");
  });

  test("pulls when the Containerfile is the pinned release's", () => {
    expect(postgresSource(PIN, CONTAINERFILE)).toEqual({ pull: true });
  });

  test("builds when the Containerfile changed, and says why and how to update the pin", () => {
    const changed = new TextEncoder().encode("FROM scratch\nUSER 26\n");
    const source = postgresSource(PIN, changed);
    expect(source.pull).toBe(false);
    const reason = source.pull ? "" : source.reason;
    expect(reason).toContain(`${CONTAINERFILE_PATH} (git blob ${gitBlobId(changed)}) is not the one release 0.0.0-rc.15 built`);
    expect(reason).toContain(UPDATE_PIN_COMMAND);
    expect(reason).toContain(PIN_PATH);
    expect(reason).not.toContain("\n");
  });

  test("builds when the Containerfile starts reading the build context, even with the pinned blob", () => {
    const reads = new TextEncoder().encode("FROM scratch\nCOPY age.so /lib/\n");
    const source = postgresSource({ ...PIN, containerfile: gitBlobId(reads) }, reads);
    expect(source.pull).toBe(false);
    expect(source.pull ? "" : source.reason).toContain("reads files besides itself (COPY age.so /lib/)");
  });
});

interface Call {
  cmd: string[];
  env: Record<string, string>;
  stdin?: string;
  /** For calls with DOCKER_CONFIG: whether the directory existed and its mode at that moment. */
  configMode?: number;
  configFile?: string;
}

/**
 * A docker that records every call. `login` writes the credential into DOCKER_CONFIG the way the
 * file store does, unless `storeCredential` is false; `pull` makes the pinned image (platform
 * `platform`) exist; `tag` names it POSTGRES_IMAGE.
 */
function fakeDocker({
  loginFailures = 0,
  pullFailures = 0,
  platform = "linux/arm64",
  storeCredential = true,
  echoToken = false,
}: { loginFailures?: number; pullFailures?: number; platform?: string; storeCredential?: boolean; echoToken?: boolean } = {}) {
  const calls: Call[] = [];
  const images = new Map<string, { Id: string; Os: string; Architecture: string }>();
  let logins = 0;
  let pulls = 0;
  const record = (cmd: string[], options?: ExecOptions): Call => {
    const env = options?.env ?? {};
    const call: Call = { cmd, env, ...(options?.stdin === undefined ? {} : { stdin: options.stdin }) };
    if (env.DOCKER_CONFIG !== undefined && existsSync(env.DOCKER_CONFIG)) {
      call.configMode = statSync(env.DOCKER_CONFIG).mode & 0o777;
      call.configFile = readFileSync(join(env.DOCKER_CONFIG, "config.json"), "utf8");
    }
    calls.push(call);
    return call;
  };
  const commands: ImportCommands = {
    run: async (runLog, cmd, options) => {
      runLog.line(`$ ${cmd.join(" ")}`);
      record(cmd, options);
      if (cmd[1] === "login") {
        if (++logins <= loginFailures) {
          throw new CommandError(cmd, failed(echoToken ? `Error response from daemon: denied for ${options?.stdin}` : "Error response from daemon: unauthorized"));
        }
        if (storeCredential) {
          const file = join(options!.env!.DOCKER_CONFIG!, "config.json");
          const auth = Buffer.from(`user:${options!.stdin}`).toString("base64");
          writeFileSync(file, JSON.stringify({ auths: { [REGISTRY_HOST]: { auth } } }));
        }
        return ok("Login Succeeded");
      }
      if (cmd[1] === "pull") {
        if (++pulls <= pullFailures) throw new CommandError(cmd, failed("Error response from daemon: Get https://ghcr.io/v2/: net/http: TLS handshake timeout"));
        const [os, architecture] = platform.split("/");
        images.set(cmd[2]!, { Id: `sha256:${"1".repeat(64)}`, Os: os!, Architecture: architecture! });
        return ok();
      }
      if (cmd[1] === "tag") {
        images.set(cmd[3]!, images.get(cmd[2]!)!);
        return ok();
      }
      throw new Error(`unexpected command ${cmd.join(" ")}`);
    },
    exec: async (cmd, options) => {
      record(cmd, options);
      if (cmd[1] === "logout") return ok();
      if (cmd[1] === "image" && cmd[2] === "inspect") {
        const image = images.get(cmd[3]!);
        return image === undefined ? failed(`Error response from daemon: No such image: ${cmd[3]}`) : ok(JSON.stringify([image]));
      }
      throw new Error(`unexpected command ${cmd.join(" ")}`);
    },
  };
  return { calls, images, commands };
}

describe("pullPinnedImage", () => {
  const credentials = { user: "nightly-actor", token: TOKEN };
  const pull = (commands: ImportCommands, attempts = PULL_ATTEMPTS, withCredentials = true) =>
    pullPinnedImage(PIN, withCredentials ? credentials : undefined, { log, commands, tempRoot: dir, attempts, pauseMs: 0 });
  const configDirs = () => readdirSync(dir).filter((name) => name.startsWith("nightly-docker-config-"));

  /** Every place the token must never be: argv, environments, the log. */
  function expectTokenOnlyOnStdin(calls: Call[]): void {
    for (const call of calls) {
      expect(call.cmd.join(" ")).not.toContain(TOKEN);
      expect(JSON.stringify(call.env)).not.toContain(TOKEN);
      expect(call.env.GITHUB_TOKEN).toBeUndefined();
      if (call.cmd[1] !== "login") expect(call.stdin).toBeUndefined();
    }
    expect(readFileSync(logPath, "utf8")).not.toContain(TOKEN);
  }

  test("logs in with the token on stdin into a throwaway 0700 DOCKER_CONFIG, pulls by digest, then removes it", async () => {
    const { calls, images, commands } = fakeDocker();
    await pull(commands);
    expect(calls.map((call) => call.cmd.slice(0, 3).join(" "))).toEqual([
      `docker login ${REGISTRY_HOST}`,
      `docker pull ${PIN.image}`,
      `docker logout ${REGISTRY_HOST}`,
      `docker image inspect`,
      `docker tag ${PIN.image}`,
      "docker image inspect",
      "docker image inspect",
    ]);
    const [login, pulled, logout] = calls;
    expect(login?.cmd).toEqual(["docker", "login", REGISTRY_HOST, "--username", "nightly-actor", "--password-stdin"]);
    expect(login?.stdin).toBe(TOKEN);
    // One directory, made for this call, private, pre-seeded so docker keeps the login in it.
    const config = login?.env.DOCKER_CONFIG;
    expect(config).toStartWith(join(dir, "nightly-docker-config-"));
    expect(login?.configMode).toBe(0o700);
    expect(JSON.parse(login?.configFile ?? "")).toEqual({ auths: { [REGISTRY_HOST]: {} } });
    expect(pulled?.env.DOCKER_CONFIG).toBe(config!);
    expect(pulled?.configMode).toBe(0o700);
    expect(logout?.env.DOCKER_CONFIG).toBe(config!);
    // The calls that need no credential do not see the configuration.
    for (const call of calls.slice(3)) expect(call.env.DOCKER_CONFIG).toBeUndefined();
    expect(existsSync(config!)).toBe(false);
    expect(configDirs()).toEqual([]);
    expect(images.get(POSTGRES_IMAGE)).toEqual(images.get(PIN.image)!);
    expect(readFileSync(logPath, "utf8")).toContain(`pulled ${PIN.image} (release 0.0.0-rc.15, linux/arm64) and tagged it ${POSTGRES_IMAGE}`);
    expectTokenOnlyOnStdin(calls);
  });

  test("retries the pull, then passes", async () => {
    const { calls, commands } = fakeDocker({ pullFailures: PULL_ATTEMPTS - 1 });
    await pull(commands);
    expect(calls.filter((call) => call.cmd[1] === "pull")).toHaveLength(PULL_ATTEMPTS);
    expect(readFileSync(logPath, "utf8")).toContain(`attempt ${PULL_ATTEMPTS - 1}/${PULL_ATTEMPTS} failed`);
    expect(configDirs()).toEqual([]);
  });

  test("a pull that keeps failing fails, naming the image and the pin, removes the configuration, and builds nothing", async () => {
    const { calls, images, commands } = fakeDocker({ pullFailures: PULL_ATTEMPTS });
    const failure = pull(commands);
    await expect(failure).rejects.toThrow(`could not pull the Postgres image ${PIN.image} of release 0.0.0-rc.15 (pinned in ${PIN_PATH})`);
    await expect(failure).rejects.toThrow("TLS handshake timeout");
    await expect(failure).rejects.toThrow("does not build it instead");
    expect(calls.filter((call) => call.cmd[1] === "pull")).toHaveLength(PULL_ATTEMPTS);
    expect(calls.some((call) => call.cmd.includes("build") || call.cmd[1] === "tag")).toBe(false);
    expect(calls.filter((call) => call.cmd[1] === "logout")).toHaveLength(1);
    expect(configDirs()).toEqual([]);
    expect(images.has(POSTGRES_IMAGE)).toBe(false);
    expectTokenOnlyOnStdin(calls);
  });

  test("a login that keeps failing fails without pulling, removes the configuration, and keeps the token out of the error", async () => {
    const { calls, commands } = fakeDocker({ loginFailures: LOGIN_ATTEMPTS, echoToken: true });
    const failure = pull(commands);
    await expect(failure).rejects.toThrow("could not pull the Postgres image");
    await failure.catch((error: Error) => {
      expect(error.message).not.toContain(TOKEN);
      expect(error.message).toContain("denied for ***");
    });
    expect(calls.filter((call) => call.cmd[1] === "login")).toHaveLength(LOGIN_ATTEMPTS);
    expect(calls.some((call) => call.cmd[1] === "pull")).toBe(false);
    expect(configDirs()).toEqual([]);
  });

  test("refuses to go on when docker kept the login anywhere but the throwaway configuration", async () => {
    const { calls, commands } = fakeDocker({ storeCredential: false });
    await expect(pull(commands)).rejects.toThrow("did not store the ghcr.io credential in the throwaway DOCKER_CONFIG");
    expect(calls.some((call) => call.cmd[1] === "pull")).toBe(false);
    // Logging out also clears a credential helper, should docker have used one.
    expect(calls.filter((call) => call.cmd[1] === "logout")).toHaveLength(1);
    expect(configDirs()).toEqual([]);
  });

  test("an image of another platform fails the step and is not tagged", async () => {
    const { calls, images, commands } = fakeDocker({ platform: "linux/amd64" });
    await expect(pull(commands)).rejects.toThrow("the pulled image is linux/amd64, expected linux/arm64");
    expect(calls.some((call) => call.cmd[1] === "tag")).toBe(false);
    expect(images.has(POSTGRES_IMAGE)).toBe(false);
    expect(configDirs()).toEqual([]);
  });

  test("without credentials (outside GitHub Actions) it pulls anonymously and never logs in", async () => {
    const { calls, commands } = fakeDocker();
    await pull(commands, PULL_ATTEMPTS, false);
    expect(calls.some((call) => call.cmd[1] === "login" || call.cmd[1] === "logout")).toBe(false);
    expect(readFileSync(logPath, "utf8")).toContain(`no GITHUB_TOKEN: pulling ${PIN.image} anonymously`);
    expect(configDirs()).toEqual([]);
  });
});

describe("the token", () => {
  test("is taken out of the environment, so no command inherits it", () => {
    const env: Record<string, string | undefined> = { GITHUB_TOKEN: TOKEN, GITHUB_ACTOR: "a" };
    expect(takeToken(env)).toBe(TOKEN);
    expect(env).toEqual({ GITHUB_ACTOR: "a" });
    expect(takeToken({ GITHUB_TOKEN: "" })).toBeUndefined();
    expect(takeToken({})).toBeUndefined();
  });

  test("a real command started after taking it does not see it", async () => {
    const before = process.env.GITHUB_TOKEN;
    process.env.GITHUB_TOKEN = TOKEN;
    try {
      expect(takeToken(process.env)).toBe(TOKEN);
      const result = await exec(["env"]);
      expect(result.code).toBe(0);
      expect(result.stdout).not.toContain(TOKEN);
      expect(result.stdout).not.toMatch(/^GITHUB_TOKEN=/m);
    } finally {
      if (before === undefined) delete process.env.GITHUB_TOKEN;
      else process.env.GITHUB_TOKEN = before;
    }
  });

  test("is redacted wherever it appears", () => {
    expect(redact(`a ${TOKEN} b ${TOKEN}`, TOKEN)).toBe("a *** b ***");
    expect(redact("a", undefined)).toBe("a");
    expect(redact("a", "")).toBe("a");
  });

  test("a notice stays one workflow command", () => {
    expect(workflowCommandMessage("50%\r\nnext")).toBe("50%25%0D%0Anext");
  });
});

describe("loadPostgresImage", () => {
  const K3D = "/pinned/k3d";
  const NODE = "k3d-nightly-server-0";

  /** A repository with the pin and a Containerfile; the pinned one unless `containerfile` is given. */
  function repository(containerfile: Uint8Array = CONTAINERFILE): string {
    const root = join(dir, "repo");
    mkdirSync(join(root, "scripts", "nightly"), { recursive: true });
    mkdirSync(join(root, "deploy", "images", "postgres"), { recursive: true });
    writeFileSync(join(root, PIN_PATH), renderPostgresPin(PIN));
    writeFileSync(join(root, CONTAINERFILE_PATH), containerfile);
    return root;
  }

  /** A docker that holds `images` and a cluster with one node that gets whatever is imported. */
  function fakeHost(images: Record<string, { Id: string; Os: string; Architecture: string }>) {
    const calls: string[][] = [];
    const commands: ImportCommands = {
      run: async (runLog, cmd) => {
        runLog.line(`$ ${cmd.join(" ")}`);
        calls.push(cmd);
        return ok();
      },
      exec: async (cmd) => {
        calls.push(cmd);
        if (cmd[0] === K3D) return ok(JSON.stringify([{ name: NODE, role: "server", runtimeLabels: { "k3d.cluster": "nightly" } }]));
        if (cmd[1] === "exec") return ok("{}");
        const image = images[cmd[3]!];
        return image === undefined ? failed(`Error: No such image: ${cmd[3]}`) : ok(JSON.stringify([image]));
      },
    };
    return { calls, commands };
  }
  const arm64 = (id: string) => ({ Id: `sha256:${id.repeat(64)}`, Os: "linux", Architecture: "arm64" });
  const load = (root: string, commands: ImportCommands) =>
    loadPostgresImage({ repoRoot: root, k3d: K3D, cluster: "nightly", log }, commands, { attempts: 1, pauseMs: 0 });
  const built = (calls: string[][]) => calls.some((cmd) => cmd[0] === "docker" && cmd[1] === "build");
  const imported = (calls: string[][]) => calls.filter((cmd) => cmd[0] === K3D && cmd[1] === "image").map((cmd) => cmd[3]);

  test("imports the pulled image, without building, when it is the pinned digest's image", async () => {
    const { calls, commands } = fakeHost({ [PIN.image]: arm64("1"), [POSTGRES_IMAGE]: arm64("1") });
    expect(await load(repository(), commands)).toBe("pulled");
    expect(built(calls)).toBe(false);
    expect(imported(calls)).toEqual([POSTGRES_IMAGE]);
    expect(readFileSync(logPath, "utf8")).toContain(`using the published Postgres image ${PIN.image} (release 0.0.0-rc.15)`);
  });

  test("builds when the pull step left nothing, or the local name holds another image", async () => {
    const cases: Record<string, ReturnType<typeof arm64>>[] = [{}, { [POSTGRES_IMAGE]: arm64("1") }, { [PIN.image]: arm64("1"), [POSTGRES_IMAGE]: arm64("2") }];
    for (const images of cases) {
      const { calls, commands } = fakeHost(images);
      expect(await load(repository(), commands)).toBe("built");
      const build = calls.find((cmd) => cmd[1] === "build");
      expect(build).toContain(POSTGRES_IMAGE);
      expect(build).toContain(join(dir, "repo", "deploy", "images", "postgres"));
      expect(imported(calls)).toEqual([POSTGRES_IMAGE]);
    }
    expect(readFileSync(logPath, "utf8")).toContain("building the Postgres image from source: the pinned image is not here");
    expect(readFileSync(logPath, "utf8")).toContain(`${POSTGRES_IMAGE} is image sha256:${"2".repeat(64)}, not ${PIN.image}`);
  });

  test("builds when the Containerfile changed, even with the pinned image at hand", async () => {
    const { calls, commands } = fakeHost({ [PIN.image]: arm64("1"), [POSTGRES_IMAGE]: arm64("1") });
    expect(await load(repository(new TextEncoder().encode("FROM scratch\nUSER 26\n")), commands)).toBe("built");
    expect(built(calls)).toBe(true);
    // It decides from the Containerfile alone and does not even look at the local images.
    expect(calls.some((cmd) => cmd[1] === "image" && cmd[2] === "inspect")).toBe(false);
    expect(readFileSync(logPath, "utf8")).toContain("is not the one release 0.0.0-rc.15 built");
  });

  test("an image of another platform is not used", async () => {
    const amd64 = { ...arm64("1"), Architecture: "amd64" };
    expect(await localImageProblem(PIN, fakeHost({ [PIN.image]: amd64, [POSTGRES_IMAGE]: amd64 }).commands)).toBe(
      `${PIN.image} is linux/amd64, expected linux/arm64`,
    );
  });
});

describe("updatePin", () => {
  const COMMIT = "7".repeat(40);
  const BLOB = "8".repeat(40);
  const COSIGN = "/pinned/cosign";
  const signature = (digest: string) => ({ critical: { type: "https://sigstore.dev/cosign/sign/v1", image: { "docker-manifest-digest": digest } } });
  const attestation = (digest: string) => ({ critical: { type: "https://in-toto.io/Statement/v1", image: { "docker-manifest-digest": digest } } });

  function fakeTools(claims: unknown, { cosignFails = false, missingTag = false } = {}) {
    const calls: string[][] = [];
    const commands: Pick<ImportCommands, "run"> = {
      run: async (runLog, cmd) => {
        runLog.line(`$ ${cmd.join(" ")}`);
        calls.push(cmd);
        if (cmd[0] === "git") {
          if (missingTag) throw new CommandError(cmd, failed("fatal: Needed a single revision"));
          return ok(`${cmd.at(-1)?.endsWith("^{commit}") ? COMMIT : BLOB}\n`);
        }
        if (cosignFails) throw new CommandError(cmd, failed("Error: no matching signatures"));
        return ok(JSON.stringify(claims));
      },
    };
    return { calls, commands };
  }
  const root = () => {
    mkdirSync(join(dir, "scripts", "nightly"), { recursive: true });
    writeFileSync(join(dir, PIN_PATH), "unchanged");
    return dir;
  };
  const update = (commands: Pick<ImportCommands, "run">, version = "0.0.0-rc.15") =>
    updatePin(version, { repoRoot: root(), cosign: COSIGN, log, commands });

  test("writes the digest the verified signature vouches for and the Containerfile blob of the tag", async () => {
    const { calls, commands } = fakeTools([signature(DIGEST), attestation(OTHER_DIGEST)]);
    const pin = await update(commands);
    expect(pin).toEqual({ version: "0.0.0-rc.15", image: `${POSTGRES_REPOSITORY}@${DIGEST}`, containerfile: BLOB });
    expect(readFileSync(join(dir, PIN_PATH), "utf8")).toBe(renderPostgresPin(pin));
    expect(calls[0]).toEqual(["git", "-C", dir, "rev-parse", "--verify", "v0.0.0-rc.15^{commit}"]);
    expect(calls[1]).toEqual(["git", "-C", dir, "rev-parse", "--verify", `v0.0.0-rc.15:${CONTAINERFILE_PATH}`]);
    // The identity release.yml's verify job uses: the workflow at the tag, at the tag's commit.
    expect(calls[2]).toEqual([
      COSIGN, "verify",
      "--certificate-identity", "https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v0.0.0-rc.15",
      "--certificate-oidc-issuer", "https://token.actions.githubusercontent.com",
      "--certificate-github-workflow-repository", "Laysi/trustiot-v2",
      "--certificate-github-workflow-ref", "refs/tags/v0.0.0-rc.15",
      "--certificate-github-workflow-sha", COMMIT,
      "--certificate-github-workflow-trigger", "push",
      "--output", "json",
      `${POSTGRES_REPOSITORY}:0.0.0-rc.15`,
    ]);
  });

  test("leaves the pin alone when the signature does not verify or vouches for no single digest", async () => {
    await expect(update(fakeTools([], { cosignFails: true }).commands)).rejects.toThrow("no matching signatures");
    await expect(update(fakeTools([attestation(DIGEST)]).commands)).rejects.toThrow("vouched for none");
    await expect(update(fakeTools([signature(DIGEST), signature(OTHER_DIGEST)]).commands)).rejects.toThrow(`vouched for ${DIGEST}, ${OTHER_DIGEST}`);
    await expect(update(fakeTools({ not: "a list" }).commands)).rejects.toThrow("vouched for none");
    await expect(update(fakeTools([signature(DIGEST)], { missingTag: true }).commands)).rejects.toThrow("git fetch --tags origin");
    expect(readFileSync(join(dir, PIN_PATH), "utf8")).toBe("unchanged");
  });

  test("refuses a version that is not a release", async () => {
    await expect(update(fakeTools([signature(DIGEST)]).commands, "main")).rejects.toThrow("not a release tag");
    await expect(update(fakeTools([signature(DIGEST)]).commands, "0.0.0-rc.15 --help")).rejects.toThrow("not a release tag");
  });
});
