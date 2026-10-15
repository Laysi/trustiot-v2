/**
 * The packing half of release stage 2 (build.ts): builds every planned image for linux/arm64
 * from the Rust binaries that build.ts compiled (or from a Containerfile directory), pushes it
 * into an OCI archive, writes its SBOM, then build.json. Kept apart from build.ts so that a
 * reproduction can pack the binaries of a published release with exactly this code
 * (docs/test-reports/data/2026-10-15-384-rootless-independent-digest/pack-binaries.ts).
 *
 * A Rust image's digest does not depend on whether podman runs as root or rootless
 * (Laysi/trustiot-v2#384; docs/architecture/ci-release.md, "交叉編譯與可重現建置"). With the
 * runner's podman 4.9.3 and the overlay driver it did: rootless overlay records its copy-up
 * of `/usr` in a `user.overlay.origin` xattr, which podman 4.9.3 writes into the COPY layer
 * (root overlay uses `trusted.overlay.*`, never exported), and that layer's ID changes the
 * intermediate image IDs that the history names. vfs keeps no such state, so the Rust images
 * are built in a store of this run's own with the vfs driver: a store records its driver, so
 * the user's own store cannot switch, and a fresh store also means nothing cached from an
 * earlier build. With vfs, rootless podman 4.9.3 packs the image that root podman always did.
 * Containerfile images (with RUN steps, where vfs copies every layer in full) use the user's
 * store as before. The build context is made here by the user that runs podman, so podman sees
 * its files as 0:0 either way, which keeps the history's `COPY file:<hash>` the same.
 *
 * Output:
 *   <out>/images/<name>.oci.tar     the image as an OCI archive, pushed byte for byte by publish
 *   <out>/images/<name>.spdx.json   its SBOM from syft
 *   <out>/build.json                digest and size of each image, for the log, and the podman
 *                                   and buildah versions that packed them, for the release page
 */

import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { containerBuildArgs } from "../lib/build-context.ts";
import { log, protoTool, rawManifest, run, systemTool, tryRun, warn } from "./exec.ts";
import {
  IMAGE_PLATFORM,
  type PlannedImage,
  RUST_CONTAINERFILE,
  BUILDER_PACKAGES,
  type ReleaseBuilder,
  type ReleasePlan,
  ReleaseError,
  errorMessage,
  imagePlatformProblems,
  parseBuilder,
  parseDpkgVersions,
  renderRustContainerfile,
  summarizeSpdx,
} from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

/** The storage driver of the store the Rust images are built in. */
export const RUST_IMAGE_DRIVER = "vfs";

export interface ImageStore {
  /** The directory that holds the store and its configuration; removed after the build. */
  dir: string;
  driver: string;
  /** The graph root, where podman must report the store to be. */
  graphRoot: string;
  /** The storage.conf that podman reads through CONTAINERS_STORAGE_CONF, and its text. */
  conf: string;
  confText: string;
  /** What every podman command that uses this store adds to its environment. */
  env: Record<string, string>;
}

/** Paths that storage.conf can hold without quoting. */
const PLAIN_PATH = /^\/[A-Za-z0-9._/+-]+$/;

/**
 * A store under `dir` (absolute) with `driver`. Its storage.conf replaces the system's and the
 * user's entirely, so no other storage setting of the machine (an additional image store, a
 * mount program) changes the build. A rootless user's store is `rootless_storage_path`:
 * podman 4.9.3 ignores `graphroot` for one, and then keeps its database in the user's
 * default store, whose recorded driver wins over this one.
 */
export function imageStore(dir: string, driver: string): ImageStore {
  if (!isAbsolute(dir) || !PLAIN_PATH.test(dir)) {
    throw new ReleaseError(`the image store needs an absolute path of plain characters, not ${dir}`);
  }
  if (!/^[a-z0-9]+$/.test(driver)) {
    throw new ReleaseError(`${driver} is not a storage driver name`);
  }
  const graphRoot = join(dir, "graph");
  const conf = join(dir, "storage.conf");
  const confText = [
    "[storage]",
    `driver = "${driver}"`,
    `graphroot = "${graphRoot}"`,
    `rootless_storage_path = "${graphRoot}"`,
    `runroot = "${join(dir, "run")}"`,
    "",
  ].join("\n");
  return { dir, driver, graphRoot, conf, confText, env: { CONTAINERS_STORAGE_CONF: conf } };
}

/** What `podman info --format json` says about the store it uses and its own version. */
interface PodmanInfo {
  store?: { graphRoot?: unknown; graphDriverName?: unknown };
  version?: { Version?: unknown };
  host?: { buildahVersion?: unknown };
}

/** Throws unless podman reports `store` as the store it uses, with its driver. */
export function requireStore(info: PodmanInfo, store: ImageStore): void {
  const graphRoot = info.store?.graphRoot;
  const driver = info.store?.graphDriverName;
  if (graphRoot !== store.graphRoot || driver !== store.driver) {
    throw new ReleaseError(
      `podman uses the store ${String(graphRoot)} with the ${String(driver)} driver, not ${store.graphRoot} with ${store.driver}; ` +
        "is podman's database configured elsewhere (containers.conf static_dir)?",
    );
  }
}

interface BuiltImage {
  name: string;
  digest: string;
  size: number;
  archive: string;
  sbom: string;
  sbomPackages: number;
}

async function buildImage(
  plan: ReleasePlan,
  image: PlannedImage,
  binary: string | undefined,
  outDir: string,
  workDir: string,
  rustStore: ImageStore,
): Promise<BuiltImage> {
  const podman = systemTool("podman");
  const skopeo = systemTool("skopeo");
  const syft = await protoTool("syft");
  const localTag = `localhost/trustiot-release/${image.name}:${plan.version}`;

  let context: string;
  let containerfile: string;
  // The Rust images' own vfs store; Containerfile images use the user's store.
  const env = image.kind === "rust" ? rustStore.env : {};
  if (image.kind === "rust") {
    if (binary === undefined) {
      throw new ReleaseError(`no binary was built for ${image.name}`);
    }
    // The build context holds only the binary and the rendered Containerfile.
    context = join(workDir, image.name);
    mkdirSync(context, { recursive: true });
    const bin = binary.split("/").at(-1) ?? "";
    copyFileSync(binary, join(context, bin));
    chmodSync(join(context, bin), 0o755);
    containerfile = join(context, "Containerfile");
    writeFileSync(containerfile, renderRustContainerfile(readFileSync(join(ROOT, RUST_CONTAINERFILE), "utf8"), bin));
  } else {
    context = join(ROOT, image.context ?? "");
    containerfile = join(context, "Containerfile");
  }

  const labels: Record<string, string> = {
    "org.opencontainers.image.source": `https://github.com/${plan.repository}`,
    "org.opencontainers.image.revision": plan.commit,
    "org.opencontainers.image.version": plan.version,
    "org.opencontainers.image.title": image.name,
  };
  log("build", `building ${image.ref} for ${IMAGE_PLATFORM}`);
  // containerBuildArgs checks the context and the Containerfile for key and credential files
  // first, and allows only known options (docs/architecture/secrets.md).
  await run(
    [
      podman,
      ...containerBuildArgs(context, [
        "--platform",
        IMAGE_PLATFORM,
        // Fixed timestamps (the commit time) make a rebuild of the same commit byte-identical.
        "--timestamp",
        String(plan.sourceDateEpoch),
        "--pull=missing",
        "--format",
        "oci",
        ...Object.entries(labels).flatMap(([key, value]) => ["--label", `${key}=${value}`]),
        "--file",
        containerfile,
        "--tag",
        localTag,
      ]),
    ],
    { stream: true, env },
  );

  const imagesDir = join(outDir, "images");
  mkdirSync(imagesDir, { recursive: true });
  const archive = join(imagesDir, `${image.name}.oci.tar`);
  rmSync(archive, { force: true });
  await run([podman, "push", "--format", "oci", "--compression-format", "gzip", localTag, `oci-archive:${archive}`], { env });
  await tryRun([podman, "image", "rm", localTag], { env });

  const { manifest, digest } = await rawManifest(skopeo, `oci-archive:${archive}`);
  const config = JSON.parse((await run([skopeo, "inspect", "--config", "--raw", `oci-archive:${archive}`])).stdout);
  const problems = imagePlatformProblems(manifest, config);
  if (problems.length > 0) {
    throw new ReleaseError(`${image.name}: ${problems.join("; ")}`);
  }

  const sbom = join(imagesDir, `${image.name}.spdx.json`);
  await run(
    [syft, "scan", `oci-archive:${archive}`, "--output", `spdx-json=${sbom}`, "--source-name", image.repository, "--source-version", plan.version],
    {
      env: {
        SYFT_CHECK_FOR_APP_UPDATE: "false",
        // Packages, their dependencies and the files they were found in. Every file a package
        // owns (about a thousand for tzdata alone) would make each SBOM attestation hundreds
        // of kilobytes without naming one more package.
        SYFT_FILE_METADATA_SELECTION: "none",
        SYFT_RELATIONSHIPS_PACKAGE_FILE_OWNERSHIP: "false",
      },
    },
  );
  const summary = summarizeSpdx(JSON.parse(readFileSync(sbom, "utf8")));
  // A Rust image's SBOM must list its crates, which only cargo-auditable data provides.
  if (image.kind === "rust" && !summary.packages.some((pkg) => pkg.startsWith(`${image.package}@`))) {
    throw new ReleaseError(`${image.name}: the SBOM does not list the Rust package ${image.package}; was the binary built with cargo-auditable?`);
  }
  log("build", `${image.ref} → ${digest}, SBOM ${summary.spdxVersion} with ${summary.packages.length} packages`);
  return { name: image.name, digest, size: statSync(archive).size, archive, sbom, sbomPackages: summary.packages.length };
}

/**
 * Checks that podman uses the Rust images' store, and returns the podman and buildah that pack
 * the images, with their packages when dpkg knows them.
 */
async function findBuilder(podman: string, store: ImageStore): Promise<ReleaseBuilder> {
  const info = JSON.parse((await run([podman, "info", "--format", "json"], { env: store.env })).stdout) as PodmanInfo;
  requireStore(info, store);
  const text = (value: unknown) => (typeof value === "string" && value !== "" ? value : "unknown");
  const dpkg = Bun.which("dpkg-query");
  // dpkg-query exits non-zero when one of the packages is not installed, and still lists the others.
  const packages =
    dpkg === null ? {} : parseDpkgVersions((await tryRun([dpkg, "-W", "-f", "${Package} ${Version}\n", ...BUILDER_PACKAGES])).stdout);
  return parseBuilder({ podman: text(info.version?.Version), buildah: text(info.host?.buildahVersion), packages });
}

/**
 * Removes the work directory with the Rust images' store in it. Rootless podman created the
 * store's files with the subordinate IDs of its user namespace, so only a command inside that
 * namespace can remove them all. A failure is logged, not thrown, so that it never hides why
 * the build failed; the directory is under the system's temporary directory.
 */
async function removeWorkDir(podman: string, workDir: string, store: ImageStore): Promise<void> {
  if (existsSync(store.dir) && process.getuid?.() !== 0) {
    const result = await tryRun([podman, "unshare", "rm", "-rf", "--", store.dir], { env: store.env });
    if (result.exitCode !== 0) {
      warn("build", `podman unshare could not remove the image store ${store.dir}: ${result.stderr.trim()}`);
    }
  }
  try {
    rmSync(workDir, { recursive: true, force: true });
  } catch (error) {
    warn("build", `could not remove ${workDir}: ${errorMessage(error)}`);
  }
}

/**
 * Packs every planned image into `outDir` (see the file comment); `binaries` maps each Rust
 * image to its aarch64 binary. Writes build.json last.
 */
export async function packImages(plan: ReleasePlan, binaries: ReadonlyMap<string, string>, outDir: string): Promise<void> {
  const workDir = mkdtempSync(join(tmpdir(), "trustiot-release-"));
  const podman = systemTool("podman");
  const rustStore = imageStore(join(workDir, "store"), RUST_IMAGE_DRIVER);
  mkdirSync(rustStore.dir);
  writeFileSync(rustStore.conf, rustStore.confText);
  try {
    const builder = await findBuilder(podman, rustStore);
    const packages = Object.entries(builder.packages).map(([name, version]) => `${name} ${version}`);
    log("build", `packing with podman ${builder.podman}, buildah ${builder.buildah}${packages.length > 0 ? `; packages: ${packages.join(", ")}` : ""}`);
    const built: BuiltImage[] = [];
    for (const image of plan.images) {
      built.push(await buildImage(plan, image, binaries.get(image.name), outDir, workDir, rustStore));
    }
    const images = built.map(({ archive, sbom, ...rest }) => ({ ...rest, archive: archive.slice(outDir.length + 1), sbom: sbom.slice(outDir.length + 1) }));
    writeFileSync(join(outDir, "build.json"), `${JSON.stringify({ tag: plan.tag, commit: plan.commit, builder, images }, null, 2)}\n`);
  } finally {
    await removeWorkDir(podman, workDir, rustStore);
  }
}
