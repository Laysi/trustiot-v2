// Builds the TrustIoT gateway image (README.md, section 「從零建置」):
//
//   bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <public key file> --out <dir> [--cache <dir>]
//
// --agent      the aarch64 release build of edge/agent (README.md gives the command)
// --ssh-key    the administrator's SSH public key(s), outside the repository
// --out        new or empty directory, outside the repository: trustiot-gateway.img, its .sha256
//              and build-info.json land here
// --cache      downloads and intermediate files (default ~/.cache/trustiot-v2-image)
import { rm } from "node:fs/promises";
import { join } from "node:path";
import {
  baseMounts,
  checkAgent,
  defaultCache,
  fail,
  outputDir,
  outsideRepo,
  parseArgs,
  readPublicKeys,
  requireArm64Host,
  requireRootlessPodman,
  runContainer,
  sourceImage,
  workDir,
} from "./src/host.ts";

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2), ["agent", "ssh-key", "out", "cache"], ["agent", "ssh-key", "out"]);
  requireArm64Host();
  await requireRootlessPodman();
  const agent = await checkAgent(args.get("agent") ?? "");
  const keyFile = outsideRepo(args.get("ssh-key") ?? "", "--ssh-key");
  await readPublicKeys(keyFile);
  const cache = outsideRepo(args.get("cache") ?? defaultCache(), "--cache");
  const out = await outputDir(args.get("out") ?? "");
  const source = await sourceImage(cache);
  const work = await workDir(cache);
  let ok = false;
  try {
    const code = await runContainer(
      [
        ...baseMounts(work),
        { host: out, container: "/work/out", readOnly: false },
        { host: source, container: "/in/source.img.xz", readOnly: true },
        { host: agent, container: "/in/trustiot-agent", readOnly: true },
        { host: keyFile, container: "/in/ssh-key.pub", readOnly: true },
      ],
      "/src/image/src/inside/build.ts",
    );
    if (code !== 0) throw new Error(`the build container exited with ${code}`);
    ok = true;
    console.log(`\nimage: ${join(out, "trustiot-gateway.img")}`);
  } finally {
    await rm(work, { recursive: true, force: true });
    // A failed build leaves nothing that could be mistaken for an image.
    if (!ok) for (const name of ["trustiot-gateway.img", "trustiot-gateway.img.sha256", "build-info.json"]) await rm(join(out, name), { force: true });
  }
}

if (import.meta.main) main().catch(fail);
