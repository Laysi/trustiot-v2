// The Chromium layer of the dashboard check's image for the nightly (Laysi/trustiot-v2#1127).
// deploy/images/dashboard-check/Containerfile downloads chromium-headless-shell and its libraries
// from snapshot.debian.org in its `chromium` stage and unpacks them to /rootfs, which the final
// stage copies. That stage depends on the Containerfile alone, so the nightly builds it on its
// own, as the image CHROMIUM_LAYER_IMAGE, and keeps it in an Actions cache entry of its own whose
// key covers CHROMIUM_LAYER_INPUTS (image-cache.ts, chromiumLayerCacheKey); service-image.ts
// loads it from the cache or builds it here, and then builds the dashboard check's image with
// CHROMIUM_LAYER_BUILD_CONTEXT, which replaces the `chromium` stage with this image. A run whose
// Rust images miss their cache entry then downloads nothing from snapshot.debian.org while the
// Containerfile is unchanged. Release builds and the image test (deploy/images/dashboard-check)
// do not use this: they run the `chromium` stage as it is.
//
// The layer's content is decided by the Containerfile and by this module and what it imports:
// the stage appended to the copy, the build options, and the build guard. Those are the cache
// key's inputs; who calls this, and when, decides only whether the layer is built or loaded.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { buildFileStages, containerBuildArgs } from "../lib/build-context.ts";
import { IMAGE_PLATFORM } from "../release/lib.ts";
import { retry, run } from "./exec.ts";
import type { RunLog } from "./exec.ts";

/** The image whose `chromium` stage this layer stands in for (deploy/images/<name>). */
export const CHROMIUM_LAYER_OF = "dashboard-check";
/** Its Containerfile, relative to the repository root: release.yml builds the same file. */
export const DASHBOARD_CHECK_CONTAINERFILE = `deploy/images/${CHROMIUM_LAYER_OF}/Containerfile`;
/** The stage of that Containerfile that downloads and unpacks Chromium into /rootfs. */
export const CHROMIUM_STAGE = "chromium";
/** The stage appended to the copy: only /rootfs of the `chromium` stage, at the same path. */
export const CHROMIUM_LAYER_STAGE = "nightly-chromium-rootfs";
/** The local tag of the layer; build-context.ts accepts it as a tag and as a named context. */
export const CHROMIUM_LAYER_IMAGE = "localhost/trustiot-e2e/dashboard-check-chromium:nightly";
/** The tarball's name in the cache directory (`<name>.tar`, image-cache.ts imageCacheFile). */
export const CHROMIUM_LAYER_CACHE_NAME = "dashboard-check-chromium";

/**
 * Appended to the copy of the Containerfile. The final stage of the dashboard check's image runs
 * `COPY --from=chromium /rootfs/ /`, so the layer keeps /rootfs/ where the stage had it.
 */
export const CHROMIUM_LAYER_STAGE_TEXT = `\nFROM scratch AS ${CHROMIUM_LAYER_STAGE}\nCOPY --from=${CHROMIUM_STAGE} /rootfs/ /rootfs/\n`;

/**
 * The build option that makes the dashboard check's build read the layer instead of running its
 * `chromium` stage. A named context "overrides a stage called `name` when used as
 * `FROM ... AS name`" (https://docs.docker.com/reference/cli/docker/buildx/build/#build-context),
 * so that stage, its base image and its apt download do not run at all. build-context.ts accepts
 * this shape only.
 */
export const CHROMIUM_LAYER_BUILD_CONTEXT: readonly string[] = ["--build-context", `${CHROMIUM_STAGE}=docker-image://${CHROMIUM_LAYER_IMAGE}`];

/**
 * What decides the layer's content, relative to the repository root: the Containerfile, this
 * module, and every module it imports, followed to the end (chromium-layer.test.ts checks that
 * this covers them). Left out, because they cannot change what is in the layer:
 * - nightly.yml and the setup-proto action: they only decide when the entry is restored and saved
 *   and install Bun, which runs this module; the build runs the runner's docker, which the Rust
 *   images' key does not cover either;
 * - .dockerignore: the context is a fresh directory that holds the copy of the Containerfile only;
 * - image-cache.ts and service-image.ts: they load or save the tarball, or call the build; a
 *   tarball that cannot be loaded or is not for linux/arm64 is built again (image-cache.ts
 *   loadCachedImage). Keeping them out spares a download each time a Rust image is added.
 */
export const CHROMIUM_LAYER_INPUTS: readonly string[] = [
  DASHBOARD_CHECK_CONTAINERFILE,
  "scripts/nightly/chromium-layer.ts",
  "scripts/nightly/exec.ts",
  "scripts/release/lib.ts",
  "scripts/lib/build-context.ts",
  "scripts/ci/check-secret-handling.ts",
  "scripts/lib/stack.ts",
  "scripts/lib/state.ts",
  "scripts/lib/cleanup.ts",
  "scripts/lib/secrets.ts",
];

/**
 * A bound for building the layer: apt downloads Chromium and its libraries from
 * snapshot.debian.org, which is slow at times (deploy/images/dashboard-check/README.md).
 */
const LAYER_BUILD_TIMEOUT_MS = 20 * 60_000;
const RETRY_PAUSE_MS = 10_000;

/**
 * Writes the layer's build context to the empty directory `dir`: a copy of the dashboard check's
 * Containerfile with CHROMIUM_LAYER_STAGE_TEXT appended, and nothing else (the stages the layer
 * needs copy nothing from the context). Returns the Containerfile's path.
 */
export function writeChromiumLayerContext(repoRoot: string, dir: string): string {
  const text = readFileSync(join(repoRoot, DASHBOARD_CHECK_CONTAINERFILE), "utf8");
  const stages = buildFileStages(text);
  if (!stages.includes(CHROMIUM_STAGE)) {
    throw new Error(`${DASHBOARD_CHECK_CONTAINERFILE} has no stage named ${CHROMIUM_STAGE} (stages: ${stages.join(", ") || "none"}), which the nightly's Chromium layer replaces`);
  }
  if (stages.includes(CHROMIUM_LAYER_STAGE)) {
    throw new Error(`${DASHBOARD_CHECK_CONTAINERFILE} already has a stage named ${CHROMIUM_LAYER_STAGE}, the stage the nightly appends`);
  }
  const containerfile = join(dir, "Containerfile");
  writeFileSync(containerfile, `${text.endsWith("\n") ? text : `${text}\n`}${CHROMIUM_LAYER_STAGE_TEXT}`);
  return containerfile;
}

/**
 * Builds the layer from snapshot.debian.org as CHROMIUM_LAYER_IMAGE (linux/arm64) with docker,
 * whose local images the dashboard check's build then reads, and checks that docker holds it for
 * that platform. The context is a throwaway directory, removed afterwards.
 */
export async function buildChromiumLayer(
  log: RunLog,
  repoRoot: string,
  commands: { run: typeof run } = { run },
  { pauseMs = RETRY_PAUSE_MS }: { pauseMs?: number } = {},
): Promise<string> {
  log.line(`== Chromium layer ${CHROMIUM_LAYER_IMAGE}: stage ${CHROMIUM_STAGE} of ${DASHBOARD_CHECK_CONTAINERFILE}, from snapshot.debian.org`);
  const dir = mkdtempSync(join(tmpdir(), "nightly-chromium-layer-"));
  try {
    const containerfile = writeChromiumLayerContext(repoRoot, dir);
    // snapshot.debian.org fails at times (503); apt retries each download itself.
    await retry(log, 2, pauseMs, () =>
      commands.run(
        log,
        // containerBuildArgs checks the context and the Containerfile first (docs/architecture/secrets.md).
        [
          "docker",
          ...containerBuildArgs(dir, [
            "--platform",
            IMAGE_PLATFORM,
            "--target",
            CHROMIUM_LAYER_STAGE,
            "--file",
            containerfile,
            "--tag",
            CHROMIUM_LAYER_IMAGE,
          ]),
        ],
        { timeoutMs: LAYER_BUILD_TIMEOUT_MS },
      ),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  const inspected = await commands.run(log, ["docker", "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", CHROMIUM_LAYER_IMAGE], { timeoutMs: 60_000 });
  const platform = inspected.stdout.trim();
  if (platform !== IMAGE_PLATFORM) {
    throw new Error(`docker built ${CHROMIUM_LAYER_IMAGE} for ${platform}, not ${IMAGE_PLATFORM}`);
  }
  log.line(`built ${CHROMIUM_LAYER_IMAGE}`);
  return CHROMIUM_LAYER_IMAGE;
}
