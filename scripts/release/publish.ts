/**
 * Release stage 3, the `publish` job of .github/workflows/release.yml (in the `release`
 * environment): pushes the images that build produced, signs them and attests their SBOMs,
 * packages, pushes and signs the chart, and creates the GitHub Release.
 *
 *   bun scripts/release/publish.ts --plan <plan.json> --artifacts <dir> --notes <notes.md>
 *     [--output <published.json>] [--local-test-key <prefix>]
 *
 * It runs no build code. The image archives are pushed byte for byte (skopeo
 * --preserve-digests) and only their digests are signed; the chart is packaged here from a copy
 * of the repository's chart, not taken from the build job, with the digests of this release's
 * images written into its values.yaml (chart-images.ts), so the signed chart pins the images it
 * deploys. Versions are immutable: a tag that already points at different content fails the
 * run, while a re-run that produces the same digests (images and chart are built reproducibly)
 * goes on and signs again.
 *
 * Environment in GitHub Actions: GITHUB_TOKEN (packages: write, contents: write), GITHUB_ACTOR,
 * GITHUB_REPOSITORY; cosign takes its signing certificate from the job's OIDC token
 * (id-token: write). See registry.ts for --local-test-key.
 */

import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { type ChartPin, type PinOptions, chartPins, packagePinnedChart, parseValues, pinnedChartValues, pinnedReference } from "./chart-images.ts";
import { log, parseOptions, protoTool, rawManifest, requireCheckout, run, runStage, setOutput, systemTool, tryRun } from "./exec.ts";
import {
  CHART_DIR,
  type PlannedImage,
  type PublishedArtifact,
  type PublishedRelease,
  type ReleasePlan,
  ReleaseError,
  helmPushDigest,
  imagePlatformProblems,
  isMap,
  parsePlan,
  renderReleaseBody,
  sbomAssetName,
  sha256Digest,
  summarizeSpdx,
} from "./lib.ts";
import {
  type RegistryAccess,
  cosignSigningFlags,
  login,
  registryAccess,
  registryHost,
  registryManifest,
  requirePlanRegistry,
  skopeoTls,
} from "./registry.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

interface Tools {
  skopeo: string;
  cosign: string;
  helm: string;
}

/** The image archive and SBOM that build wrote for `image`. */
function buildArtifacts(image: PlannedImage, artifacts: string): { archive: string; sbomFile: string } {
  const archive = join(artifacts, "images", `${image.name}.oci.tar`);
  const sbomFile = join(artifacts, "images", `${image.name}.spdx.json`);
  for (const file of [archive, sbomFile]) {
    if (!existsSync(file)) {
      throw new ReleaseError(`the build artifacts have no ${file}`);
    }
  }
  return { archive, sbomFile };
}

function pinOptions(plan: ReleasePlan, access: RegistryAccess): PinOptions {
  // A local test also points the chart at its throwaway registry; a release refuses a chart
  // whose repositories are not where it pushes.
  return { appVersion: plan.version, rewriteRepositories: access.localKey !== undefined };
}

/** The chart's pins for these images; throws unless the chart and its shape files take them. */
function chartPinsFor(plan: ReleasePlan, images: readonly PublishedArtifact[], access: RegistryAccess): ChartPin[] {
  const chartDir = join(ROOT, CHART_DIR);
  const values = parseValues(readFileSync(join(chartDir, "values.yaml"), "utf8"), `${CHART_DIR}/values.yaml`);
  const pins = chartPins(plan.images, images, values);
  pinnedChartValues(chartDir, pins, pinOptions(plan, access));
  return pins;
}

async function publishImage(
  plan: ReleasePlan,
  image: PlannedImage,
  artifacts: string,
  access: RegistryAccess,
  tools: Tools,
): Promise<PublishedArtifact & { sbomAsset: string; sbomFile: string }> {
  const { archive, sbomFile } = buildArtifacts(image, artifacts);
  const { manifest, digest } = await rawManifest(tools.skopeo, `oci-archive:${archive}`);
  const config = JSON.parse((await run([tools.skopeo, "inspect", "--config", "--raw", `oci-archive:${archive}`])).stdout);
  const problems = imagePlatformProblems(manifest, config);
  if (problems.length > 0) {
    throw new ReleaseError(`${image.name}: ${problems.join("; ")}`);
  }
  summarizeSpdx(JSON.parse(readFileSync(sbomFile, "utf8")));

  const existing = await registryManifest(access, image.ref);
  if (existing !== undefined && existing.digest !== digest) {
    throw new ReleaseError(
      `${image.ref} already exists with ${existing.digest}, not ${digest}: published versions are ` +
        "immutable, so tag a new version",
    );
  }
  if (existing === undefined) {
    log("publish", `pushing ${image.ref}`);
    await run([
      tools.skopeo,
      "copy",
      "--preserve-digests",
      "--retry-times",
      "3",
      ...skopeoTls(access, "dest"),
      `oci-archive:${archive}`,
      `docker://${image.ref}`,
    ]);
  } else {
    log("publish", `${image.ref} is already ${digest}; not pushing it again`);
  }
  const pushed = await registryManifest(access, image.ref);
  if (pushed?.digest !== digest) {
    throw new ReleaseError(`${image.ref} points at ${pushed?.digest ?? "nothing"} after the push, not ${digest}`);
  }

  const byDigest = `${image.repository}@${digest}`;
  await run([tools.cosign, "sign", ...cosignSigningFlags(access), byDigest]);
  await run([tools.cosign, "attest", ...cosignSigningFlags(access), "--type", "spdxjson", "--predicate", sbomFile, byDigest]);
  log("publish", `${image.ref} → ${digest}: signed, SBOM attested`);
  return { name: image.name, ref: image.ref, digest, sbomAsset: sbomAssetName(image.name, plan.version), sbomFile };
}

async function publishChart(
  plan: ReleasePlan,
  access: RegistryAccess,
  tools: Tools,
  workDir: string,
  images: readonly PublishedArtifact[],
): Promise<PublishedArtifact> {
  // The images this chart deploys, pinned to the digests just pushed.
  const pins = chartPinsFor(plan, images, access);
  for (const pin of pins) {
    log("publish", `the chart pins ${pin.image.name} to ${pinnedReference(pin, plan.version)}`);
  }
  for (const image of images.filter((candidate) => !pins.some((pin) => pin.image.name === candidate.name))) {
    log("publish", `the chart does not deploy ${image.name}`);
  }
  // SOURCE_DATE_EPOCH fixes the timestamps in the package; helm push takes the manifest's
  // created annotation from the file's mtime, in the local time zone. Together they make the
  // chart's digest the same on every run for the same commit and images.
  const tgz = await packagePinnedChart(pins, {
    helm: tools.helm,
    chartDir: join(ROOT, CHART_DIR),
    workDir: join(workDir, "chart"),
    sourceDateEpoch: plan.sourceDateEpoch,
    ...pinOptions(plan, access),
  });
  if (!tgz.endsWith(`/${plan.chart.name}-${plan.version}.tgz`)) {
    throw new ReleaseError(`helm package wrote ${tgz}, not the planned chart ${plan.chart.name}`);
  }
  utimesSync(tgz, plan.sourceDateEpoch, plan.sourceDateEpoch);
  const content = sha256Digest(new Uint8Array(readFileSync(tgz)));
  const ref = `${plan.chart.repository}:${plan.version}`;

  let digest: string;
  const existing = await registryManifest(access, ref);
  if (existing !== undefined) {
    const layers = isMap(existing.manifest) && Array.isArray(existing.manifest.layers) ? existing.manifest.layers : [];
    const layer: unknown = layers[0];
    if (layers.length !== 1 || !isMap(layer) || layer.digest !== content) {
      throw new ReleaseError(`${ref} already exists with different content: published versions are immutable, so tag a new version`);
    }
    log("publish", `${ref} already holds this chart (${existing.digest}); not pushing it again`);
    digest = existing.digest;
  } else {
    const output = await run(
      [tools.helm, "push", tgz, `oci://${dirname(plan.chart.repository)}`, ...(access.localKey === undefined ? [] : ["--plain-http"])],
      { env: { TZ: "UTC" } },
    );
    digest = helmPushDigest(`${output.stdout}\n${output.stderr}`);
  }
  const pushed = await registryManifest(access, ref);
  if (pushed?.digest !== digest) {
    throw new ReleaseError(`${ref} points at ${pushed?.digest ?? "nothing"} after the push, not ${digest}`);
  }
  await run([tools.cosign, "sign", ...cosignSigningFlags(access), `${plan.chart.repository}@${digest}`]);
  log("publish", `${ref} → ${digest}: signed`);
  return { name: plan.chart.name, ref, digest };
}

/** Creates the GitHub Release, or updates it when a re-run finds it; returns its URL. */
async function githubRelease(plan: ReleasePlan, body: string, sboms: { asset: string; file: string }[], workDir: string): Promise<string> {
  const gh = systemTool("gh");
  const repository = process.env.GITHUB_REPOSITORY;
  if (!repository || !process.env.GITHUB_TOKEN) {
    throw new ReleaseError("GITHUB_REPOSITORY and GITHUB_TOKEN must be set to create the GitHub Release");
  }
  const env = { GH_TOKEN: process.env.GITHUB_TOKEN, GH_REPO: repository };
  const bodyFile = join(workDir, "release-body.md");
  writeFileSync(bodyFile, body);
  const assetsDir = join(workDir, "assets");
  mkdirSync(assetsDir, { recursive: true });
  const assets = sboms.map(({ asset, file }) => {
    const path = join(assetsDir, asset);
    copyFileSync(file, path);
    return path;
  });
  // A prerelease never becomes "latest". For a final release GitHub decides by version and date,
  // so a patch release of an older line does not take "latest" from a newer one.
  const flags = [
    "--title",
    plan.tag,
    "--notes-file",
    bodyFile,
    ...(plan.prerelease ? ["--prerelease", "--latest=false"] : ["--prerelease=false"]),
  ];
  const exists = (await tryRun([gh, "release", "view", plan.tag, "--json", "url"], { env })).exitCode === 0;
  if (exists) {
    log("publish", `updating the existing GitHub Release ${plan.tag}`);
    await run([gh, "release", "edit", plan.tag, ...flags], { env });
    await run([gh, "release", "upload", plan.tag, "--clobber", ...assets], { env });
  } else {
    await run([gh, "release", "create", plan.tag, "--verify-tag", ...flags, ...assets], { env });
  }
  return (await run([gh, "release", "view", plan.tag, "--json", "url", "--jq", ".url"], { env })).stdout.trim();
}

await runStage("publish", async () => {
  const options = parseOptions(Bun.argv.slice(2), {
    plan: {},
    artifacts: {},
    notes: {},
    output: { default: "" },
    "local-test-key": { default: "" },
  });
  const plan = parsePlan(readFileSync(options.plan ?? "", "utf8"));
  const access = registryAccess(options["local-test-key"] ?? "");
  requirePlanRegistry(access, plan);
  // The chart is packaged from this checkout.
  await requireCheckout(plan.commit);
  const notes = readFileSync(options.notes ?? "", "utf8");
  const tools: Tools = { skopeo: systemTool("skopeo"), cosign: await protoTool("cosign"), helm: await protoTool("helm") };
  const hosts = new Set([...plan.images.map((image) => registryHost(image.ref)), registryHost(plan.chart.repository)]);
  for (const host of hosts) {
    await login(access, host, tools);
  }

  const workDir = mkdtempSync(join(tmpdir(), "trustiot-publish-"));
  try {
    // Before anything is pushed: the chart must take this build's digests, so a chart that
    // cannot be pinned stops the release while nothing is published yet.
    const built: PublishedArtifact[] = [];
    for (const image of plan.images) {
      const { digest } = await rawManifest(tools.skopeo, `oci-archive:${buildArtifacts(image, resolve(options.artifacts ?? "")).archive}`);
      built.push({ name: image.name, ref: image.ref, digest });
    }
    chartPinsFor(plan, built, access);
    const images = [];
    for (const image of plan.images) {
      images.push(await publishImage(plan, image, resolve(options.artifacts ?? ""), access, tools));
    }
    const chart = await publishChart(plan, access, tools, workDir, images);
    const published = { images: images.map(({ sbomFile: _, ...image }) => image), chart };
    const body = renderReleaseBody(notes, plan, published);
    let releaseUrl: string;
    if (access.localKey === undefined) {
      releaseUrl = await githubRelease(plan, body, images.map((image) => ({ asset: image.sbomAsset, file: image.sbomFile })), workDir);
    } else {
      releaseUrl = "";
      const local = join(dirname(resolve(options.plan ?? "")), "release-body.md");
      writeFileSync(local, body);
      log("publish", `local test: no GitHub Release; its body is in ${local}`);
    }
    const result: PublishedRelease = { ...published, releaseUrl };
    const json = JSON.stringify(result);
    if (options.output) {
      writeFileSync(options.output, `${JSON.stringify(result, null, 2)}\n`);
    }
    setOutput("published", json);
    setOutput("release-url", releaseUrl);
    log("publish", `released ${plan.tag}${releaseUrl ? `: ${releaseUrl}` : ""}`);
  } finally {
    rmSync(workDir, { recursive: true, force: true });
  }
});
