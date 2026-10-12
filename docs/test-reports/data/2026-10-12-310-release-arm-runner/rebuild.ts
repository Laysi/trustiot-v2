// Runs only the Cargo step of scripts/release/build.ts for a plan, the same way build.ts does
// (rustup toolchain and target, then rust-build.ts's rustCrossCompileCommand), from /work/repo.
//   bun /work/bundle/rebuild.ts <plan.json>
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { parsePlan, RUST_TARGET } from "/work/repo/scripts/release/lib.ts";
import { protoTool, run, systemTool } from "/work/repo/scripts/release/exec.ts";
import { rustCrossCompileCommand, toolchainChannel } from "/work/repo/scripts/release/rust-build.ts";

const ROOT = "/work/repo";
const plan = parsePlan(readFileSync(process.argv[2]!, "utf8"));
const toolchain = toolchainChannel(readFileSync(join(ROOT, "rust-toolchain.toml"), "utf8"));
const rustup = systemTool("rustup");
await run([rustup, "toolchain", "install", toolchain, "--profile", "minimal", "--no-self-update"], { stream: true });
await run([rustup, "target", "add", "--toolchain", toolchain, RUST_TARGET], { stream: true });
const packages = [...new Set(plan.images.filter((image) => image.kind === "rust").map((image) => image.package ?? ""))];
const { cmd, env } = rustCrossCompileCommand({
  cargo: systemTool("cargo"),
  zig: await protoTool("zig"),
  cargoZigbuild: await protoTool("cargo-zigbuild"),
  cargoAuditable: await protoTool("cargo-auditable"),
  packages,
  root: ROOT,
  cargoHome: process.env.CARGO_HOME || join(homedir(), ".cargo"),
  path: process.env.PATH ?? "",
  version: plan.version,
  sourceDateEpoch: plan.sourceDateEpoch,
});
await run(cmd, { cwd: ROOT, stream: true, env });
