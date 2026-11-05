/**
 * `bun run test` (package.json): every frontend test, in several `bun test` processes run one
 * after another. A single process over every file keeps what each test file leaves behind until it
 * exits: on 2026-11-05 the suite (about 1390 tests in 84 files) peaked at about 14 GB in one process,
 * and `--isolate` did not lower it. Separate processes give the memory back between shards.
 * frontend/README.md, 「測試的寫法」.
 *
 * With arguments (a path, `-t <filter>`, any `bun test` option) it runs one `bun test` with them,
 * unchanged: a filtered run is small, and a path may fall into a single shard.
 * FRONTEND_TEST_SHARDS sets the number of shards (default 8).
 */

/** The counts in the summary `bun test` prints at the end of a run. */
export interface RunSummary {
  pass: number;
  fail: number;
  tests: number;
  files: number;
}

/** Reads the summary lines of one `bun test` run's output; a count that is missing is 0. */
export function parseSummary(output: string): RunSummary {
  const count = (pattern: RegExp) => Number(pattern.exec(output)?.[1] ?? 0);
  return {
    pass: count(/^\s*(\d+) pass$/m),
    fail: count(/^\s*(\d+) fail$/m),
    tests: count(/^Ran (\d+) tests?\b/m),
    files: count(/^Ran \d+ tests? across (\d+) files?\b/m),
  };
}

/** The number of shards from FRONTEND_TEST_SHARDS, or the default; anything else is an error. */
export function shardCount(value: string | undefined): number {
  if (value === undefined || value === "") return 8;
  const shards = Number(value);
  if (!Number.isInteger(shards) || shards < 1) {
    throw new Error(`FRONTEND_TEST_SHARDS must be a whole number of at least 1, got "${value}"`);
  }
  return shards;
}

/** Runs `bun test` with `args`, passing its output through, and returns its exit code and output. */
async function bunTest(args: readonly string[]): Promise<{ code: number; output: string }> {
  const proc = Bun.spawn([process.execPath, "test", ...args], {
    stdin: "inherit",
    stdout: "pipe",
    stderr: "pipe",
  });
  const decoder = new TextDecoder();
  let output = "";
  const pipe = async (stream: ReadableStream<Uint8Array>, write: (chunk: Uint8Array) => void) => {
    for await (const chunk of stream) {
      write(chunk);
      output += decoder.decode(chunk, { stream: true });
    }
  };
  await Promise.all([
    pipe(proc.stdout, (chunk) => process.stdout.write(chunk)),
    pipe(proc.stderr, (chunk) => process.stderr.write(chunk)),
  ]);
  return { code: await proc.exited, output };
}

async function main(): Promise<number> {
  const args = Bun.argv.slice(2);
  if (args.length > 0) return (await bunTest(args)).code;

  const shards = shardCount(process.env.FRONTEND_TEST_SHARDS);
  const total: RunSummary = { pass: 0, fail: 0, tests: 0, files: 0 };
  const failed: number[] = [];
  for (let shard = 1; shard <= shards; shard += 1) {
    process.stdout.write(`\n== bun test --shard=${shard}/${shards}\n`);
    const { code, output } = await bunTest([`--shard=${shard}/${shards}`]);
    const summary = parseSummary(output);
    total.pass += summary.pass;
    total.fail += summary.fail;
    total.tests += summary.tests;
    total.files += summary.files;
    if (code !== 0) failed.push(shard);
  }
  process.stdout.write(
    `\nAll shards: ${total.pass} pass, ${total.fail} fail, ${total.tests} tests across ${total.files} files in ${shards} shards.\n`,
  );
  if (failed.length > 0) {
    console.error(`Shards that failed: ${failed.join(", ")} of ${shards}.`);
    return 1;
  }
  return 0;
}

if (import.meta.main) process.exit(await main());
