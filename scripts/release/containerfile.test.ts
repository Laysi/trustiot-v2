import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { targetStageRunProblems } from "./containerfile.ts";
import { RUST_CONTAINERFILE } from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

describe("targetStageRunProblems", () => {
  const pinned = `docker.io/library/debian:13@sha256:${"d".repeat(64)}`;

  test("allows RUN only in stages that build for the build machine's platform", () => {
    // The layout of an image that compiles in a build stage and copies the result
    // (deploy/images/README.md), with both spellings of the variable and quoting.
    const containerfile = [
      `FROM ${pinned} AS base`,
      `FROM --platform=$BUILDPLATFORM ${pinned} AS build`,
      "# a comment, then a continued RUN with a comment line inside it",
      "RUN set -eu; \\",
      "    # explains the next line",
      "    make",
      "",
      "FROM build AS more",
      "RUN make install",
      `FROM --platform="\${BUILDPLATFORM}" ${pinned} AS other`,
      "RUN true",
      "FROM base",
      "COPY --from=build /out /",
      "USER 65532",
    ].join("\n");
    expect(targetStageRunProblems(containerfile)).toEqual([]);
    expect(targetStageRunProblems(readFileSync(join(ROOT, RUST_CONTAINERFILE), "utf8"))).toEqual([]);
  });

  test("rejects RUN in a stage that builds for the image's platform", () => {
    expect(targetStageRunProblems(`FROM ${pinned}\nCOPY x /\nrun \\\n  apt-get update\n`)).toEqual([
      "line 3: RUN in the stage of line 1, which builds for the image's platform; run programs only in a `FROM --platform=$BUILDPLATFORM` stage and COPY the result",
    ]);
    // An explicit platform, the target platform, and a stage based on a target stage.
    expect(targetStageRunProblems(`FROM --platform=linux/arm64 ${pinned}\nRUN true\n`)).toHaveLength(1);
    expect(targetStageRunProblems(`FROM --platform linux/amd64 ${pinned}\nRUN true\n`)).toHaveLength(1);
    expect(targetStageRunProblems(`FROM --platform=$TARGETPLATFORM ${pinned}\nRUN true\n`)).toHaveLength(1);
    expect(targetStageRunProblems(`FROM ${pinned} AS base\nFROM base AS next\nRUN true\n`)[0]).toStartWith("line 3: RUN in the stage of line 2");
    // A build stage before it does not make a later target stage a build stage.
    const later = targetStageRunProblems(`FROM --platform=$BUILDPLATFORM ${pinned} AS build\nRUN make\nFROM ${pinned}\nRUN true\n`);
    expect(later).toHaveLength(1);
    expect(later[0]).toStartWith("line 4: RUN in the stage of line 3");
  });

  test("does not read here-document bodies as instructions", () => {
    const containerfile = [
      `FROM --platform=$BUILDPLATFORM ${pinned} AS build`,
      "RUN <<EOF",
      "FROM scratch",
      "EOF",
      "RUN true",
      "COPY <<-'CONF' /etc/x.conf",
      "\tRUN not an instruction",
      "\tCONF",
      `FROM ${pinned}`,
      "RUN <<<x cat",
    ].join("\n");
    const problems = targetStageRunProblems(containerfile);
    expect(problems).toHaveLength(1);
    expect(problems[0]).toStartWith("line 10: RUN in the stage of line 9");
  });
});
