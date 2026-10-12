/**
 * Rules for the Containerfiles of deploy/images/ that plan checks besides lib.ts's
 * baseImageProblems. Kept apart from lib.ts on purpose: scripts/ci/changes.ts imports lib.ts, so
 * every change to lib.ts runs every CI job. Like lib.ts, it only parses; containerfile.test.ts
 * covers it without tools.
 */

/** The two spellings of the build machine's platform in `FROM --platform=`. */
const BUILD_PLATFORM_VALUES = new Set(["$BUILDPLATFORM", "${BUILDPLATFORM}"]);

/**
 * Returns a problem for each `RUN` of a Containerfile that would execute in a stage built for
 * the image's platform (linux/arm64) instead of the build machine's, or an empty list. Images
 * must build on any machine without an emulator: the arm64 release runner, and x86 machines
 * for the local release flow (docs/architecture/ci-release.md). So a program may only run in a
 * `FROM --platform=$BUILDPLATFORM` stage, or a stage based on one, and its output is copied into
 * the image. On the x86 runner such a RUN failed for want of an emulator; on the arm64 runner it
 * would run, so this check keeps the rule.
 */
export function targetStageRunProblems(containerfile: string): string[] {
  const problems: string[] = [];
  /** Whether each named stage runs on the build machine's platform. */
  const stages = new Map<string, boolean>();
  let stage: { line: number; build: boolean } | undefined;
  for (const { line, text } of containerfileInstructions(containerfile)) {
    const words = text.split(/\s+/);
    const keyword = words[0]?.toUpperCase();
    if (keyword === "FROM") {
      const platform = fromPlatform(words.slice(1));
      const args = words.slice(1).filter((word) => !word.startsWith("--"));
      const base = (args[0] ?? "").toLowerCase();
      const build = platform === undefined ? (stages.get(base) ?? false) : BUILD_PLATFORM_VALUES.has(platform);
      stage = { line, build };
      if (args[1]?.toUpperCase() === "AS" && args[2] !== undefined) {
        stages.set(args[2].toLowerCase(), build);
      }
    } else if (keyword === "RUN" && stage !== undefined && !stage.build) {
      problems.push(
        `line ${line}: RUN in the stage of line ${stage.line}, which builds for the image's platform; ` +
          "run programs only in a `FROM --platform=$BUILDPLATFORM` stage and COPY the result",
      );
    }
  }
  return problems;
}

/** The value of `--platform` among the options of a FROM line, unquoted, or undefined. */
function fromPlatform(words: readonly string[]): string | undefined {
  for (const [index, word] of words.entries()) {
    if (!word.startsWith("--")) {
      break;
    }
    const value = word === "--platform" ? words[index + 1] : word.startsWith("--platform=") ? word.slice("--platform=".length) : undefined;
    if (value !== undefined) {
      return value.replace(/^(["'])(.*)\1$/, "$2");
    }
  }
  return undefined;
}

/** The instructions that may open here-documents. */
const HEREDOC_INSTRUCTIONS = new Set(["RUN", "COPY", "ADD"]);

/**
 * The instructions of a Containerfile with the line each starts on: continuation lines are
 * joined, comment lines (also inside a continuation) are dropped, and here-document bodies
 * (`RUN <<EOF` ... `EOF`) are skipped, so their lines are never read as instructions.
 */
function containerfileInstructions(containerfile: string): { line: number; text: string }[] {
  const instructions: { line: number; text: string }[] = [];
  const lines = containerfile.split(/\r?\n/);
  let current: { line: number; text: string } | undefined;
  /** Here-documents the current instruction opened, in order: terminator and whether tabs are stripped. */
  const heredocs: { end: string; stripTabs: boolean }[] = [];
  for (const [index, raw] of lines.entries()) {
    const heredoc = heredocs[0];
    if (heredoc !== undefined) {
      if ((heredoc.stripTabs ? raw.replace(/^\t+/, "") : raw) === heredoc.end) {
        heredocs.shift();
      }
      continue;
    }
    const trimmed = raw.trim();
    // Docker also skips empty lines inside a continuation.
    if (trimmed.startsWith("#") || trimmed === "") {
      continue;
    }
    const continued = trimmed.endsWith("\\");
    const part = continued ? trimmed.slice(0, -1).trim() : trimmed;
    current = current === undefined ? { line: index + 1, text: part } : { line: current.line, text: `${current.text} ${part}`.trim() };
    if (!continued) {
      instructions.push(current);
      const keyword = current.text.split(/\s+/)[0]?.toUpperCase() ?? "";
      // Only these instructions take here-documents; `<<<` is a shell here-string.
      const opened = HEREDOC_INSTRUCTIONS.has(keyword) ? current.text.matchAll(/(?<!<)<<(?!<)(-?)\s*(["']?)([A-Za-z_][A-Za-z0-9_]*)\2/g) : [];
      for (const match of opened) {
        heredocs.push({ end: match[3] ?? "", stripTabs: match[1] === "-" });
      }
      current = undefined;
    }
  }
  if (current !== undefined) {
    instructions.push(current);
  }
  return instructions;
}
