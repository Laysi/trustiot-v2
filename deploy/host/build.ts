// Builds the installer into one self-contained executable per host architecture, plus a
// SHA256SUMS file to check the copies on the host. Run: bun run --cwd deploy/host build
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const HERE = import.meta.dir;
const DIST = join(HERE, "dist");
// The x64 baseline build runs on CPUs without AVX2 as well.
const TARGETS = [
  { target: "bun-linux-x64-baseline", name: "trustiot-host-install-x86_64" },
  { target: "bun-linux-arm64", name: "trustiot-host-install-aarch64" },
] as const;

rmSync(DIST, { recursive: true, force: true });
mkdirSync(DIST);
const sums: string[] = [];
for (const { target, name } of TARGETS) {
  const outfile = join(DIST, name);
  const build = Bun.spawnSync({
    cmd: [process.execPath, "build", "--compile", "--minify", `--target=${target}`, "trustiot-host-install.ts", "--outfile", outfile],
    cwd: HERE,
    stdout: "inherit",
    stderr: "inherit",
  });
  if (build.exitCode !== 0) {
    console.error(`build for ${target} failed`);
    process.exit(1);
  }
  sums.push(`${createHash("sha256").update(readFileSync(outfile)).digest("hex")}  ${name}`);
}
writeFileSync(join(DIST, "SHA256SUMS"), `${sums.join("\n")}\n`);
console.log(sums.join("\n"));
