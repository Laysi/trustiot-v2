import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";

import { assertBuildContext, assertCheckedBuild, containerBuildArgs } from "../lib/build-context.ts";
import { containerfile, SERVICES } from "../lib/services.ts";
import {
  annotation,
  checkBuildArgsInText,
  checkBuildFile,
  checkBuildInvocations,
  contextPath,
  shellWords,
  checkRepository,
  checkRootIgnore,
  checkSecretManifests,
  checkTemplateText,
  checkWorkflowSecrets,
  githubExpressions,
  globRegExp,
  ignoresEverything,
  isIgnored,
  isBuildFile,
  listFiles,
  parseBuildFile,
  reincludedSecrets,
  SECRET_LIKE_NAME,
  secretFilesInContext,
  type Violation,
} from "./check-secret-handling.ts";

const SCRIPT = join(import.meta.dir, "check-secret-handling.ts");
const REPOSITORY_ROOT = join(import.meta.dir, "..", "..");
const DIGEST = `sha256:${"0123456789abcdef".repeat(4)}`;

/** A temporary repository root; `files` maps relative paths to contents. */
function tempRoot(files: Record<string, string>): string {
  const root = mkdtempSync(join(tmpdir(), "check-secret-handling-"));
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), content);
  }
  return root;
}

function messages(violations: Violation[]): string[] {
  return violations.map((violation) => `${violation.file}:${violation.line} ${violation.message}`);
}

/** Checks a build file that sits alone in a root with no ignore file next to it. */
function checkAlone(source: string, name = "Containerfile"): Violation[] {
  const root = tempRoot({ [name]: source });
  try {
    return checkBuildFile(root, name, source);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("build file names", () => {
  test("finds Dockerfiles and Containerfiles under every naming convention", () => {
    for (const name of [
      "Dockerfile",
      "Containerfile",
      "deploy/images/x/Containerfile",
      "deploy/images/rust-binary.Containerfile",
      "build.dockerfile",
      "Dockerfile.dev",
      "containerfile",
    ]) {
      expect(isBuildFile(name), name).toBe(true);
    }
    for (const name of [".dockerignore", "Dockerfile.dockerignore", "Containerfile.containerignore", "docs/dockerfiles.md"]) {
      expect(isBuildFile(name), name).toBe(false);
    }
  });
});

describe("parseBuildFile", () => {
  test("reads heredocs only where Docker does", () => {
    const source = [
      "FROM scratch",
      "ENV NOTE=<<EOF",
      "ARG API_TOKEN",
      "LABEL x=<<EOF",
      "COPY a<<EOF /x",
      "ARG DB_PASSWORD",
      "ONBUILD RUN <<EOF",
      "ARG HIDDEN_IN_HEREDOC_TOKEN",
      "EOF",
      "ARG NPM_TOKEN",
    ].join("\n");
    expect(parseBuildFile(source).map((instruction) => instruction.line)).toEqual([1, 2, 3, 4, 5, 6, 7, 10]);
    expect(messages(checkAlone(source)).filter((message) => message.includes("ARG"))).toEqual([
      expect.stringMatching(/^Containerfile:3 ARG API_TOKEN looks like a secret/),
      expect.stringMatching(/^Containerfile:6 ARG DB_PASSWORD looks like a secret/),
      expect.stringMatching(/^Containerfile:10 ARG NPM_TOKEN looks like a secret/),
    ]);
  });

  test("joins continuations, drops comments and skips heredoc bodies", () => {
    const source = [
      "# syntax=docker/dockerfile:1",
      `FROM gcr.io/distroless/static@${DIGEST}`,
      "COPY --chown=65532:65532 \\",
      "  # a comment inside a continuation",
      "  bin/identity /usr/local/bin/",
      "RUN <<EOF",
      "COPY .env /looks/like/an/instruction",
      "EOF",
      "COPY <<-CONFIG /etc/app.toml",
      "\tARG API_TOKEN",
      "\tCONFIG",
      'ENTRYPOINT ["/usr/local/bin/identity"]',
    ].join("\n");
    expect(parseBuildFile(source)).toEqual([
      { line: 2, keyword: "FROM", args: `gcr.io/distroless/static@${DIGEST}` },
      { line: 3, keyword: "COPY", args: "--chown=65532:65532    bin/identity /usr/local/bin/" },
      { line: 6, keyword: "RUN", args: "<<EOF" },
      { line: 9, keyword: "COPY", args: "<<-CONFIG /etc/app.toml" },
      { line: 12, keyword: "ENTRYPOINT", args: '["/usr/local/bin/identity"]' },
    ]);
  });

  test("honours the escape parser directive", () => {
    const source = "# escape=`\nFROM scratch\nCOPY `\n  .env /x\n";
    expect(parseBuildFile(source)).toEqual([
      { line: 2, keyword: "FROM", args: "scratch" },
      { line: 3, keyword: "COPY", args: ".env /x" },
    ]);
  });
});

describe("checkBuildFile", () => {
  test("accepts a binary copied onto a pinned base, as release.yml and the dev environment build", () => {
    const source = [
      `FROM gcr.io/distroless/static-debian13:nonroot@${DIGEST}`,
      "COPY --chmod=0555 @BIN@ /usr/local/bin/@BIN@",
      "USER 65532:65532",
      'ENTRYPOINT ["/usr/local/bin/@BIN@"]',
    ].join("\n");
    expect(checkAlone(source)).toEqual([]);
  });

  test("the dev environment's generated Containerfiles pass", () => {
    for (const service of SERVICES) {
      expect(checkAlone(containerfile(`gcr.io/distroless/cc@${DIGEST}`, service))).toEqual([]);
    }
  });

  test("accepts a secret mount and non-secret build arguments", () => {
    const source = [
      "FROM --platform=$BUILDPLATFORM rust AS build",
      "ARG TARGETARCH VERSION=1.0",
      "RUN --mount=type=secret,id=registry_token,env=REGISTRY_TOKEN cargo fetch",
      "ENV RUST_LOG=info",
    ].join("\n");
    expect(checkAlone(source)).toEqual([]);
  });

  test("rejects COPY and ADD of key and credential files", () => {
    const source = [
      "FROM scratch",
      "COPY .env /app/.env",
      "COPY certs/server.key /etc/tls/",
      "ADD signing.pem /keys/",
      "COPY [\"deploy/kubeconfig\", \"/root/.kube/config\"]",
      "COPY --from=build /root/.ssh/id_ed25519 /root/",
      "COPY .npmrc .git-credentials /home/",
      "ONBUILD COPY credentials.json /app/",
      "COPY .git /src/.git",
      // Templates for dotenv files and public certificates are fine.
      "COPY .env.example /app/",
      "COPY ca.crt id_ed25519.pub /etc/",
      // The JSON form after flags.
      'COPY --chown=1000:1000 --chmod=0400 [".env", "/app/.env"]',
      'ADD --link ["tls/server.key", "/etc/tls/"]',
    ].join("\n");
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:2 COPY \.env copies a dotenv file into the image/),
      expect.stringMatching(/^Containerfile:3 COPY certs\/server\.key copies a private key or keystore/),
      expect.stringMatching(/^Containerfile:4 ADD signing\.pem copies a private key or keystore/),
      expect.stringMatching(/^Containerfile:5 COPY deploy\/kubeconfig copies a kubeconfig/),
      expect.stringMatching(/^Containerfile:6 COPY \/root\/\.ssh\/id_ed25519 copies a credential directory/),
      expect.stringMatching(/^Containerfile:7 COPY \.npmrc copies a credential file/),
      expect.stringMatching(/^Containerfile:7 COPY \.git-credentials copies a credential file/),
      expect.stringMatching(/^Containerfile:8 COPY credentials\.json copies a credentials file/),
      expect.stringMatching(/^Containerfile:9 COPY \.git copies the git metadata/),
      expect.stringMatching(/^Containerfile:12 COPY \.env copies a dotenv file/),
      expect.stringMatching(/^Containerfile:13 ADD tls\/server\.key copies a private key or keystore/),
    ]);
  });

  test("quoted shell-form sources are read as one word", () => {
    const source = [
      "FROM scratch",
      'COPY "cert folder/server.key" /etc/tls/',
      "COPY 'my keys/id_ed25519' /root/",
      "COPY cert\\ folder/server.key /etc/tls/",
      'COPY "a b".env /app/',
      // A quoted public file is fine.
      'COPY "public certs/ca.crt" /etc/',
    ].join("\n");
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:2 COPY cert folder\/server\.key copies a private key or keystore/),
      expect.stringMatching(/^Containerfile:3 COPY my keys\/id_ed25519 copies an SSH private key/),
      expect.stringMatching(/^Containerfile:4 COPY cert folder\/server\.key copies a private key or keystore/),
      expect.stringMatching(/^Containerfile:5 COPY a b\.env copies a dotenv file/),
    ]);
    expect(shellWords('a "b c" \'d e\' f\\ g "h\\"i" C:\\app ""')).toEqual(["a", "b c", "d e", "f g", 'h"i', "C:\\app", ""]);
  });

  test("sources with .. are read as the path the builder copies", () => {
    const source = ["FROM scratch", "COPY foo/.. /app/", "COPY foo/../* /app/", "COPY ../../.env /app/", "COPY app/../certs/server.key /etc/"].join("\n");
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:2 COPY foo\/\.\. copies the whole build context, but the repository root has no ignore file/),
      expect.stringMatching(/^Containerfile:3 COPY foo\/\.\.\/\* copies the whole build context/),
      expect.stringMatching(/^Containerfile:4 COPY \.\.\/\.\.\/\.env copies a dotenv file/),
      expect.stringMatching(/^Containerfile:5 COPY app\/\.\.\/certs\/server\.key copies a private key or keystore/),
    ]);
    expect(["foo/..", "./foo/../", "../../x/y", "/a/./b", "https://example.com/a/../b"].map(contextPath)).toEqual([".", ".", "x/y", "a/b", "https://example.com/a/../b"]);
  });

  test("credential files are recognized by a word of their name", () => {
    const names = ["service-account.json", "client_secret.json", "gcp-credentials.json", "prod.token", "api_key.txt", "signing-private-key.bin", "sa-key.json"];
    const source = ["FROM scratch", ...names.map((name) => `COPY config/${name} /app/`), "COPY config/app.toml tokenizer.json /app/"].join("\n");
    expect(messages(checkAlone(source))).toEqual(
      names.map((name, index) => expect.stringMatching(new RegExp(`^Containerfile:${index + 2} COPY config/${name.replaceAll(".", "\\.")} copies a credentials file`))),
    );
    const context = tempRoot({
      ".dockerignore": "*\n!config\n",
      Containerfile: "FROM scratch\nCOPY config/*.json /app/\nCOPY config /app/config\n",
      "config/service-account.json": "{}",
      "config/client_secret.json": "{}",
      "config/app.json": "{}",
    });
    try {
      expect(secretFilesInContext(context)).toEqual(["config/client_secret.json", "config/service-account.json"]);
      expect(messages(checkBuildFile(context, "Containerfile", readFileSync(join(context, "Containerfile"), "utf8")))).toEqual([
        expect.stringMatching(/^Containerfile:2 COPY config\/\*\.json copies config\/client_secret\.json, config\/service-account\.json from the build context/),
        expect.stringMatching(/^Containerfile:3 COPY config copies config\/client_secret\.json, config\/service-account\.json from the build context/),
      ]);
    } finally {
      rmSync(context, { recursive: true, force: true });
    }
  });

  test("dotenv files are recognized by their .env suffix too", () => {
    const source = [
      "FROM scratch",
      "COPY app.env /app/",
      "COPY config/production.env.local /app/",
      "COPY .envrc /app/",
      "COPY .env.production /app/",
      // Templates and names that only contain "env" are fine.
      "COPY app.env.example deploy/env/settings.toml environment.json /app/",
    ].join("\n");
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:2 COPY app\.env copies a dotenv file/),
      expect.stringMatching(/^Containerfile:3 COPY config\/production\.env\.local copies a dotenv file/),
      expect.stringMatching(/^Containerfile:4 COPY \.envrc copies a dotenv file/),
      expect.stringMatching(/^Containerfile:5 COPY \.env\.production copies a dotenv file/),
    ]);
    const context = tempRoot({ Containerfile: "FROM scratch\n", app: "", "app.env": "X=1\n", "config/production.env": "X=1\n", "app.env.example": "" });
    try {
      expect(secretFilesInContext(context)).toEqual(["app.env", "config/production.env"]);
      expect(() => containerBuildArgs(context, [])).toThrow(/would send app\.env, config\/production\.env to the builder/);
    } finally {
      rmSync(context, { recursive: true, force: true });
    }
  });

  test("rejects COPY and ADD sources that use a variable", () => {
    const source = ["FROM scratch", "ARG FILE=.env", "COPY $FILE /app/", 'ADD ["${DIR}/x", "/opt/"]', "COPY bin/app /usr/local/bin/$NAME"].join("\n");
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:3 COPY \$FILE uses a variable, so the copied path cannot be checked/),
      expect.stringMatching(/^Containerfile:4 ADD \$\{DIR\}\/x uses a variable/),
    ]);
  });

  test("rejects a secret passed through ARG or baked in with ENV", () => {
    const source = [
      "FROM scratch",
      "ARG GITHUB_TOKEN",
      "ARG VERSION=1 NPM_AUTH_TOKEN",
      "ARG signing_key=",
      "ENV DATABASE_PASSWORD=changeme",
      "ENV SENTRY_DSN https://example.test/1",
      "ENV API_KEY",
    ].join("\n");
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:2 ARG GITHUB_TOKEN looks like a secret; .*RUN --mount=type=secret/),
      expect.stringMatching(/^Containerfile:3 ARG NPM_AUTH_TOKEN looks like a secret/),
      expect.stringMatching(/^Containerfile:4 ARG signing_key looks like a secret/),
      expect.stringMatching(/^Containerfile:5 ENV DATABASE_PASSWORD looks like a secret/),
      expect.stringMatching(/^Containerfile:6 ENV SENTRY_DSN looks like a secret/),
      expect.stringMatching(/^Containerfile:7 ENV API_KEY looks like a secret/),
    ]);
  });

  test("rejects ADD of a URL with credentials", () => {
    const violations = checkAlone("FROM scratch\nADD https://user:pass@downloads.example.test/tool.tgz /opt/\n");
    expect(messages(violations)).toEqual([
      expect.stringMatching(/^Containerfile:2 ADD fetches a URL with credentials in it/),
    ]);
  });

  test("copying the whole context needs an ignore file that starts by ignoring everything", () => {
    const source = "FROM scratch\nCOPY . /src\n";
    expect(messages(checkAlone(source))).toEqual([
      expect.stringMatching(/^Containerfile:2 COPY \. copies the whole build context, but the repository root has no ignore file/),
    ]);

    const root = tempRoot({
      // The repository root lets nothing in, as in this repository.
      ".dockerignore": "*\n",
      "a/Containerfile": source,
      "a/.containerignore": "# allow-list\n*\n!src/\n",
      "b/Dockerfile": "FROM scratch\nCOPY ./ /src\n",
      "b/.dockerignore": ".env\n*.key\n",
      "c/app.Dockerfile": "FROM scratch\nADD * /src/\n",
      "c/app.Dockerfile.dockerignore": "**\n!Cargo.toml\n",
      "c/.dockerignore": "target\n",
      "d/Containerfile": "FROM scratch\nCOPY --from=build . /app\n",
      "e/Containerfile": source,
      "e/.containerignore": "*\n!src/\n!.env\n!/certs/server.key\n!**\n",
    });
    try {
      // A deny-all ignore file must not bring key files, or everything, back.
      expect(messages(checkBuildFile(root, "e/Containerfile", source))).toEqual([
        expect.stringMatching(/^e\/Containerfile:2 COPY \. copies the whole build context, but e\/\.containerignore brings back !\.env, !\/certs\/server\.key, !\*\*;/),
      ]);
      expect(checkBuildFile(root, "a/Containerfile", source)).toEqual([]);
      expect(messages(checkBuildFile(root, "b/Dockerfile", "FROM scratch\nCOPY ./ /src\n"))).toEqual([
        expect.stringMatching(/^b\/Dockerfile:2 COPY \.\/ copies the whole build context, but b\/\.dockerignore does not start with "\*"/),
      ]);
      // Every ignore file that a builder may pick must deny everything first.
      expect(messages(checkBuildFile(root, "c/app.Dockerfile", "FROM scratch\nADD * /src/\n"))).toEqual([
        expect.stringMatching(/c\/\.dockerignore does not start with "\*"/),
      ]);
      // Copying from another stage does not read the build context.
      expect(checkBuildFile(root, "d/Containerfile", "FROM scratch\nCOPY --from=build . /app\n")).toEqual([]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("key files that exist in the build context must stay out of every copy", () => {
    const root = tempRoot({
      ".dockerignore": "*\n",
      "f/Containerfile": "FROM scratch\nCOPY . /src\n",
      "f/.containerignore": "*\n!src/\n",
      "f/src/main.rs": "fn main() {}\n",
      "f/src/server.key": "not a real key\n",
      "g/Containerfile": "FROM scratch\nCOPY config /etc/app\nCOPY bin/* /usr/local/bin/\n",
      "g/config/app.toml": "",
      "g/config/tls.pem": "not a real key\n",
      "g/bin/app": "",
      "h/Containerfile": "FROM scratch\nCOPY config /etc/app\n",
      "h/.dockerignore": "*\n!config/\nconfig/*.pem\n",
      "h/config/app.toml": "",
      "h/config/tls.pem": "not a real key\n",
    });
    try {
      expect(messages(checkBuildFile(root, "f/Containerfile", "FROM scratch\nCOPY . /src\n"))).toEqual([
        expect.stringMatching(/^f\/Containerfile:2 COPY \. copies f\/src\/server\.key from the build context into the image/),
      ]);
      expect(messages(checkBuildFile(root, "g/Containerfile", "FROM scratch\nCOPY config /etc/app\nCOPY bin/* /usr/local/bin/\n"))).toEqual([
        expect.stringMatching(/^g\/Containerfile:2 COPY config copies g\/config\/tls\.pem from the build context/),
      ]);
      // An ignore rule after the exception leaves the key out again.
      expect(checkBuildFile(root, "h/Containerfile", "FROM scratch\nCOPY config /etc/app\n")).toEqual([]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("the context scan also sees directories the repository scan skips", () => {
    const source = "FROM scratch\nCOPY . /app\n";
    const root = tempRoot({
      ".dockerignore": "*\n",
      ".containerignore": "*\n",
      "web/Containerfile": source,
      "web/.containerignore": "*\n!dist/\n!node_modules/\n",
      "web/node_modules/pkg/server.key": "not a real key\n",
      "web/dist/index.html": "",
    });
    try {
      expect(messages(checkBuildFile(root, "web/Containerfile", source))).toEqual([
        expect.stringMatching(/^web\/Containerfile:2 COPY \. copies web\/node_modules\/pkg\/server\.key from the build context/),
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("every directory up to the repository root counts as a possible build context", () => {
    const source = "FROM scratch\nCOPY . /src\n";
    const root = tempRoot({
      ".dockerignore": "*\n",
      "images/x/Containerfile": source,
      "images/x/.containerignore": "*\n!bin/app\n",
      // A build with images/ as the context (no ignore file there) would take this key.
      "images/signing.key": "not a real key\n",
    });
    try {
      expect(messages(checkBuildFile(root, "images/x/Containerfile", source))).toEqual([
        expect.stringMatching(/^images\/x\/Containerfile:2 COPY \. copies images\/signing\.key from the build context/),
        expect.stringMatching(/^images\/x\/Containerfile:2 COPY \. copies the whole build context, but images\/ has no ignore file/),
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("wildcard exceptions that could match a key file are rejected", () => {
    expect(reincludedSecrets("*\n!src/\n!Cargo.*\n!bin/app\n")).toEqual([]);
    expect(reincludedSecrets("*\n!certs/*\n!**/*.pem\n!config/?env\n!*\n")).toEqual([
      "!certs/*",
      "!**/*.pem",
      // `?env` also matches `.env`.
      "!config/?env",
      "!*",
    ]);
  });

  test("ignore patterns follow the .dockerignore rules", () => {
    expect(isIgnored(["*", "!src/"], "src/a/b.rs")).toBe(false);
    expect(isIgnored(["*", "!src/", "src/**/*.key"], "src/a/b.key")).toBe(true);
    expect(isIgnored(["*", "!src/", "src/**/*.key"], "src/b.key")).toBe(true);
    expect(isIgnored(["target"], "target/debug/app")).toBe(true);
    expect(isIgnored(["*.md"], "docs/a.md")).toBe(false);
    expect(isIgnored(["**/*.md"], "docs/a.md")).toBe(true);
    expect(globRegExp("./bin/[!a]pp").test("bin/xpp")).toBe(true);
    expect(globRegExp("a+b(c).txt").test("a+b(c).txt")).toBe(true);
  });

  test("ignoresEverything looks at the first pattern only", () => {
    expect(ignoresEverything("# comment\n\n*\n!bin/\n")).toBe(true);
    expect(ignoresEverything("**")).toBe(true);
    expect(ignoresEverything(".env\n*\n")).toBe(false);
    // `.` names the context itself and matches nothing, so it neither ignores nor re-includes.
    expect(ignoresEverything(".\n")).toBe(false);
    expect(isIgnored([".", "./"], "config/server.key")).toBe(false);
    expect(isIgnored(["*", "!."], "config/server.key")).toBe(true);
    const dotOnly = tempRoot({ ".dockerignore": ".\n", "config/server.key": "", "config/app.toml": "" });
    try {
      expect(secretFilesInContext(dotOnly)).toEqual(["config/server.key"]);
    } finally {
      rmSync(dotOnly, { recursive: true, force: true });
    }
    expect(ignoresEverything("")).toBe(false);
  });
});

describe("checkRootIgnore", () => {
  test("requires a root .dockerignore that ignores everything, and the same of .containerignore", () => {
    const missing = tempRoot({});
    const permissive = tempRoot({ ".dockerignore": "# deny all\n*\n", ".containerignore": ".env\n" });
    const exception = tempRoot({ ".dockerignore": "*\n!.env\n" });
    try {
      expect(messages(checkRootIgnore(missing))).toEqual([expect.stringMatching(/^\.dockerignore:0 missing/)]);
      expect(messages(checkRootIgnore(permissive))).toEqual([expect.stringMatching(/^\.containerignore:1 must hold only "\*"/)]);
      // Any exception brings files from the repository root back into a build.
      expect(messages(checkRootIgnore(exception))).toEqual([expect.stringMatching(/^\.dockerignore:1 must hold only "\*", with no "!" exceptions/)]);
    } finally {
      for (const root of [missing, permissive, exception]) rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("build arguments in commands", () => {
  test("accepts literal non-secret build arguments, non-secret GitHub expressions and secret mounts", () => {
    const script = [
      'podman build --build-arg VERSION=1.2.3 --secret id=token,src="$TOKEN_FILE" .',
      'await run(["podman", "build", "--build-arg", "TARGET=aarch64", "--file", file, context]);',
      "docker buildx build \\\n  --build-arg=TARGET=aarch64 .",
      'podman build --build-arg VERSION="1.2.3" --build-arg GIT_SHA=${{ github.sha }} .',
      "podman build --build-arg 'TAG=${{ github.ref_name }}' .",
    ].join("\n");
    expect(checkBuildArgsInText("scripts/build.ts", script)).toEqual([]);
  });

  test("rejects secret-looking names, secrets.* values, unreadable names and --build-arg-file", () => {
    const script = [
      "podman build --build-arg GITHUB_TOKEN .",
      'run(["docker", "build",',
      '  "--build-arg",',
      '  "NPM_AUTH_TOKEN=" + process.env.NPM_AUTH_TOKEN]);',
      "docker build --build-arg VERSION=${{ secrets.VERSION }} .",
      'run(["podman", "build", "--build-arg", arg]);',
      "podman build --build-arg-file build.args .",
    ].join("\n");
    expect(messages(checkBuildArgsInText("scripts/build.ts", script))).toEqual([
      expect.stringMatching(/^scripts\/build\.ts:1 build argument GITHUB_TOKEN looks like a secret/),
      expect.stringMatching(/^scripts\/build\.ts:3 build argument NPM_AUTH_TOKEN looks like a secret/),
      expect.stringMatching(/^scripts\/build\.ts:5 build argument VERSION reads a secret/),
      expect.stringMatching(/^scripts\/build\.ts:6 the build argument name must be written as one literal/),
      expect.stringMatching(/^scripts\/build\.ts:7 --build-arg-file hides the build arguments/),
    ]);
  });

  test("rejects names and values that are assembled at run time", () => {
    const script = [
      'run(["podman", "build", "--build-arg", "VERSION" + "_PASSWORD=" + process.env.PASSWORD]);',
      'run(["podman", "build", "--build-arg", "VERSION=" + process.env.DB_PASSWORD]);',
      'run(["podman", "build", "--build-arg", `GIT_SHA=${sha}`]);',
      'run(["podman", "build", "--build-arg", `VERSION${suffix}=1`]);',
      'podman build --build-arg "VERSION"_PASSWORD=x .',
      "podman build --build-arg VERSION=$VERSION .",
      "podman build --build-arg VERSION=${{ env.VERSION }} .",
      // github.token and step outputs can carry secrets.
      "podman build --build-arg VERSION=${{ github.token }} .",
      "podman build --build-arg VERSION=${{ steps.plan.outputs.tag }} .",
      // A bare name takes its value from the builder's environment.
      "podman build --build-arg VERSION .",
    ].join("\n");
    expect(messages(checkBuildArgsInText("scripts/build.ts", script))).toEqual([
      expect.stringMatching(/^scripts\/build\.ts:1 the build argument name must be written as one literal/),
      expect.stringMatching(/^scripts\/build\.ts:2 the value of build argument VERSION is computed/),
      expect.stringMatching(/^scripts\/build\.ts:3 the value of build argument GIT_SHA is computed/),
      expect.stringMatching(/^scripts\/build\.ts:4 the build argument name must be written as one literal/),
      expect.stringMatching(/^scripts\/build\.ts:5 build argument VERSION_PASSWORD looks like a secret/),
      expect.stringMatching(/^scripts\/build\.ts:6 the value of build argument VERSION is computed/),
      expect.stringMatching(/^scripts\/build\.ts:7 the value of build argument VERSION is computed/),
      expect.stringMatching(/^scripts\/build\.ts:8 the value of build argument VERSION is computed/),
      expect.stringMatching(/^scripts\/build\.ts:9 the value of build argument VERSION is computed/),
      expect.stringMatching(/^scripts\/build\.ts:10 build argument VERSION has no value, so the builder takes it from the environment/),
    ]);
  });

  test("checks the build-args input of workflow steps", () => {
    const workflow = `on: push
permissions: {}
jobs:
  image:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: docker/build-push-action@${"a".repeat(40)} # v6
        with:
          build-args: |
            VERSION=1.0
            REGISTRY_PASSWORD=\${{ secrets.REGISTRY_PASSWORD }}
            MIRROR=\${{ secrets.MIRROR_URL }}
`;
    expect(messages(checkWorkflowSecrets(".github/workflows/image.yml", workflow))).toEqual([
      expect.stringMatching(/^\.github\/workflows\/image\.yml:9 jobs\.image\.steps\[0\] builds an image with docker\/build-push-action, which sends a build context the checks cannot see/),
      expect.stringMatching(/^\.github\/workflows\/image\.yml:13 jobs\.image\.steps\[0\]\.with\.build-args: build argument REGISTRY_PASSWORD looks like a secret/),
      expect.stringMatching(/^\.github\/workflows\/image\.yml:14 jobs\.image\.steps\[0\]\.with\.build-args: build argument MIRROR reads a secret/),
    ]);
  });
});

describe("builds check their actual context", () => {
  test("container builds take their arguments from containerBuildArgs", () => {
    const direct = [
      'await run([podman, "build", "--file", containerfile, context]);',
      // Checking some directory first does not cover a build of another one.
      "assertBuildContext(context);",
      'await podmanOk([\n  "build",\n  other,\n]);',
      'await run(["docker", "buildx", "build", "."]);',
      'await run([podman, "image", "build", "."]);',
      'await podmanOk(["image", "build", "."]);',
      'await run(["docker", "compose", "build"]);',
      'await run(["docker-compose", "build"]);',
      'await run([docker, "buildx", "bake"]);',
      'Bun.spawn(["buildah", "bud", context]);',
      'Bun.spawn(["/usr/bin/Buildah", "build", context]);',
    ].join("\n");
    expect(messages(checkBuildInvocations("scripts/release/build.ts", direct))).toEqual([
      expect.stringMatching(/^scripts\/release\/build\.ts:1 writes a container build command; take the arguments from containerBuildArgs/),
      expect.stringMatching(/^scripts\/release\/build\.ts:3 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:7 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:8 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:9 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:10 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:11 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:12 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:13 writes a container build command/),
      expect.stringMatching(/^scripts\/release\/build\.ts:14 writes a container build command/),
    ]);
    const wrapped = 'await podmanOk(containerBuildArgs(buildDir, ["--tag", image]));\nawait run([podman, ...containerBuildArgs(context, [])]);\n';
    expect(checkBuildInvocations("scripts/lib/commands.ts", wrapped)).toEqual([]);
    // "build" for another tool is not a container build, and the wrapper itself may build.
    expect(checkBuildInvocations("scripts/lib/commands.ts", 'podmanOk(["rm"]);\nBun.spawn(["cargo", "build", "--release"]);\n')).toEqual([]);
    expect(checkBuildInvocations("scripts/lib/build-context.ts", 'return ["build", ...options, context]; // podman\n')).toEqual([]);
  });

  test("container commands with a subcommand computed at run time are rejected", () => {
    const dynamic = [
      "const operation = \"build\";",
      "await run([podman, operation, context]);",
      "await run([\"docker\", ...args]);",
      "Bun.spawn([\"sudo\", \"/usr/bin/podman\", sub, \".\"]);",
      "await podmanOk([verb, context]);",
      "await $`podman ${operation} ${context}`;",
      "await run([podman, \"image\", operation]);",
      "await podmanOk([`${verb}`, context]);",
      "execSync(`docker ${verb} .`);",
      "Bun.spawn([\"sh\", \"-c\", `podman ${verb} .`]);",
      "Bun.spawn([\"docker\", \"--context\", \"prod\", operation, \".\"]);",
      "await podmanOk([\"--log-level\", \"info\", verb]);",
      "await run([\"docker\", \"buildx\", \"--builder\", \"x\", operation]);",
      "await $`docker --context prod ${operation} .`;",
      "Bun.spawn([\"buildah\", op, context]);",
    ].join("\n");
    expect(messages(checkBuildInvocations("scripts/release/build.ts", dynamic)).map((message) => message.split(" runs ")[0])).toEqual([
      "scripts/release/build.ts:2",
      "scripts/release/build.ts:3",
      "scripts/release/build.ts:4",
      "scripts/release/build.ts:5",
      "scripts/release/build.ts:6",
      "scripts/release/build.ts:7",
      "scripts/release/build.ts:8",
      "scripts/release/build.ts:9",
      "scripts/release/build.ts:10",
      "scripts/release/build.ts:11",
      "scripts/release/build.ts:12",
      "scripts/release/build.ts:13",
      "scripts/release/build.ts:14",
      "scripts/release/build.ts:15",
    ]);
    expect(messages(checkBuildInvocations("deploy/build.sh", "docker $CMD .\n"))).toEqual([
      expect.stringMatching(/^deploy\/build\.sh:1 runs a container tool whose subcommand is not fixed text/),
    ]);
    // Options before the subcommand, with or without values and across line continuations.
    for (const command of [
      "docker --context prod $CMD .",
      "podman --log-level=info ${OP} .",
      "docker --debug \\\n  $CMD .",
      'podman "$CMD" .',
      'docker "${OP}" .',
      "podman '$CMD' .",
      "docker image \"$OP\" .",
      "podman `echo build` .",
    ]) {
      expect(checkBuildInvocations("deploy/build.sh", `${command}\n`), command).toHaveLength(1);
    }
    for (const command of [
      "docker --context prod build .",
      "podman --remote image build .",
      "docker \\\n  buildx --builder x build .",
      'podman "build" .',
      "docker 'buildx' \"build\" .",
    ]) {
      expect(messages(checkBuildInvocations("deploy/build.sh", `${command}\n`)), command).toEqual([
        expect.stringMatching(/^deploy\/build\.sh:1 builds a container image directly/),
      ]);
    }
    expect(checkBuildInvocations("deploy/build.sh", "podman --version\ndocker --context prod ps\n")).toEqual([]);
    // Fixed subcommands, containerBuildArgs, arrays computed by a helper (the runner checks
    // them at run time) and prose in comments pass.
    const fixed = [
      "await run([podman, \"push\", localTag, `oci-archive:${archive}`]);",
      "await run([podman, ...containerBuildArgs(context, [\"--tag\", tag])]);",
      "await podmanOk([\"network\", \"rm\", ...networks]);",
      "await podmanOk(runArgs(ctx.instance, options));",
      "await podmanOk(containerBuildArgs(dir, []));",
      "// run([podman, operation]) would be rejected",
      "/* podman ${x} */",
      "throw new Error(`podman ${verb} failed`);",
    ].join("\n");
    expect(checkBuildInvocations("scripts/lib/commands.ts", fixed)).toEqual([]);
  });

  test("the podman runner passes its callers' arguments along only behind the run-time guard", () => {
    const runner = 'export async function podman(args: readonly string[]) {\n  return Bun.spawn(["podman", ...args]);\n}\n';
    expect(messages(checkBuildInvocations("scripts/lib/podman.ts", runner))).toEqual([
      expect.stringMatching(/^scripts\/lib\/podman\.ts:1 runs podman with its callers' arguments but does not call assertCheckedBuild/),
    ]);
    const guarded = runner.replace("{\n", "{\n  assertCheckedBuild(args);\n");
    expect(checkBuildInvocations("scripts/lib/podman.ts", guarded)).toEqual([]);
    // Anywhere else, the same spread is a computed subcommand.
    expect(checkBuildInvocations("scripts/lib/other.ts", guarded)).toHaveLength(1);
    expect(checkBuildInvocations("scripts/lib/podman.ts", readFileSync(join(import.meta.dir, "../lib/podman.ts"), "utf8"))).toEqual([]);
  });

  test("containerBuildArgs checks the build file podman reads, wherever it is", () => {
    const clean = tempRoot({ Containerfile: "FROM scratch\nCOPY app /app\n", app: "" });
    const elsewhere = tempRoot({
      "bad.Containerfile": "FROM scratch\nCOPY app /app\nARG NPM_TOKEN\n",
      "copies-key.Containerfile": "FROM scratch\nCOPY id_ed25519 /root/.ssh/\n",
      "copies-all.Containerfile": "FROM scratch\nCOPY . /app\n",
      "good.Containerfile": "FROM scratch\nCOPY app /app\n",
      "somedir/x": "",
    });
    const empty = tempRoot({ app: "" });
    try {
      expect(() => containerBuildArgs(clean, ["--file", join(elsewhere, "bad.Containerfile")])).toThrow(/breaks the secret rules: line 3: ARG NPM_TOKEN looks like a secret/);
      expect(() => containerBuildArgs(clean, ["-f", join(elsewhere, "copies-key.Containerfile")])).toThrow(/breaks the secret rules: line 2: COPY id_ed25519/);
      // The context has no deny-all ignore file, so copying all of it is refused.
      expect(() => containerBuildArgs(clean, [`--file=${join(elsewhere, "copies-all.Containerfile")}`])).toThrow(/breaks the secret rules: line 2: COPY \. copies the whole build context/);
      expect(() => containerBuildArgs(clean, ["--file", "-"])).toThrow(/--file - is not a regular file/);
      expect(() => containerBuildArgs(clean, ["--file", "https://example.com/Containerfile"])).toThrow(/is not a regular file/);
      expect(() => containerBuildArgs(clean, ["--file", join(elsewhere, "somedir")])).toThrow(/is not a regular file/);
      expect(() => containerBuildArgs(clean, ["--file", join(elsewhere, "good.Containerfile"), "--file", join(elsewhere, "bad.Containerfile")])).toThrow(/takes one --file here/);
      expect(() => containerBuildArgs(empty, [])).toThrow(/has no Containerfile or Dockerfile/);
      expect(containerBuildArgs(clean, ["--file", join(elsewhere, "good.Containerfile")])).toHaveLength(4);
    } finally {
      for (const root of [clean, elsewhere, empty]) rmSync(root, { recursive: true, force: true });
    }
  });

  test("compose is not used, because up, run and create build from the compose file too", () => {
    for (const command of ["docker compose up --build", "podman compose run api", "docker-compose up -d", "podman-compose create", "docker --context prod compose up"]) {
      expect(messages(checkBuildInvocations("deploy/run.sh", `${command}\n`)), command).toEqual([
        expect.stringMatching(/^deploy\/run\.sh:1 runs compose, which builds images/),
      ]);
    }
    const workflow = "steps:\n  - run: docker compose up -d --build\n";
    expect(checkBuildInvocations(".github/workflows/x.yml", workflow)).toHaveLength(1);
    const script = ['await run(["docker", "compose", "up", "--build"]);', 'await run(["docker-compose", "up"]);', 'await podmanOk(["compose", "up"]);'].join("\n");
    expect(messages(checkBuildInvocations("scripts/x.ts", script))).toEqual([
      expect.stringMatching(/^scripts\/x\.ts:1 runs compose/),
      expect.stringMatching(/^scripts\/x\.ts:2 runs compose/),
      expect.stringMatching(/^scripts\/x\.ts:3 runs compose/),
    ]);
    // Prose that mentions compose outside a command is fine.
    expect(checkBuildInvocations("deploy/run.sh", "# no compose here\necho composed\n")).toEqual([]);
  });

  test("the runner refuses a build whose arguments did not come from containerBuildArgs", () => {
    const clean = tempRoot({ Containerfile: "FROM scratch\n", app: "" });
    try {
      const operation = ["bu", "ild"].join("");
      expect(() => assertCheckedBuild([operation, clean])).toThrow(/builds an image without checking its context/);
      expect(() => assertCheckedBuild(["image", "bud", clean])).toThrow(/without checking its context/);
      const checked = containerBuildArgs(clean, ["--tag", "localhost/trustiot-dev-default-x:dev"]);
      expect(() => assertCheckedBuild(checked)).not.toThrow();
      // A copy, or the checked arguments with another context added, is not the checked array.
      expect(() => assertCheckedBuild([...checked])).toThrow(/without checking its context/);
      expect(() => (checked as string[]).push("/")).toThrow();
      expect(() => assertCheckedBuild(["ps", "-a"])).not.toThrow();
      expect(() => assertCheckedBuild(["buildx", "--debug", "build", clean])).toThrow(/podman buildx option --debug is not used/);
      expect(() => assertCheckedBuild(["image", "--log-level", "info", "build", clean])).toThrow(/podman image option --log-level is not used/);
      // `build` as the command run inside a container is not a podman build.
      expect(() => assertCheckedBuild(["exec", "dev-api", "build"])).not.toThrow();
      expect(() => assertCheckedBuild(["run", "--rm", "busybox", "bud"])).not.toThrow();
      expect(() => assertCheckedBuild(["image", "ls", "build"])).not.toThrow();
      expect(() => assertCheckedBuild(["compose", "build"])).toThrow(/podman compose can build images/);
      expect(() => assertCheckedBuild(["buildx", "bake"])).toThrow(/without checking its context/);
      expect(() => assertCheckedBuild(["compose", "-f", "x.yaml", "build"])).toThrow(/podman compose can build images/);
      // `up --build`, or `up` with a build section and no image yet, builds too.
      expect(() => assertCheckedBuild(["compose", "up", "--build"])).toThrow(/podman compose can build images/);
      expect(() => assertCheckedBuild(["compose", "run", "api"])).toThrow(/podman compose can build images/);
      // Global options could hide where the subcommand starts.
      expect(() => assertCheckedBuild(["--log-level", "info", "build", clean])).toThrow(/global option --log-level is not used/);
    } finally {
      rmSync(clean, { recursive: true, force: true });
    }
  });

  test("containerBuildArgs checks the context it returns and refuses options that hide inputs", () => {
    const clean = tempRoot({ Containerfile: "FROM scratch\n", app: "" });
    const leaky = tempRoot({ Containerfile: "FROM scratch\n", "id_ed25519": "" });
    try {
      expect(containerBuildArgs(clean, ["--tag", "localhost/trustiot-dev-default-x:dev"])).toEqual(["build", "--tag", "localhost/trustiot-dev-default-x:dev", clean]);
      expect(() => containerBuildArgs(leaky, [])).toThrow(/would send id_ed25519 to the builder/);
      expect(() => containerBuildArgs(clean, ["--ignorefile", "x"])).toThrow(/option --ignorefile is not allowed/);
      expect(() => containerBuildArgs(clean, ["--build-arg-file=x"])).toThrow(/option --build-arg-file is not allowed/);
      // Another directory sent to the builder, or mounted into RUN steps, is not checked.
      expect(() => containerBuildArgs(clean, ["--build-context", `keys=${leaky}`])).toThrow(/option --build-context is not allowed/);
      expect(() => containerBuildArgs(clean, ["--build-context=keys=/home"])).toThrow(/option --build-context is not allowed/);
      expect(() => containerBuildArgs(clean, ["-v", `${leaky}:/keys`])).toThrow(/option -v is not allowed/);
      expect(() => containerBuildArgs(clean, ["--volume", `${leaky}:/keys`])).toThrow(/option --volume is not allowed/);
      expect(() => containerBuildArgs(clean, [leaky])).toThrow(/options must not name another one/);
      expect(() => containerBuildArgs(clean, ["--tag"])).toThrow(/--tag needs a value/);
      // A build argument's value stays in the image history and may be computed at run time.
      const computed = ["VER", "SION=", "x"].join("");
      expect(() => containerBuildArgs(clean, ["--build-arg", computed])).toThrow(/option --build-arg is not allowed/);
      expect(() => containerBuildArgs(clean, ["--build-arg=VERSION=1"])).toThrow(/option --build-arg is not allowed/);
      // The options of the dev environment and of the release build pass.
      const file = join(clean, "Containerfile");
      const release = [
        "--platform", "linux/amd64", "--timestamp", "1791600000", "--pull=missing", "--format", "oci",
        "--label", "org.opencontainers.image.source=https://github.com/Laysi/trustiot-v2",
        "--label", `org.opencontainers.image.revision=${"0123456789abcdef".repeat(2)}01234567`,
        "--label", "org.opencontainers.image.version=0.1.0-rc.2",
        "--label", "org.opencontainers.image.title=identity",
        "--file", file, "--tag", "localhost/trustiot-release/identity:0.1.0-rc.2", "--secret", "id=npm,src=/run/secrets/npm",
      ];
      expect(containerBuildArgs(clean, release)).toEqual(["build", ...release, clean]);
      expect(containerBuildArgs(clean, ["--quiet", "--label", "io.laysi.trustiot-dev.instance=default", "--tag", "localhost/trustiot-dev-default-api:dev", "--file", file])).toHaveLength(9);
      // Labels and other values stored in the image cannot carry anything but their own shape.
      expect(() => containerBuildArgs(clean, ["--label", "TOKEN=ghp_x"])).toThrow(/label TOKEN is not one the builds set/);
      expect(() => containerBuildArgs(clean, ["--label", "org.opencontainers.image.title=hunter2 secret"])).toThrow(/label org\.opencontainers\.image\.title needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--label=org.opencontainers.image.source=https://user:pw@github.com/a/b"])).toThrow(/needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--label", "org.opencontainers.image.revision"])).toThrow(/needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--platform", "linux/amd64 --x"])).toThrow(/--platform needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--timestamp", "now"])).toThrow(/--timestamp needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--pull=sometimes"])).toThrow(/--pull needs a value of the form/);
      // Tags take only the shapes the builds use, and no token fits in them.
      const token = ["ghp", "0123456789abcdefghijABCDEFGHIJ"].join("_");
      expect(() => containerBuildArgs(clean, ["--tag", `localhost/${token}:dev`])).toThrow(/--tag needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--tag", "ghcr.io/laysi/identity:latest"])).toThrow(/--tag needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--tag", "localhost/trustiot-dev-default-a1b2c3d4e5f6a7b8c9d0e1f2:dev"])).toThrow(/--tag needs a value of the form/);
      expect(() => containerBuildArgs(clean, ["--label", "org.opencontainers.image.title=a1b2c3d4e5f6a7b8c9d0e1f2"])).toThrow(/label org\.opencontainers\.image\.title needs a value/);
      expect(containerBuildArgs(clean, ["--tag", "localhost/trustiot-dev-default-example-service:dev"])).toHaveLength(4);
      expect(containerBuildArgs(clean, ["--tag", "localhost/trustiot-e2e/identity-double:nightly"])).toHaveLength(4);
      // Relative paths come back absolute, so another working directory cannot change what is built.
      const relativeContext = relative(process.cwd(), clean);
      expect(containerBuildArgs(relativeContext, ["--file", relative(process.cwd(), file)])).toEqual([
        "build",
        "--file",
        file,
        clean,
      ]);
      expect(containerBuildArgs(relativeContext, [`--file=${relative(process.cwd(), file)}`])).toEqual(["build", `--file=${file}`, clean]);
      expect(() => containerBuildArgs(clean, ["--iidfile", "/etc/passwd"])).toThrow(/option --iidfile is not allowed/);
      expect(containerBuildArgs(clean, ["--tag", "localhost/trustiot-dev-default-x:dev"])).toHaveLength(4);
    } finally {
      for (const root of [clean, leaky]) rmSync(root, { recursive: true, force: true });
    }
  });

  test("shell scripts and workflow steps do not build images themselves", () => {
    expect(messages(checkBuildInvocations("deploy/build.sh", "set -e\ndocker buildx build -t x .\n"))).toEqual([
      expect.stringMatching(/^deploy\/build\.sh:2 builds a container image directly/),
    ]);
    expect(checkBuildInvocations(".github/workflows/x.yml", "steps:\n  - run: podman build .\n")).toHaveLength(1);
    for (const command of [
      "podman image build .",
      "docker image build .",
      "docker builder build .",
      "buildah bud .",
      "nerdctl build .",
      "docker compose build",
      "podman compose build api",
      "docker-compose build",
      "podman-compose build",
      "docker buildx bake",
      "docker compose -f compose.yaml build",
    ]) {
      expect(checkBuildInvocations("deploy/build.sh", `${command}\n`), command).toHaveLength(1);
    }
    expect(checkBuildInvocations(".github/workflows/x.yml", "steps:\n  - run: bun scripts/release/build.ts\n")).toEqual([]);
  });

  test("the guard refuses a context that would send a key file to the builder", () => {
    const clean = tempRoot({ Containerfile: "FROM scratch\n", app: "" });
    const leaky = tempRoot({ Containerfile: "FROM scratch\n", app: "", ".env": "X=1\n", "certs/server.key": "" });
    const ignored = tempRoot({ Containerfile: "FROM scratch\n", ".containerignore": "*\n!app\n", app: "", ".env": "X=1\n" });
    try {
      expect(() => assertBuildContext(clean)).not.toThrow();
      expect(() => assertBuildContext(leaky)).toThrow(/would send \.env, certs\/server\.key to the builder/);
      expect(() => assertBuildContext(ignored)).not.toThrow();
      expect(secretFilesInContext(leaky)).toEqual([".env", "certs/server.key"]);
    } finally {
      for (const root of [clean, leaky, ignored]) rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("ignore files a build command could swap in", () => {
  test("rejects podman's --ignorefile", () => {
    const script = 'await run(["podman", "build", "--ignorefile", "custom.ignore", "-f", file, "."]);\n';
    expect(messages(checkBuildArgsInText("scripts/build.ts", script))).toEqual([
      expect.stringMatching(/^scripts\/build\.ts:1 --ignorefile replaces the build context's own ignore file/),
    ]);
  });

  test("rejects Dockerfile-specific ignore files anywhere in the repository", () => {
    const root = tempRoot({
      ".dockerignore": "*\n",
      "deploy/images/x/Containerfile": `FROM scratch@${DIGEST}\nCOPY bin /bin/app\n`,
      "deploy/images/x/Containerfile.dockerignore": "*\n!bin\n",
      "deploy/images/x/.containerignore": "*\n!bin\n",
    });
    try {
      expect(messages(checkRepository(root).violations)).toEqual([
        expect.stringMatching(/^deploy\/images\/x\/Containerfile\.dockerignore:0 a Dockerfile-specific ignore file replaces/),
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("secrets in workflow run scripts", () => {
  test("finds whole expressions, braces inside strings included", () => {
    expect(githubExpressions("a ${{ format('{0}}', x) }} b ${{ 'it''s }}' }} c ${{ open")).toEqual([
      "${{ format('{0}}', x) }}",
      "${{ 'it''s }}' }}",
      "${{ open",
    ]);
  });

  test("recognises the secrets context in any letter case and as a whole", () => {
    for (const expression of [
      "${{ Secrets.TOKEN }}",
      "${{ toJSON(secrets) }}",
      "${{ SECRETS['TOKEN'] }}",
      "${{ secrets[format('{0}', 'TOKEN')] }}",
    ]) {
      const action = `runs:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo "${expression}"\n`;
      expect(checkWorkflowSecrets(".github/actions/x/action.yml", action), expression).toHaveLength(1);
    }
    expect(checkBuildArgsInText("x.sh", "podman build --build-arg VERSION=${{ Secrets.V }} .\n")).toHaveLength(1);
  });

  test("rejects ${{ secrets.* }} inside run and accepts it in env and with", () => {
    const workflow = `on: push
permissions: {}
jobs:
  notes:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - env:
          API_KEY: \${{ secrets.AI_API_KEY }}
        run: ./notes.sh
      - uses: Laysi/release-notes-action@${"b".repeat(40)} # v1
        with:
          api-key: \${{ secrets.AI_API_KEY }}
      - run: |
          echo start
          curl -H "Authorization: Bearer \${{ secrets.AI_API_KEY }}" https://api.example.test
`;
    expect(messages(checkWorkflowSecrets(".github/workflows/notes.yml", workflow))).toEqual([
      expect.stringMatching(/^\.github\/workflows\/notes\.yml:17 jobs\.notes\.steps\[2\]\.run writes a secret into the script; pass it through the step's env/),
    ]);
  });

  test("rejects the secrets context in workflow- and job-level env and other places", () => {
    const workflow = `on: push
permissions: {}
env:
  TOKEN: \${{ secrets.WORKFLOW_TOKEN }}
jobs:
  deploy:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    env:
      KEY: \${{ secrets.JOB_KEY }}
    container:
      image: ghcr.io/laysi/runner@sha256:${"c".repeat(64)}
      credentials:
        username: bot
        password: \${{ secrets.REGISTRY_PASSWORD }}
    services:
      db:
        image: postgres
        credentials:
          password: \${{ secrets.SERVICE_PASSWORD }}
        env:
          POSTGRES_PASSWORD: \${{ secrets.DB_PASSWORD }}
    steps:
      - env:
          STEP_KEY: \${{ secrets.STEP_KEY }}
        run: ./deploy.sh
  reuse:
    uses: ./.github/workflows/reusable.yml
    secrets:
      token: \${{ secrets.REUSE_TOKEN }}
`;
    expect(messages(checkWorkflowSecrets(".github/workflows/deploy.yml", workflow))).toEqual([
      expect.stringMatching(/^\.github\/workflows\/deploy\.yml:4 env\.TOKEN reads a secret outside a step's env or with/),
      expect.stringMatching(/^\.github\/workflows\/deploy\.yml:11 jobs\.deploy\.env\.KEY reads a secret outside a step's env or with/),
      expect.stringMatching(/^\.github\/workflows\/deploy\.yml:23 jobs\.deploy\.services\.db\.env\.POSTGRES_PASSWORD reads a secret/),
    ]);
  });

  test("rejects actions that build images", () => {
    for (const action of ["docker/build-push-action", "docker/bake-action", "redhat-actions/buildah-build", "depot/build-push-action", "aevea/action-kaniko"]) {
      const workflow = `on: push\njobs:\n  x:\n    steps:\n      - uses: ${action}@${"d".repeat(40)} # v1\n`;
      expect(messages(checkWorkflowSecrets(".github/workflows/x.yml", workflow)), action).toEqual([
        expect.stringMatching(/^\.github\/workflows\/x\.yml:5 jobs\.x\.steps\[0\] builds an image with /),
      ]);
    }
    const other = `on: push\njobs:\n  x:\n    steps:\n      - uses: actions/checkout@${"e".repeat(40)} # v5\n      - uses: docker/login-action@${"f".repeat(40)} # v3\n`;
    expect(checkWorkflowSecrets(".github/workflows/x.yml", other)).toEqual([]);
  });

  test("also checks composite actions", () => {
    const action = "runs:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo ${{ secrets['TOKEN'] }}\n";
    expect(messages(checkWorkflowSecrets(".github/actions/x/action.yml", action))).toEqual([
      expect.stringMatching(/^\.github\/actions\/x\/action\.yml:5 runs\.steps\[0\]\.run writes a secret/),
    ]);
  });
});

describe("checkSecretManifests", () => {
  // Bytes that do not read as text, standing in for ciphertext (deterministic, so no flakes).
  const opaque = (length: number, seed: number) =>
    Buffer.from(Array.from({ length }, (_, i) => (seed + i * 151) % 256 | 0x80)).toString("base64");
  // The shape SOPS writes: a 32-byte IV, a 16-byte tag, an age recipient and its wrapped key.
  const iv = opaque(32, 1);
  const tag = opaque(16, 2);
  const enc = (data: string, ivPart = iv) => `ENC[AES256_GCM,data:${data},iv:${ivPart},tag:${tag},type:str]`;
  const recipient = `age1${"qpzry9x8gf2tvdw0s3jn54khce6mua7l".repeat(2).slice(0, 58)}`;
  // age writes base64 without padding.
  const unpadded = (length: number, seed: number) => opaque(length, seed).replace(/=+$/, "");
  const ageFile = (payload: Buffer, stanzas = `-> X25519 ${unpadded(32, 3)}\n${unpadded(32, 4)}\n`) => {
    const header = Buffer.from(`age-encryption.org/v1\n${stanzas}--- ${unpadded(32, 5)}\n`);
    const lines = Buffer.concat([header, payload]).toString("base64").match(/.{1,64}/g) ?? [];
    return ["-----BEGIN AGE ENCRYPTED FILE-----", ...lines, "-----END AGE ENCRYPTED FILE-----"]
      .map((line) => `        ${line}`)
      .join("\n");
  };
  const wrappedKey = ageFile(Buffer.from(opaque(48, 6), "base64"));
  const encrypted = `apiVersion: v1
kind: Secret
metadata:
  name: trustiot-smtp
stringData:
  username: ${enc(opaque(12, 7))}
  password: ${enc(opaque(24, 8))}
sops:
  age:
    - recipient: ${recipient}
      enc: |
${wrappedKey}
  lastmodified: "2026-10-10T00:00:00Z"
  mac: ${enc(opaque(32, 9))}
  encrypted_regex: ^(data|stringData)$
  version: "3.10.2"
`;

  test("accepts a SOPS-encrypted Secret and an empty Secret", () => {
    expect(checkSecretManifests("deploy/flux/smtp.sops.yaml", encrypted)).toEqual([]);
    // Grease stanzas next to the X25519 stanza are part of the format.
    const greased = ageFile(Buffer.from(opaque(48, 6), "base64"), `-> X25519 ${unpadded(32, 3)}\n${unpadded(32, 4)}\n-> 9z-grease A b\n${unpadded(20, 11)}\n`);
    expect(checkSecretManifests("deploy/flux/smtp.sops.yaml", encrypted.replace(wrappedKey, greased))).toEqual([]);
    expect(checkSecretManifests("x.yaml", "apiVersion: v1\nkind: Secret\nmetadata:\n  name: empty\n")).toEqual([]);
  });

  test("a value or sops block that only looks encrypted does not pass", () => {
    const password = /password: ENC\[[^\n]*/;
    for (const forged of [
      encrypted.replace(password, "password: ENC[plain-password]"),
      // Readable text wrapped in the right shape: base64 of plain text, or a readable IV.
      encrypted.replace(password, `password: ${enc(Buffer.from("plain-password").toString("base64"))}`),
      encrypted.replace(password, `password: ${enc(opaque(24, 8), Buffer.from("A".repeat(32)).toString("base64"))}`),
      encrypted.replace(password, `password: ENC[AES256_GCM,data:${opaque(24, 8)},iv:short,tag:${tag},type:str]`),
      encrypted.replace(/  mac: [^\n]*\n/, ""),
      encrypted.replace(recipient, "age1example"),
      encrypted.replace(wrappedKey, ageFile(Buffer.from("the data key in plain text, not encrypted at all"))),
      encrypted.replace(wrappedKey, "        -----BEGIN AGE ENCRYPTED FILE-----\n        -----END AGE ENCRYPTED FILE-----"),
      // The wrapped file key in the X25519 stanza must be 32 opaque bytes, not readable text.
      encrypted.replace(wrappedKey, ageFile(Buffer.from(opaque(48, 6), "base64"), `-> X25519 ${unpadded(32, 3)}\n${Buffer.from("plain data key, readable bytes!!").toString("base64").replace(/=+$/, "")}\n`)),
      encrypted.replace(wrappedKey, ageFile(Buffer.from(opaque(48, 6), "base64"), `-> X25519 ${unpadded(32, 3)}\nc2hvcnQ\n`)),
      encrypted.replace(wrappedKey, ageFile(Buffer.from(opaque(48, 6), "base64"), `-> X25519 ${unpadded(32, 3)}\n${unpadded(32, 4)}\n${unpadded(16, 10)}\n`)),
      encrypted.replace(wrappedKey, ageFile(Buffer.from(opaque(48, 6), "base64"), `-> X25519 short\n${unpadded(32, 4)}\n`)),
      encrypted.replace(wrappedKey, ageFile(Buffer.from(opaque(48, 6), "base64"), `-> scrypt ${unpadded(16, 3)} 18\n${unpadded(32, 4)}\n`)),
      encrypted.replace(wrappedKey, ageFile(Buffer.from(opaque(48, 6), "base64"), "")),
    ]) {
      expect(checkSecretManifests("deploy/flux/smtp.sops.yaml", forged)).toHaveLength(1);
    }
  });

  test("rejects plain-text Secrets, also in multi-document files and Lists", () => {
    const plain = `apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
data:
  mode: saas
---
apiVersion: v1
kind: Secret
metadata:
  name: trustiot-ai-chat
stringData:
  api-key: sk-plain
`;
    expect(messages(checkSecretManifests("deploy/flux/ai.yaml", plain))).toEqual([
      expect.stringMatching(/^deploy\/flux\/ai\.yaml:9 Secret trustiot-ai-chat is not SOPS-encrypted/),
    ]);
    // A sops block alone does not help when a value is still plain text.
    const partly = encrypted.replace(/password: ENC\[[^\n]*/, "password: hunter2");
    expect(checkSecretManifests("deploy/flux/smtp.sops.yaml", partly)).toHaveLength(1);
    const list = "apiVersion: v1\nkind: List\nitems:\n  - apiVersion: v1\n    kind: Secret\n    metadata:\n      name: x\n    data:\n      k: dg==\n";
    expect(checkSecretManifests("list.yaml", list)).toHaveLength(1);
  });

  test("rejects secretGenerator and Secrets in templates that cannot be verified", () => {
    for (const source of ["literals:\n      - password=x", "files:\n      - password.txt", "envs:\n      - smtp.env"]) {
      const kustomization = `resources: []\nsecretGenerator:\n  - name: smtp\n    ${source}\n`;
      expect(messages(checkSecretManifests("deploy/flux/kustomization.yaml", kustomization))).toEqual([
        expect.stringMatching(/^deploy\/flux\/kustomization\.yaml:2 secretGenerator builds Secrets from literals or files that are not encrypted/),
      ]);
    }
    const template = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {{ .Release.Name }}\ndata:\n  {{- toYaml .Values.x | nindent 2 }}\n";
    expect(checkSecretManifests("deploy/chart/templates/secret.yaml", template)).toHaveLength(1);
    // A templated kind could render a Secret, so it fails closed; so does a .tpl helper.
    const dynamic = "apiVersion: v1\nkind: {{ .Values.kind }}\nmetadata:\n  name: x\ndata:\n  {{- toYaml .Values.x | nindent 2 }}\n";
    expect(messages(checkSecretManifests("deploy/chart/templates/any.yaml", dynamic))).toEqual([
      expect.stringMatching(/^deploy\/chart\/templates\/any\.yaml:2 a template that declares a Secret \(or a templated kind\)/),
    ]);
    expect(checkTemplateText("deploy/chart/templates/_secret.tpl", '{{- define "x" -}}\n  kind: Secret\n{{- end }}\n')).toHaveLength(1);
    // A quoted templated kind parses as YAML and is still caught.
    const quoted = 'apiVersion: v1\nkind: "{{ .Values.kind }}"\nmetadata:\n  name: x\nstringData:\n  value: "{{ .Values.value }}"\n';
    expect(messages(checkSecretManifests("deploy/chart/templates/any.yaml", quoted))).toEqual([
      expect.stringMatching(/^deploy\/chart\/templates\/any\.yaml:2 a template that declares a Secret \(or a templated kind\)/),
    ]);
    // Other templates are skipped.
    expect(checkSecretManifests("deploy/chart/templates/cm.yaml", "kind: ConfigMap\ndata:\n  {{- x }}\n")).toEqual([]);
    expect(checkTemplateText("deploy/chart/templates/_service.tpl", "kind: Deployment\nspec: {{ .x }}\n")).toEqual([]);
  });

  test("a template may declare only an empty Secret that the migration Job fills", () => {
    const file = "deploy/chart/templates/passwords.yaml";
    const empty = [
      '{{- range include "trustiot.migrate.passwordSecrets" . | fromYamlArray }}',
      "---",
      "apiVersion: v1",
      "kind: Secret",
      "metadata:",
      "  name: {{ .name }}",
      "  labels:",
      '    {{- include "trustiot.componentLabels" (dict "root" $ "name" .component) | nindent 4 }}',
      "    trustiot.io/filled-by: migrate",
      "  annotations:",
      "    helm.sh/resource-policy: keep",
      "type: Opaque",
      "{{- end }}",
      "",
    ].join("\n");
    expect(checkSecretManifests(file, empty)).toEqual([]);
    expect(checkTemplateText(file, empty)).toEqual([]);
    // The chart's own template passes.
    const chart = readFileSync(join(import.meta.dir, "..", "..", file), "utf8");
    expect(checkSecretManifests(file, chart)).toEqual([]);
    // Anything that carries or could add a value fails, and so does a Secret without the label.
    for (const [what, source] of [
      ["data", empty.replace("type: Opaque", "type: Opaque\ndata:\n  x: eA==")],
      ["stringData", empty.replace("type: Opaque", "stringData:\n  x: y\ntype: Opaque")],
      ["a templated data key", empty.replace("type: Opaque", "type: Opaque\ndata: {{ .Values.x }}")],
      ["a templated type", empty.replace("type: Opaque", "type: {{ .Values.type }}")],
      ["an action at the Secret's own level", empty.replace("type: Opaque", "type: Opaque\n{{ toYaml .Values.extra }}")],
      ["an action that renders at that level", empty.replace("type: Opaque", "type: Opaque\n{{- toYaml .Values.extra | nindent 0 }}")],
      ["no filled-by label", empty.replace("    trustiot.io/filled-by: migrate\n", "")],
      ["another filler", empty.replace("filled-by: migrate", "filled-by: someone")],
    ] as const) {
      expect(messages(checkSecretManifests(file, source)), what).toEqual([
        expect.stringMatching(/^deploy\/chart\/templates\/passwords\.yaml:4 a template that declares a Secret/),
      ]);
    }
    // The label in another document does not excuse a Secret.
    const two = `${empty}---\napiVersion: v1\nkind: Secret\nmetadata:\n  name: other\ndata:\n  {{- toYaml .Values.x | nindent 2 }}\n`;
    expect(messages(checkSecretManifests(file, two))).toEqual([
      expect.stringMatching(/^deploy\/chart\/templates\/passwords\.yaml:16 a template that declares a Secret/),
    ]);
  });

  test("template declarations are found with comments and next to template actions, references by name are not", () => {
    const commented = [
      "{{- if .Values.enabled }}",
      "apiVersion: v1",
      "kind: Secret # generated",
      "metadata:",
      "  name: {{ .Release.Name }}",
      "{{- end }}",
      "---",
      "apiVersion: v1",
      "kind: 'Secret'",
      "{{ .Values.broken",
    ].join("\n");
    expect(messages(checkTemplateText("deploy/chart/templates/x.yaml", commented))).toEqual([
      expect.stringMatching(/^deploy\/chart\/templates\/x\.yaml:3 a template that declares a Secret/),
      expect.stringMatching(/^deploy\/chart\/templates\/x\.yaml:9 a template that declares a Secret/),
    ]);
    // A List item that declares a Secret has apiVersion beside it.
    const list = "apiVersion: v1\nkind: List\nitems:\n  - apiVersion: v1\n    kind: Secret\n    metadata:\n      name: {{ .x }}\n";
    expect(checkTemplateText("deploy/chart/templates/list.yaml", list)).toHaveLength(1);
    const listKindFirst = "items:\n  - kind: Secret\n    apiVersion: v1\n    metadata:\n      name: x\n";
    expect(checkTemplateText("deploy/chart/templates/list.yaml", listKindFirst)).toHaveLength(1);
    // The Gateway's TLS certificate is a reference to a Secret by name, not a declaration.
    const gateway = [
      "apiVersion: gateway.networking.k8s.io/v1",
      "kind: Gateway",
      "spec:",
      "  listeners:",
      "    - name: https",
      "      tls:",
      "        certificateRefs:",
      "          - kind: Secret",
      '            group: ""',
      "            name: {{ $v.ingress.direct.tlsSecret }}",
      "      targetRef:",
      "        {{- if .x }}",
      "        kind: Secret",
      "        {{- end }}",
      "        name: tls",
    ].join("\n");
    expect(checkTemplateText("deploy/chart/templates/ingress.yaml", gateway)).toEqual([]);
    expect(checkSecretManifests("deploy/chart/templates/ingress.yaml", gateway)).toEqual([]);
  });

  test("the repository scan skips test doubles kept as test evidence, not other scripts", () => {
    const wrapper = '#!/bin/sh\nexec /usr/bin/podman "$@"\n';
    const root = tempRoot({
      ".dockerignore": "*\n",
      "docs/test-reports/data/2026-10-11-1-x/podman-wrapper.sh": wrapper,
      "scripts/podman-wrapper.sh": wrapper,
    });
    try {
      expect(messages(checkRepository(root).violations)).toEqual([
        expect.stringMatching(/^scripts\/podman-wrapper\.sh:2 runs a container tool whose subcommand is not fixed text/),
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("the repository scan reads .tpl helpers and JSON manifests", () => {
    const root = tempRoot({
      ".dockerignore": "*\n",
      "deploy/chart/templates/_secret.tpl": "kind: Secret\n",
      "deploy/flux/secret.json": JSON.stringify({ apiVersion: "v1", kind: "Secret", metadata: { name: "j" }, stringData: { setting: "plain" } }, null, 2),
      "api/openapi.json": JSON.stringify({ openapi: "3.1.0", components: { schemas: { Secret: { type: "object" } } } }),
    });
    try {
      expect(messages(checkRepository(root).violations)).toEqual([
        expect.stringMatching(/^deploy\/chart\/templates\/_secret\.tpl:1 a template that declares a Secret/),
        expect.stringMatching(/^deploy\/flux\/secret\.json:3 Secret j is not SOPS-encrypted/),
      ]);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("secret-looking names", () => {
  test("match the chart's rule", () => {
    for (const name of ["DB_PASSWORD", "GITHUB_TOKEN", "API_KEY", "apikey", "SENTRY_DSN", "KEY", "SIGNING_KEY", "PRIVATE_PEM", "AWS_CREDENTIALS", "PASSPHRASE"]) {
      expect(SECRET_LIKE_NAME.test(name), name).toBe(true);
    }
    for (const name of ["VERSION", "TARGETARCH", "TRUSTIOT_INTERNAL_IDENTITY_KEYS", "KEYBOARD", "RUST_LOG"]) {
      expect(SECRET_LIKE_NAME.test(name), name).toBe(false);
    }
  });
});

describe("repository scan", () => {
  let root: string;

  beforeAll(() => {
    root = tempRoot({
      ".dockerignore": "*\n",
      "deploy/images/good/Containerfile": `FROM scratch@${DIGEST}\nCOPY bin /bin/app\n`,
      "deploy/images/bad.Containerfile": "FROM scratch\nCOPY id_rsa /root/.ssh/\nARG DEPLOY_TOKEN\n",
      "services/x/Dockerfile.dev": "FROM scratch\nCOPY . /src\n",
      "scripts/build.ts": 'run(["podman", "build", "--build-arg", "SIGNING_KEY=" + key]);\n',
      // Tests spell out violations on purpose and are not scanned.
      "scripts/build.test.ts": 'run(["podman", "build", "--build-arg", "SIGNING_KEY=x"]);\n',
      ".github/workflows/release.yml": "on: push\njobs:\n  a:\n    steps:\n      - run: echo ${{ secrets.TOKEN }}\n",
      "deploy/flux/secret.yaml": "apiVersion: v1\nkind: Secret\nmetadata:\n  name: plain\nstringData:\n  token: abc\n",
      "docs/notes.md": "COPY .env /app and --build-arg GITHUB_TOKEN in prose are not scanned\n",
      "node_modules/pkg/Dockerfile": "FROM scratch\nCOPY .env /\n",
      "target/debug/Dockerfile": "FROM scratch\nCOPY .env /\n",
      // A nested checkout or git worktree (for example .claude/worktrees/*) is another repository.
      "nested/.git": "gitdir: /elsewhere\n",
      "nested/Dockerfile": "FROM scratch\nCOPY .env /\n",
    });
    // A linked build file is checked where it points; a dangling link is skipped.
    symlinkSync(join(root, "deploy/images/good/Containerfile"), join(root, "deploy/linked.Containerfile"));
    symlinkSync(join(root, "missing"), join(root, "deploy/dangling.Containerfile"));
  });

  afterAll(() => {
    rmSync(root, { recursive: true, force: true });
  });

  test("lists files outside dependencies, build output and nested checkouts", () => {
    const files = listFiles(root);
    expect(files).toContain("deploy/linked.Containerfile");
    expect(files.filter((file) => /node_modules|target|nested|dangling/.test(file))).toEqual([]);
  });

  test("finds every container build file and reports each violation", () => {
    const result = checkRepository(root);
    expect(result.buildFiles).toEqual([
      "deploy/images/bad.Containerfile",
      "deploy/images/good/Containerfile",
      "deploy/linked.Containerfile",
      "services/x/Dockerfile.dev",
    ]);
    expect(messages(result.violations)).toEqual([
      expect.stringMatching(/^\.github\/workflows\/release\.yml:5 jobs\.a\.steps\[0\]\.run writes a secret/),
      expect.stringMatching(/^deploy\/flux\/secret\.yaml:2 Secret plain is not SOPS-encrypted/),
      expect.stringMatching(/^deploy\/images\/bad\.Containerfile:2 COPY id_rsa copies an SSH private key/),
      expect.stringMatching(/^deploy\/images\/bad\.Containerfile:3 ARG DEPLOY_TOKEN looks like a secret/),
      expect.stringMatching(/^scripts\/build\.ts:1 build argument SIGNING_KEY looks like a secret/),
      expect.stringMatching(/^scripts\/build\.ts:1 writes a container build command/),
      expect.stringMatching(/^services\/x\/Dockerfile\.dev:2 COPY \. copies the whole build context/),
    ]);
  });

  test("the command fails with annotations on a fixture that copies a key file", () => {
    const result = Bun.spawnSync([process.execPath, SCRIPT, root], {
      env: { ...process.env, GITHUB_ACTIONS: "true" },
    });
    const stdout = result.stdout.toString();
    expect(result.exitCode).toBe(1);
    expect(stdout).toContain("deploy/images/bad.Containerfile:2: COPY id_rsa copies an SSH private key into the image");
    expect(stdout).toContain("::error file=deploy/images/bad.Containerfile,line=2,title=secret handling::");
    expect(stdout).toContain("secret handling: 7 violation(s)");
  });

  test("the command fails when the root cannot be read", () => {
    const result = Bun.spawnSync([process.execPath, SCRIPT, join(root, "does-not-exist")]);
    expect(result.exitCode).toBe(2);
    expect(result.stderr.toString()).toContain("cannot read the repository");
  });

  test("annotations escape the message", () => {
    expect(annotation({ file: "a,b.yml", line: 0, message: "100%\nnext" })).toBe(
      "::error file=a%2Cb.yml,title=secret handling::100%25%0Anext",
    );
  });

  test("this repository passes", () => {
    const result = Bun.spawnSync([process.execPath, SCRIPT, REPOSITORY_ROOT]);
    expect(result.stdout.toString()).toContain("no violations");
    expect(result.exitCode).toBe(0);
  });
});
