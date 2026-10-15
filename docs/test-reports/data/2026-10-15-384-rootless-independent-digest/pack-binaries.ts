// Laysi/trustiot-v2#384: packs Rust binaries that were built elsewhere (for example on the arm64
// runner, or in an emulated arm64 container) into their images with build.ts's own packing code
// (scripts/release/pack.ts), so that the result can be compared with a release's digests.
//   bun pack-binaries.ts --plan <plan.json> --binaries <dir with one file per Rust image> --out <dir>
// Run it from the checkout of the plan's commit: pack.ts reads rust-binary.Containerfile there.
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { parseOptions, requireCheckout, runStage } from "../../../../scripts/release/exec.ts";
import { ReleaseError, aarch64ElfProblem, parsePlan } from "../../../../scripts/release/lib.ts";
import { packImages } from "../../../../scripts/release/pack.ts";

await runStage("pack-binaries", async () => {
  const options = parseOptions(Bun.argv.slice(2), { plan: {}, binaries: {}, out: {} });
  const plan = parsePlan(readFileSync(options.plan ?? "", "utf8"));
  await requireCheckout(plan.commit);
  const binaries = new Map<string, string>();
  for (const image of plan.images.filter((planned) => planned.kind === "rust")) {
    const path = join(resolve(options.binaries ?? ""), image.package ?? "");
    const problem = aarch64ElfProblem(new Uint8Array(readFileSync(path)));
    if (problem !== undefined) {
      throw new ReleaseError(`${path} is not an aarch64 binary: ${problem}`);
    }
    binaries.set(image.name, path);
  }
  await packImages(plan, binaries, resolve(options.out ?? ""));
});
