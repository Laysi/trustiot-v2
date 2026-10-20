// Runs every test executable of the agent crate RUNS times (default 30) while 32 busy loops
// load the CPU, and prints the pass and fail counts with the load average every 10 rounds.
// Run from the repository root: bun docs/test-reports/data/2026-10-20-87-modbus-read/repeat-under-load.ts [RUNS]

const runs = Number(process.argv[2] ?? 30);
const loadavg = async () => (await Bun.file("/proc/loadavg").text()).split(" ").slice(0, 3).join(" ");

// The test executables, from cargo's build messages.
const build = Bun.spawnSync(
  ["cargo", "test", "-p", "agent", "--locked", "--no-run", "--message-format=json"],
  { stderr: "inherit" },
);
if (build.exitCode !== 0) throw new Error("cargo test --no-run failed");
const binaries = build.stdout
  .toString()
  .split("\n")
  .filter((line) => line.startsWith("{"))
  .map((line) => JSON.parse(line))
  .filter((message) => message.reason === "compiler-artifact" && message.executable && message.package_id.includes("edge/agent"))
  .map((message) => message.executable as string);
console.log(`test executables:\n  ${binaries.join("\n  ")}`);

const hog = Bun.spawn(["bun", "docs/test-reports/data/2026-10-19-97-fake-modbus-endpoint/cpu-hog.ts", "32"]);
try {
  await Bun.sleep(45_000); // let the load average climb
  console.log(`load before: ${await loadavg()}`);
  let passed = 0;
  let failed = 0;
  for (let round = 1; round <= runs; round++) {
    for (const binary of binaries) {
      const run = Bun.spawnSync([binary, "-q"]);
      if (run.exitCode === 0) {
        passed++;
      } else {
        failed++;
        console.log(`round ${round} failed: ${binary}\n${run.stdout.toString()}${run.stderr.toString()}`);
      }
    }
    if (round % 10 === 0) console.log(`after ${round} rounds: load ${await loadavg()}`);
  }
  console.log(`passed ${passed}, failed ${failed}`);
} finally {
  hog.kill();
}
