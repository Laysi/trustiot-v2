/**
 * Release stage 4, the `verify` job of .github/workflows/release.yml: checks the published
 * release the way a consumer would, with read-only access, and writes the result as Markdown
 * to the step output `report`, which the workflow's report job appends to the GitHub Release.
 *
 *   bun scripts/release/verify.ts --plan <plan.json> --published <published.json>
 *     [--output <report.md>] [--local-test-key <prefix>]
 *
 * Every image: its tag points at the published digest; the manifest is one linux/arm64 image;
 * the cosign signature verifies against the release identity (lib.ts keylessVerifyArgs); the
 * SBOM attestation verifies and holds an SPDX document that lists packages; an anonymous pull
 * is refused, so the package is private. The chart: tag, signature, Chart.yaml version and
 * appVersion, and its image references: every image of this release that the chart deploys
 * must render as `<repository>:<version>@<published digest>` from the chart's values.yaml alone
 * and layered with each shipped shape file (chart-images.ts). Once per run, a signature check
 * with another identity must fail, so the checks above are known not to pass vacuously.
 */

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type ChartPin, chartPinProblems, chartPins, pinnedReference, readChartValues, SHAPE_VALUES_FILES, unpackChart } from "./chart-images.ts";
import { log, parseOptions, protoTool, run, runStage, setOutput, systemTool, tryRun } from "./exec.ts";
import {
  type PublishedArtifact,
  type ReleasePlan,
  ReleaseError,
  SBOM_PREDICATE_TYPE,
  imagePlatformProblems,
  isDigest,
  isMap,
  keylessVerifyArgs,
  parsePlan,
  parsePublished,
  requirePublishedAsPlanned,
  sha256Digest,
  signingIdentity,
  summarizeSpdx,
} from "./lib.ts";
import {
  type RegistryAccess,
  cosignRegistryFlags,
  login,
  registryAccess,
  registryHost,
  registryManifest,
  requirePlanRegistry,
  skopeoTls,
} from "./registry.ts";

/** Signature payload types of cosign's bundle format and of its older signature format. */
const SIGNATURE_TYPES: ReadonlySet<string> = new Set(["https://sigstore.dev/cosign/sign/v1", "cosign container image signature"]);
/** What skopeo reports when the registry refuses an anonymous request. */
const ANONYMOUS_REFUSED = /unauthorized|denied|authentication required|forbidden/i;

interface Tools {
  skopeo: string;
  cosign: string;
  helm: string;
}

function verifyFlags(plan: ReleasePlan, access: RegistryAccess, tag = plan.tag): string[] {
  if (access.localKey !== undefined) {
    return ["--key", access.localKey.publicKey, "--insecure-ignore-tlog", ...cosignRegistryFlags(access)];
  }
  return keylessVerifyArgs({ ...plan, tag });
}

/** The tag must point at the published digest. */
async function checkTag(access: RegistryAccess, artifact: PublishedArtifact): Promise<unknown> {
  const found = await registryManifest(access, artifact.ref);
  if (found === undefined) {
    throw new ReleaseError(`${artifact.ref} does not exist in the registry`);
  }
  if (found.digest !== artifact.digest) {
    throw new ReleaseError(`${artifact.ref} points at ${found.digest}, but ${artifact.digest} was published`);
  }
  return found.manifest;
}

/** `cosign verify` must return a signature (not only an attestation) for exactly this digest. */
async function checkSignature(plan: ReleasePlan, access: RegistryAccess, tools: Tools, artifact: PublishedArtifact): Promise<void> {
  const { stdout } = await run([tools.cosign, "verify", ...verifyFlags(plan, access), "--output", "json", artifact.ref]);
  const claims: unknown = JSON.parse(stdout);
  const signed =
    Array.isArray(claims) &&
    claims.some((claim: unknown) => {
      const critical = isMap(claim) && isMap(claim.critical) ? claim.critical : undefined;
      const type = critical?.type;
      const image = critical !== undefined && isMap(critical.image) ? critical.image : undefined;
      return typeof type === "string" && SIGNATURE_TYPES.has(type) && image?.["docker-manifest-digest"] === artifact.digest;
    });
  if (!signed) {
    throw new ReleaseError(`cosign verify found no signature of ${artifact.digest} for ${artifact.ref}`);
  }
}

/** The SBOM attestation must verify, be about this digest and hold an SPDX document. */
async function checkSbom(plan: ReleasePlan, access: RegistryAccess, tools: Tools, artifact: PublishedArtifact): Promise<string> {
  const { stdout } = await run([tools.cosign, "verify-attestation", ...verifyFlags(plan, access), "--type", "spdxjson", artifact.ref]);
  const hex = artifact.digest.slice("sha256:".length);
  for (const line of stdout.split("\n").filter((text) => text.trim() !== "")) {
    const envelope: unknown = JSON.parse(line);
    if (!isMap(envelope) || typeof envelope.payload !== "string") {
      continue;
    }
    const statement: unknown = JSON.parse(Buffer.from(envelope.payload, "base64").toString("utf8"));
    if (!isMap(statement) || statement.predicateType !== SBOM_PREDICATE_TYPE || !Array.isArray(statement.subject)) {
      continue;
    }
    const about = statement.subject.some((subject: unknown) => isMap(subject) && isMap(subject.digest) && subject.digest.sha256 === hex);
    if (about) {
      const summary = summarizeSpdx(statement.predicate);
      return `${summary.spdxVersion}，${summary.packages.length} 個套件`;
    }
  }
  throw new ReleaseError(`no verified SPDX SBOM attestation for ${artifact.digest} of ${artifact.ref}`);
}

/** An anonymous request must be refused: the packages are private. */
async function checkPrivate(access: RegistryAccess, tools: Tools, ref: string): Promise<boolean> {
  if (access.localKey !== undefined) {
    return false;
  }
  const result = await tryRun([tools.skopeo, "inspect", "--raw", "--no-creds", `docker://${ref}`]);
  if (result.exitCode === 0) {
    throw new ReleaseError(`${ref} can be pulled anonymously; the package must be private`);
  }
  if (!ANONYMOUS_REFUSED.test(result.stderr)) {
    throw new ReleaseError(`cannot tell whether ${ref} is private: ${result.stderr.trim()}`);
  }
  return true;
}

await runStage("verify", async () => {
  const options = parseOptions(Bun.argv.slice(2), {
    plan: {},
    published: {},
    output: { default: "" },
    "local-test-key": { default: "" },
  });
  const plan = parsePlan(readFileSync(options.plan ?? "", "utf8"));
  const published = parsePublished(readFileSync(options.published ?? "", "utf8"));
  const access = registryAccess(options["local-test-key"] ?? "");
  requirePlanRegistry(access, plan);
  // Before any credential is used: the publish result must name exactly what was planned, and
  // logins go only to the plan's registry hosts.
  requirePublishedAsPlanned(plan, published);
  const tools: Tools = { skopeo: systemTool("skopeo"), cosign: await protoTool("cosign"), helm: await protoTool("helm") };
  for (const host of new Set([...plan.images.map((image) => registryHost(image.repository)), registryHost(plan.chart.repository)])) {
    await login(access, host, tools);
  }

  const lines: string[] = [];
  for (const image of published.images) {
    const planned = plan.images.find((candidate) => candidate.name === image.name);
    if (planned === undefined) {
      throw new ReleaseError(`published image ${image.name} was not planned`);
    }
    const manifest = await checkTag(access, image);
    const config = JSON.parse(
      (await run([tools.skopeo, "inspect", "--config", "--raw", ...skopeoTls(access, "inspect"), `docker://${planned.repository}@${image.digest}`])).stdout,
    );
    const problems = imagePlatformProblems(manifest, config);
    if (problems.length > 0) {
      throw new ReleaseError(`${image.ref}: ${problems.join("; ")}`);
    }
    await checkSignature(plan, access, tools, image);
    const sbom = await checkSbom(plan, access, tools, image);
    const isPrivate = await checkPrivate(access, tools, image.ref);
    log("verify", `${image.ref}: ${image.digest}, signature and SBOM attestation verified (${sbom})`);
    lines.push(
      `- 映像 \`${image.name}\`：tag 指向 \`${image.digest}\`；平台 linux/arm64；cosign 簽章驗證通過；SBOM attestation 驗證通過（${sbom}）` +
        (isPrivate ? "；匿名拉取被拒（套件為私有）。" : "。"),
    );
  }

  const chart = published.chart;
  const chartManifest = await checkTag(access, chart);
  const chartConfig = isMap(chartManifest) && isMap(chartManifest.config) ? chartManifest.config.mediaType : undefined;
  if (chartConfig !== "application/vnd.cncf.helm.config.v1+json") {
    throw new ReleaseError(`${chart.ref} is not a Helm chart (config ${String(chartConfig)})`);
  }
  // The chart archive is the manifest's only layer; the pulled archive must be that blob.
  const chartLayers = isMap(chartManifest) && Array.isArray(chartManifest.layers) ? chartManifest.layers : [];
  const chartLayer: unknown = chartLayers[0];
  if (chartLayers.length !== 1 || !isMap(chartLayer) || !isDigest(chartLayer.digest)) {
    throw new ReleaseError(`${chart.ref} does not hold exactly one chart archive`);
  }
  await checkSignature(plan, access, tools, chart);
  const workDir = mkdtempSync(join(tmpdir(), "trustiot-verify-"));
  let pins: ChartPin[];
  try {
    await run([
      tools.helm,
      "pull",
      `oci://${plan.chart.repository}`,
      "--version",
      plan.version,
      "--destination",
      workDir,
      ...(access.localKey === undefined ? [] : ["--plain-http"]),
    ]);
    const archive = join(workDir, `${plan.chart.name}-${plan.version}.tgz`);
    const pulled = sha256Digest(new Uint8Array(readFileSync(archive)));
    if (pulled !== chartLayer.digest) {
      throw new ReleaseError(`helm pulled ${pulled} for ${chart.ref}, not the published archive ${chartLayer.digest}`);
    }
    const shown = Bun.YAML.parse((await run([tools.helm, "show", "chart", archive])).stdout);
    if (!isMap(shown) || shown.version !== plan.version || shown.appVersion !== plan.version) {
      throw new ReleaseError(`${chart.ref}: Chart.yaml must have version and appVersion ${plan.version}`);
    }
    // The images the chart deploys must be this release's, by digest, in every shipped shape.
    const chartDir = await unpackChart(systemTool("tar"), archive, join(workDir, "unpacked"), plan.chart.name);
    const { values, shapes } = readChartValues(chartDir);
    pins = chartPins(plan.images, published.images, values);
    const problems = chartPinProblems(values, shapes, pins, plan.version);
    if (problems.length > 0) {
      throw new ReleaseError(`${chart.ref} does not pin this release's images: ${problems.join("; ")}`);
    }
  } finally {
    rmSync(workDir, { recursive: true, force: true });
  }
  const chartPrivate = await checkPrivate(access, tools, chart.ref);
  log("verify", `${chart.ref}: ${chart.digest}, signature verified, version and appVersion ${plan.version}`);
  for (const pin of pins) {
    log("verify", `${chart.ref} deploys ${pinnedReference(pin, plan.version)}`);
  }
  const notDeployed = published.images.filter((image) => !pins.some((pin) => pin.image.name === image.name)).map((image) => `\`${image.name}\``);
  const shapeFiles = SHAPE_VALUES_FILES.map((file) => `\`${file}\``).join("、");
  const pinned =
    pins.length === 0
      ? "chart 不部署這次發佈的任何映像"
      : `chart 部署的映像以 digest 固定為這次發佈的映像（只用 \`values.yaml\` 與分別疊上 ${shapeFiles} 都相同）：${pins.map((pin) => `\`${pinnedReference(pin, plan.version)}\``).join("、")}` +
        (notDeployed.length > 0 ? `；${notDeployed.join("、")} 不由 chart 部署` : "");
  lines.push(
    `- Helm chart \`${chart.name}\`：tag 指向 \`${chart.digest}\`；cosign 簽章驗證通過；拉下的 chart 就是發佈的檔案；Chart.yaml 的 version 與 appVersion 都是 \`${plan.version}\`；${pinned}` +
      (chartPrivate ? "；匿名拉取被拒（套件為私有）。" : "。"),
  );

  // The same check with an identity this release does not have must fail.
  const first = published.images[0] ?? chart;
  const wrong = access.localKey === undefined ? verifyFlags(plan, access, `${plan.tag}-not-this-release`) : null;
  if (wrong !== null) {
    const result = await tryRun([tools.cosign, "verify", ...wrong, first.ref]);
    if (result.exitCode === 0) {
      throw new ReleaseError(`cosign verify accepted ${first.ref} for a different identity`);
    }
    lines.push(`- 對照：以另一個身分（\`refs/tags/${plan.tag}-not-this-release\`）驗證 \`${first.name}\` 被拒，上列驗證不是空檢查。`);
  }

  const runUrl =
    process.env.GITHUB_SERVER_URL && process.env.GITHUB_REPOSITORY && process.env.GITHUB_RUN_ID
      ? `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`
      : "";
  const signer =
    access.localKey === undefined
      ? `簽章身分為 \`${signingIdentity(plan.repository, plan.tag)}\``
      : "以本機測試金鑰驗證簽章（本機測試，不是正式發佈）";
  const report = [
    "## 驗證結果",
    "",
    `${new Date().toISOString().replace(/\.\d+Z$/, "Z")} 由 release workflow 的 verify job 以唯讀權限從 registry 檢查，${signer}：`,
    "",
    ...lines,
    ...(runUrl ? ["", `執行紀錄：${runUrl}`] : []),
    "",
  ].join("\n");
  if (options.output) {
    writeFileSync(options.output, report);
  }
  setOutput("report", report);
  setOutput("verified", String(published.images.length));
  log("verify", `verified ${published.images.length} image(s) and the chart`);
});
