// Runs on the build host in rcheck's copy of the branch (a fresh git repository, so the copy can
// be restored with git checkout): applies what each kind of Renovate pull request changes, runs
// the checks that pull request would pass or fail in CI, and restores the copy.
// Usage, from the repository root: bun pr-simulation.ts
import { readFileSync, writeFileSync } from "node:fs";

const CH_OLD = "110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c";
const CH_NEW = "dd81097e2c38827a438287d8ae5d4dc8afafbdd0e61e83376fdd7ed4580673b9";
const OT_OLD = "fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1";
const OT_NEW = "39923a8e431bd1f57be82411999d389fcfe40857492e4365456d97a4c1f74be6";

const run = (cmd: string[]) => {
  const result = Bun.spawnSync(cmd, { stdout: "pipe", stderr: "pipe" });
  return `${result.stdout.toString()}${result.stderr.toString()}`;
};
const summary = (output: string) =>
  output
    .split("\n")
    .filter((line) => /^\(fail\)| pass$| fail$/.test(line))
    .join("\n");
function edit(file: string, from: string, to: string) {
  const text = readFileSync(file, "utf8");
  if (!text.includes(from)) throw new Error(`${file}: ${from} not found`);
  writeFileSync(file, text.replaceAll(from, to));
}
function scenario(title: string, apply: () => void) {
  apply();
  console.log(`\n=== ${title}`);
  console.log(run(["git", "diff", "--stat"]).trim().split("\n").at(-1) ?? "");
  console.log(summary(run(["bun", "test", "deploy/chart", "-t", "bundled ClickHouse|OpenTelemetry collector"])));
  console.log(summary(run(["bun", "test", "scripts/nightly/renovate.test.ts"])));
  run(["git", "checkout", "--quiet", "--", "."]);
}

run(["bun", "install", "--cwd", "scripts", "--frozen-lockfile"]);
const CH_FILES = ["deploy/chart/values.yaml", "scripts/lib/stack.ts", "services/ingest/tests/common/mod.rs"];
scenario("A: the branch as committed", () => {});
scenario("B: ClickHouse 26.8 digest pull request (all three copies, as Renovate plans it)", () => {
  for (const file of CH_FILES) edit(file, CH_OLD, CH_NEW);
});
scenario("C: the same update in values.yaml only (what main's config would have opened)", () => {
  edit("deploy/chart/values.yaml", CH_OLD, CH_NEW);
});
scenario("D: a pull request to the next LTS minor, 27.3 (made-up digest)", () => {
  edit("deploy/chart/values.yaml", `tag: "26.8"\n      digest: sha256:${CH_OLD}`, `tag: "27.3"\n      digest: sha256:${CH_NEW}`);
  for (const file of CH_FILES.slice(1)) edit(file, `clickhouse-server:26.8@sha256:${CH_OLD}`, `clickhouse-server:27.3@sha256:${CH_NEW}`);
});
scenario("E: collector digest pull request, same version 0.161.0 (made-up digest)", () => {
  edit("deploy/chart/values.yaml", OT_OLD, OT_NEW);
});
scenario("F: collector version pull request, 0.162.0", () => {
  edit("deploy/chart/values.yaml", `tag: "0.161.0"\n      digest: sha256:${OT_OLD}`, `tag: "0.162.0"\n      digest: sha256:${OT_NEW}`);
});
scenario("G: k3s pull request to v1.36.6-k3s1 (nightly.yml only; the host pins need the installer SHA-256)", () => {
  edit(".github/workflows/nightly.yml", "rancher/k3s:v1.36.5-k3s1@", "rancher/k3s:v1.36.6-k3s1@");
});
scenario("H: Istio pull request to 1.30.6 (nightly.yml only)", () => {
  edit(".github/workflows/nightly.yml", "ISTIO_VERSION: 1.30.5", "ISTIO_VERSION: 1.30.6");
});
