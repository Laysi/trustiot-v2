// Boots copies of a built gateway image in QEMU with a software TPM (README.md, section 「開機測試」):
//
//   bun edge/image/boot-test.ts --image <trustiot-gateway.img> [--report <dir>] [--cache <dir>]
//
// It proves the data partition's encryption, unlock and fail-closed behaviour end to end: the
// first boot with a TPM encrypts it, that TPM unlocks it afterwards, and without a TPM, with
// another TPM or with a cleared one it stays locked and unmounted, with no password prompt and the
// agent stopped. --report (new or empty directory, outside the repository) receives each boot's
// probe output, serial console log and the results. Needs an arm64 Linux host with rootless
// podman, like the build; no KVM, no privilege, no device mapper on the host.
// Exit code: 0 when every check passed, 1 when one failed, 2 on a usage error, 3 when the test
// could not run.
import { cp, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { UsageError, baseMounts, defaultCache, fail, outputDir, outsideRepo, parseArgs, requireArm64Host, requireRootlessPodman, runContainer, workDir } from "./src/host.ts";

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2), ["image", "report", "cache"], ["image"]);
  requireArm64Host();
  await requireRootlessPodman();
  const image = resolve(args.get("image") ?? "");
  if (!(await Bun.file(image).exists())) throw new UsageError(`--image ${image} does not exist`);
  const report = args.has("report") ? await outputDir(args.get("report") ?? "") : null;
  const cache = outsideRepo(args.get("cache") ?? defaultCache(), "--cache");
  const work = await workDir(cache);
  let exitCode = 0;
  try {
    const code = await runContainer([...baseMounts(work), { host: image, container: "/in/image.img", readOnly: true }], "/src/image/src/inside/boot-test.ts");
    if (report && (await Bun.file(join(work, "report/boot-test.json")).exists())) {
      await cp(join(work, "report"), report, { recursive: true });
      console.log(`report: ${report}`);
    }
    if (code !== 0) {
      console.error(code === 1 ? "the image failed at least one boot test check" : `the boot test could not run (exit ${code})`);
      exitCode = code === 1 ? 1 : 3;
    }
  } finally {
    await rm(work, { recursive: true, force: true });
  }
  if (exitCode !== 0) process.exit(exitCode);
}

if (import.meta.main) main().catch(fail);
