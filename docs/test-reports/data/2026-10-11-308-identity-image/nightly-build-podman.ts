// Runs the nightly's buildServiceImage on a workstation with podman in place of docker
// (Laysi/trustiot-v2#308 evidence); the k3d import is not run here. build-all.sh calls it as
// `bun nightly-build-podman.ts <repo root> <log>`. Saved with the report; CI never runs it.
// (The run of 2026-10-11 imported the same modules by the worktree's absolute path.)
import { RunLog } from "../../../../scripts/nightly/exec.ts";
import { buildServiceImage, findServiceImageTools } from "../../../../scripts/nightly/service-image.ts";

const [root, logPath] = Bun.argv.slice(2);
if (root === undefined || logPath === undefined) throw new Error("usage: nightly-build-podman.ts <repo root> <log>");
const tools = { ...(await findServiceImageTools(root)), containerTool: "podman" };
const image = await buildServiceImage({ repoRoot: root, log: new RunLog(logPath) }, "identity", tools);
console.log(`nightly helper built ${image}`);
