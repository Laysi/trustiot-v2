// Checks a built gateway image without booting it (README.md, section 「映像檢查」):
//
//   bun edge/image/check.ts --image <trustiot-gateway.img> --agent <trustiot-agent> --ssh-key <public key file>
//                           [--report <dir>] [--manifest yes] [--cache <dir>] [--gitleaks <path>]
//
// --agent and --ssh-key are the files the image was built with; the check compares the image
// against them. --report (new or empty directory, outside the repository) receives the check
// results and the list of paths the build changed in the source image; --manifest yes adds a
// listing of every file in the root file system with its SHA-256, to compare two builds.
// Exit code: 0 when every check passed, 1 when one failed, 2 on a usage error.
import { cp, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import {
  UsageError,
  baseMounts,
  checkAgent,
  defaultCache,
  fail,
  outputDir,
  outsideRepo,
  parseArgs,
  protoBin,
  readPublicKeys,
  requireArm64Host,
  requireRootlessPodman,
  runContainer,
  sourceImage,
  workDir,
} from "./src/host.ts";

async function main(): Promise<void> {
  const args = parseArgs(
    process.argv.slice(2),
    ["image", "agent", "ssh-key", "report", "manifest", "cache", "gitleaks"],
    ["image", "agent", "ssh-key"],
  );
  requireArm64Host();
  await requireRootlessPodman();
  const image = resolve(args.get("image") ?? "");
  if (!(await Bun.file(image).exists())) throw new UsageError(`--image ${image} does not exist`);
  const agent = await checkAgent(args.get("agent") ?? "");
  const keyFile = outsideRepo(args.get("ssh-key") ?? "", "--ssh-key");
  await readPublicKeys(keyFile);
  const manifest = args.get("manifest");
  if (manifest !== undefined && manifest !== "yes") throw new UsageError("--manifest takes yes");
  const report = args.has("report") ? await outputDir(args.get("report") ?? "") : null;
  const gitleaks = args.has("gitleaks") ? resolve(args.get("gitleaks") ?? "") : await protoBin("gitleaks");
  const cache = outsideRepo(args.get("cache") ?? defaultCache(), "--cache");
  const source = await sourceImage(cache);
  const work = await workDir(cache);
  let exitCode = 0;
  try {
    const code = await runContainer(
      [
        ...baseMounts(work),
        { host: image, container: "/in/image.img", readOnly: true },
        { host: source, container: "/in/source.img.xz", readOnly: true },
        { host: agent, container: "/in/trustiot-agent", readOnly: true },
        { host: keyFile, container: "/in/ssh-key.pub", readOnly: true },
        { host: gitleaks, container: "/opt/gitleaks/gitleaks", readOnly: true },
      ],
      "/src/image/src/inside/check.ts",
      manifest === "yes" ? { TRUSTIOT_IMAGE_MANIFEST: "1" } : {},
    );
    if (report && (await Bun.file(join(work, "report/checks.json")).exists())) {
      await cp(join(work, "report"), report, { recursive: true });
      console.log(`report: ${report}`);
    }
    if (code !== 0) {
      console.error(code === 1 ? "the image failed at least one check" : `the check could not run (exit ${code})`);
      exitCode = code === 1 ? 1 : 3;
    }
  } finally {
    await rm(work, { recursive: true, force: true });
  }
  if (exitCode !== 0) process.exit(exitCode);
}

if (import.meta.main) main().catch(fail);
