/**
 * Aligns the demo machine's platform components with the repository before Flux installs the
 * chart (Laysi/trustiot-v2#465, part of the demo environment Laysi/trustiot-v2#457). The pins are
 * the ones the nightly installs (.github/workflows/nightly.yml: ISTIO_VERSION,
 * GATEWAY_API_VERSION, CNPG_VERSION; `bun test scripts/nightly` keeps them equal to
 * deploy/istio/README.md and deploy/cloudnative-pg/README.md). The lab installed Istio as k3s
 * HelmCharts in kube-system (deploy/istio/README.md).
 *
 * Checks (read only):
 * - Istio: the four HelmCharts at ISTIO_VERSION and not failed, the running istiod, ztunnel and
 *   istio-cni images at that version; istio-cni and ztunnel with values-k3s.yaml, istio-base with
 *   no values or values-k3s.yaml (both render the same; checked for 1.30.5 on 2026-10-20);
 * - istiod's mesh config: the extension providers and the path normalization;
 * - every Gateway in the cluster (waypoints included): the mesh-wide change applies to them;
 * - the Gateway API CRDs the chart uses, from the standard channel at GATEWAY_API_VERSION, and the
 *   GatewayClass `istio`;
 * - the CloudNativePG operator at CNPG_VERSION, available, with the CRDs the chart uses and
 *   restricted Pod Security on its namespace.
 *
 * Changes (with --apply), only one: istiod's values become exactly values-k3s.yaml plus
 * istiod-values.yaml, which replaces the PoC's ext_authz provider with `trustiot-identity` and
 * turns on the mesh's path normalization DECODE_AND_MERGE_SLASHES (Laysi/trustiot-v2#255).
 * k3s's helm-controller upgrades istiod; istiod applies both at once. Everything else that
 * differs is reported for a person to fix with the component's README: this script does not
 * install or upgrade cluster-wide components.
 *
 * The HelmChart itself is patched, not overlaid with a HelmChartConfig: lab e2e tests add and
 * delete that overlay (deploy/chart/tests/e2e/README.md), which would take the demo's provider
 * away with it. The lab applied the HelmChart from a file on the demo machine
 * (deploy/istio/README.md); applying that file again would put the PoC's values back. The patch
 * is a compare-and-swap (a JSON patch that tests the resourceVersion read under the lock and the
 * exact valuesContent the plan was confirmed against), and it records the sha256 of what it wrote
 * in the annotation APPLIED_ANNOTATION.
 *
 *   bun scripts/demo/platform.ts --ssh-host <host>                        # check and plan
 *   bun scripts/demo/platform.ts --ssh-host <host> --apply --confirm <code>
 *   bun scripts/demo/platform.ts --ssh-host <host> --rollback <saved json> [--apply --confirm <code>]
 *
 * The confirmation code names the change (istiod's values before and after, and the Gateways it
 * applies to). cleanup.ts removes the PoC's Gateway, so take the code from a dry run after it.
 * --apply refuses while something blocks the change (see planPlatform). The HelmChart as it was is
 * saved next to the log before the change; --rollback puts its valuesContent back byte for byte,
 * under the rules of planRollback.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { CNPG_CRDS, CNPG_DEPLOYMENT, CNPG_NAMESPACE } from "../nightly/platform.ts";
import { type VersionPins, readPins } from "../nightly/run.ts";
import { parseOptions } from "../release/exec.ts";
import { ReleaseError } from "../release/lib.ts";
import {
  Box,
  COMMON_SPEC,
  type CommonOptions,
  ISTIOD_PROVIDER,
  type KindState,
  Log,
  type OpsTunnel,
  ROOT,
  checkConfirmation,
  clusterState,
  commonOptions,
  lastLines,
  main,
  opsTunnel,
  sha256,
  shq,
  stableJson,
  stateDiff,
  takeLock,
  withOpsTunnel,
} from "./lib.ts";

/** The issue whose lab lock this script takes. */
export const ISSUE = "Laysi/trustiot-v2#465";
export const ISTIO_CHARTS = ["istio-base", "istiod", "istio-cni", "ztunnel"] as const;
export type IstioChart = (typeof ISTIO_CHARTS)[number];
/** The Istio workloads and the image each runs, `<registry>/<name>:<version>[-<variant>]`. */
export const ISTIO_WORKLOADS = { "deployment/istiod": "pilot", "daemonset/ztunnel": "ztunnel", "daemonset/istio-cni-node": "install-cni" } as const;
/** The Gateway API kinds the chart renders (deploy/chart/templates/ingress.yaml), and its GatewayClass. */
export const GATEWAY_API_CRDS = ["gateways.gateway.networking.k8s.io", "httproutes.gateway.networking.k8s.io"] as const;
export const GATEWAY_CLASS = "istio";
/** The sha256 of the valuesContent this script last wrote into HelmChart istiod. */
export const APPLIED_ANNOTATION = "trustiot.io/demo-platform-values-sha256";
/** k3s marks objects it applies from its manifests directory (and re-applies on restart) with these. */
const ADDON_ANNOTATION_PREFIX = "objectset.rio.cattle.io/";
const NIGHTLY_WORKFLOW = ".github/workflows/nightly.yml";

type Values = Record<string, unknown>;

function isMap(value: unknown): value is Values {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Helm's merge of values files: maps merge recursively, everything else is replaced. */
export function mergeValues(base: Values, overlay: Values): Values {
  const out: Values = { ...base };
  for (const [key, value] of Object.entries(overlay)) {
    out[key] = isMap(value) && isMap(out[key]) ? mergeValues(out[key] as Values, value) : value;
  }
  return out;
}

export function parseValuesContent(text: string | undefined): Values {
  if (text === undefined || text.trim() === "") return {};
  const parsed: unknown = Bun.YAML.parse(text);
  if (!isMap(parsed)) throw new ReleaseError("a HelmChart's valuesContent is not a YAML mapping");
  return parsed;
}

/** The valuesContent --apply writes: the values as JSON (which is YAML), without the files' comments. */
export function valuesContentOf(values: Values): string {
  return `${JSON.stringify(values, null, 2)}\n`;
}

/** The versions the nightly installs (nightly.yml's smoke job), the repository's pins. */
export function repoPins(root = ROOT): VersionPins {
  const workflow = Bun.YAML.parse(readFileSync(join(root, NIGHTLY_WORKFLOW), "utf8")) as { jobs?: { smoke?: { env?: Record<string, string> } } };
  const env = workflow.jobs?.smoke?.env;
  if (env === undefined) throw new ReleaseError(`${NIGHTLY_WORKFLOW} has no jobs.smoke.env with the platform pins`);
  return readPins(env);
}

/** The values each Istio chart may have: the repository's files (deploy/istio/README.md). */
export function desiredIstioValues(k3s: Values, istiod: Values): Record<IstioChart, Values[]> {
  return {
    // istio-base renders the same with and without values-k3s.yaml (1.30.5), and the lab has none.
    "istio-base": [{}, k3s],
    istiod: [mergeValues(k3s, istiod)],
    "istio-cni": [k3s],
    ztunnel: [k3s],
  };
}

export interface HelmChartFacts {
  name: string;
  version?: string;
  /** spec.valuesContent exactly as stored; undefined when the HelmChart has none. */
  valuesContent?: string;
  failed: boolean;
  /** spec.set is not empty: values that override valuesContent. */
  set: boolean;
  /** spec.valuesSecrets is not empty: values from Secrets that override valuesContent. */
  valuesSecrets: boolean;
  /** An objectset.rio.cattle.io/* annotation: a k3s AddOn, re-applied from its manifest on restart. */
  addon: boolean;
  /** The APPLIED_ANNOTATION value, when this script wrote the values. */
  appliedSha256?: string;
}

export interface MeshFacts {
  /** The extension providers of istiod's mesh config, as rendered. */
  extensionProviders: { name?: string }[];
  pathNormalization?: string;
}

export interface PlatformFacts {
  charts: HelmChartFacts[];
  /** The image each of ISTIO_WORKLOADS runs; absent when the workload is missing. */
  istioImages: Partial<Record<keyof typeof ISTIO_WORKLOADS, string>>;
  mesh: MeshFacts;
  /** AuthorizationPolicies with action CUSTOM: `<namespace>/<name>=<provider>`. */
  customPolicies: string[];
  /** Every Gateway (boxGateways): `<namespace>/<name>=<gatewayClassName>`, or `=networking.istio.io` for Istio's own. */
  gateways: string[];
  /**
   * HelmChartConfigs in kube-system that overlay an Istio chart: a lab e2e test's temporary
   * provider (deploy/chart/tests/e2e/README.md). While one exists, istiod does not run the
   * HelmChart's values alone.
   */
  overlays: string[];
  gatewayCrds: Partial<Record<(typeof GATEWAY_API_CRDS)[number], { bundleVersion?: string; channel?: string }>>;
  gatewayClass?: { controller?: string; accepted: boolean };
  cnpg: { image?: string; available: boolean; namespaceEnforce?: string; crds: string[] };
}

/** One change of HelmChart istiod's valuesContent, named by the sha256 of the exact text before and after. */
export interface ValuesChange {
  name: "istiod";
  from: Values;
  to: Values;
  fromSha256: string;
  toSha256: string;
}

export interface PlatformPlan {
  /** HelmCharts whose values --apply sets (only istiod). */
  setValues: ValuesChange[];
  /** The Gateways (waypoints included) a mesh-wide change applies to. */
  gateways: string[];
  /** What makes --apply refuse the change. */
  blockers: string[];
  /** Differences from the pins that a person fixes with the component's README; never changed here. */
  differences: string[];
  ok: string[];
}

function valuesOf(chart: HelmChartFacts): Values {
  return parseValuesContent(chart.valuesContent);
}

function providerNames(values: Values | undefined): string[] {
  const mesh = values?.meshConfig;
  return isMap(mesh) && Array.isArray(mesh.extensionProviders) ? (mesh.extensionProviders as { name?: string }[]).map((p) => String(p.name)).sort() : [];
}

function normalizationOf(values: Values | undefined): string | undefined {
  const mesh = values?.meshConfig;
  const normalization = isMap(mesh) && isMap(mesh.pathNormalization) ? mesh.pathNormalization.normalization : undefined;
  return typeof normalization === "string" ? normalization : undefined;
}

/** CUSTOM policies whose provider is not among these values' providers: they would deny everything they cover. */
function orphanedPolicies(facts: PlatformFacts, values: Values): string[] {
  const providers = providerNames(values);
  return facts.customPolicies.filter((p) => !providers.includes(p.slice(p.lastIndexOf("=") + 1)));
}

/** What makes a write to HelmChart istiod unsafe whatever the values: something else also sets them. */
function istiodWriteBlockers(chart: HelmChartFacts, facts: PlatformFacts): string[] {
  return [
    ...(chart.valuesContent === undefined ? ["HelmChart istiod has no spec.valuesContent to compare and swap"] : []),
    ...(chart.set ? ["HelmChart istiod has spec.set, which overrides valuesContent: move it into the values first"] : []),
    ...(chart.valuesSecrets ? ["HelmChart istiod has spec.valuesSecrets, which override valuesContent: move them into the values first"] : []),
    ...(chart.addon ? ["HelmChart istiod is a k3s AddOn (objectset.rio.cattle.io annotations): k3s re-applies it from its manifests directory on restart; change that manifest instead"] : []),
    ...facts.overlays.map((overlay) => `HelmChartConfig kube-system/${overlay} overlays the Istio values (a lab e2e run?); wait until its owner removes it`),
  ];
}

/**
 * What to change, what blocks it and what to report, from the facts. Pure, so it is tested without
 * a cluster. Blockers: istiod not at the pin or failed; anything else that sets istiod's values
 * (spec.set, spec.valuesSecrets, a k3s AddOn, a HelmChartConfig overlay); a CUSTOM policy whose
 * provider the new values drop.
 */
export function planPlatform(facts: PlatformFacts, desired: Record<IstioChart, Values[]>, pins: Pick<VersionPins, "istioVersion" | "gatewayApiVersion" | "cnpgVersion">): PlatformPlan {
  const plan: PlatformPlan = { setValues: [], gateways: [...facts.gateways].sort(), blockers: [], differences: [], ok: [] };
  const istio = pins.istioVersion;
  const wantedIstiod = desired.istiod[0]!;

  for (const name of ISTIO_CHARTS) {
    const chart = facts.charts.find((c) => c.name === name);
    const wanted = desired[name];
    if (chart === undefined) {
      (name === "istiod" ? plan.blockers : plan.differences).push(`HelmChart kube-system/${name} is missing: install Istio ${istio} as deploy/istio/README.md describes`);
      continue;
    }
    const issues: string[] = [];
    if (chart.version !== istio) issues.push(`HelmChart ${name} is at ${chart.version ?? "no version"}, the pin is ${istio}: upgrade the four Istio charts together (deploy/istio/README.md)`);
    if (chart.failed) issues.push(`HelmChart ${name} failed to install: see the logs of the Job kube-system/helm-install-${name}`);
    const values = valuesOf(chart);
    if (name === "istiod") {
      if (!wanted.some((v) => stableJson(v) === stableJson(values))) {
        // istiod's values are what --apply sets; the chart must be at the pin, healthy and the only
        // source of its values first.
        plan.blockers.push(...issues, ...istiodWriteBlockers(chart, facts));
        const orphaned = orphanedPolicies(facts, wantedIstiod);
        if (orphaned.length > 0) {
          plan.blockers.push(`CUSTOM AuthorizationPolicies use providers the new values drop (${orphaned.join(", ")}): the PoC's are removed by scripts/demo/cleanup.ts, run it first`);
        }
        plan.setValues.push({ name, from: values, to: wantedIstiod, fromSha256: sha256(chart.valuesContent ?? ""), toSha256: sha256(valuesContentOf(wantedIstiod)) });
      } else {
        plan.differences.push(...issues);
        if (chart.set || chart.valuesSecrets) plan.differences.push("HelmChart istiod has spec.set or spec.valuesSecrets, which override its values");
        plan.ok.push("istiod: values are deploy/istio/values-k3s.yaml plus istiod-values.yaml");
      }
      continue;
    }
    plan.differences.push(...issues);
    if (wanted.some((v) => stableJson(v) === stableJson(values)) && !chart.set && !chart.valuesSecrets) {
      if (issues.length === 0) plan.ok.push(`${name}: ${istio}, values match deploy/istio`);
    } else {
      plan.differences.push(`HelmChart ${name} has other values than deploy/istio/values-k3s.yaml`);
    }
  }

  for (const [workload, image] of Object.entries(ISTIO_WORKLOADS) as [keyof typeof ISTIO_WORKLOADS, string][]) {
    const running = facts.istioImages[workload];
    const tag = running === undefined ? undefined : /:([^:/@]+)(@.*)?$/.exec(running)?.[1];
    const name = running === undefined ? undefined : /\/([^/:@]+)[:@][^/]*$/.exec(running)?.[1];
    if (running === undefined) plan.differences.push(`istio-system ${workload} is missing`);
    else if (name !== image || (tag !== istio && !tag?.startsWith(`${istio}-`))) plan.differences.push(`istio-system ${workload} runs ${running}, not ${image} ${istio}`);
    else plan.ok.push(`${workload}: ${running}`);
  }

  if (plan.setValues.length === 0) {
    const rendered = facts.mesh.extensionProviders.map((p) => String(p.name)).sort();
    if (stableJson(rendered) !== stableJson(providerNames(wantedIstiod)) || facts.mesh.pathNormalization !== normalizationOf(wantedIstiod)) {
      plan.differences.push(
        `istiod's mesh config (providers ${rendered.join(", ") || "none"}, path normalization ${facts.mesh.pathNormalization ?? "default"}) does not show its values: check the Job kube-system/helm-install-istiod`,
      );
    } else {
      plan.ok.push(`istiod's mesh config: provider ${ISTIOD_PROVIDER}, path normalization ${facts.mesh.pathNormalization}`);
    }
  }

  for (const crd of GATEWAY_API_CRDS) {
    const found = facts.gatewayCrds[crd];
    if (found === undefined) plan.differences.push(`CRD ${crd} is missing: install the Gateway API ${pins.gatewayApiVersion} standard channel (deploy/istio/README.md)`);
    else if (found.channel !== "standard") plan.differences.push(`CRD ${crd} is from the ${found.channel ?? "unknown"} channel, not standard`);
    else if (found.bundleVersion !== pins.gatewayApiVersion) plan.differences.push(`CRD ${crd} is Gateway API ${found.bundleVersion ?? "of no version"}, the pin is ${pins.gatewayApiVersion}`);
    else plan.ok.push(`${crd}: Gateway API ${found.bundleVersion} (standard)`);
  }
  if (facts.gatewayClass === undefined) plan.differences.push(`GatewayClass ${GATEWAY_CLASS} is missing (istiod creates it)`);
  else if (!facts.gatewayClass.accepted) plan.differences.push(`GatewayClass ${GATEWAY_CLASS} is not accepted`);
  else plan.ok.push(`GatewayClass ${GATEWAY_CLASS}: accepted (${facts.gatewayClass.controller ?? "?"})`);

  const cnpg = pins.cnpgVersion;
  const operatorImage = `ghcr.io/cloudnative-pg/cloudnative-pg:${cnpg}`;
  if (facts.cnpg.image !== operatorImage) {
    plan.differences.push(`the CloudNativePG operator runs ${facts.cnpg.image ?? "nothing"}, the pin is ${cnpg} (deploy/cloudnative-pg/README.md)`);
  } else if (!facts.cnpg.available) {
    plan.differences.push(`the CloudNativePG operator ${cnpg} is not available (${CNPG_NAMESPACE}/${CNPG_DEPLOYMENT})`);
  } else {
    plan.ok.push(`CloudNativePG operator ${cnpg}, available`);
  }
  const missingCrds = CNPG_CRDS.map((crd) => crd.replace(/^crd\//, "")).filter((crd) => !facts.cnpg.crds.includes(crd));
  if (missingCrds.length > 0) plan.differences.push(`CloudNativePG CRDs missing: ${missingCrds.join(", ")}`);
  if (facts.cnpg.namespaceEnforce !== "restricted") plan.differences.push(`namespace ${CNPG_NAMESPACE} does not enforce the restricted Pod Security Standard (deploy/cloudnative-pg/README.md)`);
  return plan;
}

export interface RollbackPlan {
  /** istiod's values now and the saved ones; empty when they are already equal. */
  setValues: ValuesChange[];
  gateways: string[];
  blockers: string[];
}

/**
 * What --rollback would change, from the facts. Pure. Blockers: the values now are not what the
 * last --apply wrote (a rollback would overwrite a newer setting); anything else sets istiod's
 * values; a CUSTOM policy uses a provider the saved values do not have; the saved values change the
 * mesh's path normalization while a Gateway or waypoint exists (Laysi/trustiot-v2#255: it would
 * re-open encoded and doubled slashes past the chart's /internal* DENY).
 */
export function planRollback(facts: PlatformFacts, saved: string): RollbackPlan {
  const gateways = [...facts.gateways].sort();
  const chart = facts.charts.find((c) => c.name === "istiod");
  if (chart === undefined) return { setValues: [], gateways, blockers: ["HelmChart kube-system/istiod is missing"] };
  if (chart.valuesContent === saved) return { setValues: [], gateways, blockers: [] };
  const from = valuesOf(chart);
  const to = parseValuesContent(saved);
  const current = sha256(chart.valuesContent ?? "");
  const blockers = [
    ...(chart.appliedSha256 !== current
      ? [`istiod's values are not what the last --apply of this script wrote (${chart.appliedSha256 === undefined ? `no ${APPLIED_ANNOTATION}` : "they changed since"}); a rollback would overwrite them`]
      : []),
    ...istiodWriteBlockers(chart, facts),
  ];
  const orphaned = orphanedPolicies(facts, to);
  if (orphaned.length > 0) blockers.push(`CUSTOM AuthorizationPolicies use providers the saved values do not have (${orphaned.join(", ")}); they would deny every request they cover`);
  if (normalizationOf(from) !== normalizationOf(to) && gateways.length > 0) {
    blockers.push(
      `the saved values change the mesh's path normalization (${normalizationOf(from) ?? "default"} -> ${normalizationOf(to) ?? "default"}) while Gateways or waypoints exist (${gateways.join(", ")}): that re-opens //internal and %2f paths past the chart's DENY (Laysi/trustiot-v2#255)`,
    );
  }
  return { setValues: [{ name: "istiod", from, to, fromSha256: current, toSha256: sha256(saved) }], gateways, blockers };
}

// --- The run -----------------------------------------------------------------------------------

/** What the run does to the cluster; boxOps does it over ssh, the tests with a fake cluster. */
export interface PlatformOps {
  tunnel(): Promise<OpsTunnel>;
  facts(): Promise<PlatformFacts>;
  /** istiod's mesh config alone: what the wait after a change polls. */
  mesh(): Promise<MeshFacts>;
  lock(step: string): Promise<() => Promise<void>>;
  clusterState(): Promise<Record<string, KindState>>;
  /** The HelmChart kube-system/istiod as the API returns it: the backup and the compare-and-swap's expected object. */
  istiodChart(): Promise<Record<string, unknown>>;
  /**
   * Sends istiodPatch(chart, next): replaces istiod's spec.valuesContent with `next` and records
   * sha256(next) in APPLIED_ANNOTATION only if the HelmChart is still exactly `chart`. Fails,
   * changing nothing, when anything in it changed in between.
   */
  swapIstiodValues(chart: Record<string, unknown>, next: string): Promise<void>;
  rolloutIstiod(): Promise<void>;
  now(): number;
  sleep(ms: number): Promise<void>;
}

const MESH_WAIT_MS = 600_000;

/**
 * The JSON patch for the HelmChart `chart` as it was read: test its resourceVersion and its exact
 * old values text, replace the text, record what was written. The API server applies the patch
 * atomically to the object as it is then, and every write changes the resourceVersion, so any
 * change since the read (another annotation, other values) fails the `test` and changes nothing.
 * That also covers a chart read without annotations, where the patch adds the whole map.
 */
export function istiodPatch(chart: Record<string, unknown>, next: string): object[] {
  const metadata = (chart.metadata ?? {}) as { resourceVersion?: unknown; annotations?: Record<string, string> };
  const expected = (chart.spec as { valuesContent?: string } | undefined)?.valuesContent;
  if (expected === undefined) throw new ReleaseError("HelmChart istiod has no spec.valuesContent to compare and swap");
  if (typeof metadata.resourceVersion !== "string" || metadata.resourceVersion === "") {
    throw new ReleaseError("HelmChart istiod has no metadata.resourceVersion to compare and swap");
  }
  const annotation = `/metadata/annotations/${APPLIED_ANNOTATION.replaceAll("~", "~0").replaceAll("/", "~1")}`;
  return [
    { op: "test", path: "/metadata/resourceVersion", value: metadata.resourceVersion },
    { op: "test", path: "/spec/valuesContent", value: expected },
    { op: "replace", path: "/spec/valuesContent", value: next },
    metadata.annotations !== undefined
      ? { op: "add", path: annotation, value: sha256(next) }
      : { op: "add", path: "/metadata/annotations", value: { [APPLIED_ANNOTATION]: sha256(next) } },
  ];
}

/** The confirmation covers the change and the Gateways it applies to, not the blockers: a blocker that goes away keeps the code. */
function changeOf(plan: { setValues: ValuesChange[]; gateways: string[] }): unknown {
  return { setValues: plan.setValues, gateways: plan.gateways };
}

/**
 * Reads HelmChart istiod (the backup), checks it is still the text the plan was confirmed against,
 * swaps in `next`, and waits until istiod's mesh config shows the new providers and path
 * normalization.
 */
async function swapAndWait(ops: PlatformOps, log: Log, change: ValuesChange, next: string, backupName: string): Promise<void> {
  const chart = await ops.istiodChart();
  const content = (chart.spec as { valuesContent?: string } | undefined)?.valuesContent;
  if (content === undefined || sha256(content) !== change.fromSha256) {
    throw new ReleaseError("HelmChart istiod's valuesContent is not the one the plan was confirmed against; nothing changed, run again without --apply");
  }
  const backup = log.save(backupName, chart);
  log.note(`saved the HelmChart as it was: ${backup} (roll back with --rollback <that file>)`);
  await ops.swapIstiodValues(chart, next);
  log.note("patched HelmChart kube-system/istiod; waiting for k3s's helm-controller to upgrade istiod");
  const values = parseValuesContent(next);
  const providers = providerNames(values);
  const normalization = normalizationOf(values);
  const deadline = ops.now() + MESH_WAIT_MS;
  for (;;) {
    const mesh = await ops.mesh();
    const rendered = mesh.extensionProviders.map((p) => String(p.name)).sort();
    if (stableJson(rendered) === stableJson(providers) && mesh.pathNormalization === normalization) break;
    if (ops.now() > deadline) {
      throw new ReleaseError(
        `timed out after ${MESH_WAIT_MS / 1000} s waiting for istiod's mesh config to show providers [${providers.join(", ")}] and path normalization ${normalization ?? "default"} (it shows [${rendered.join(", ")}], ${mesh.pathNormalization ?? "default"}); see the Job kube-system/helm-install-istiod`,
      );
    }
    await ops.sleep(5_000);
  }
  await ops.rolloutIstiod();
  log.note("istiod serves the new mesh config");
}

function report(log: Log, plan: PlatformPlan): void {
  for (const line of plan.ok) log.note(`ok: ${line}`);
  for (const line of plan.differences) log.note(`DIFFERENCE (a person fixes it; this script does not): ${line}`);
  for (const line of plan.blockers) log.note(`BLOCKED (--apply refuses until it is gone): ${line}`);
  log.note(`Gateways and waypoints in the cluster (a mesh-wide change applies to them): ${plan.gateways.join(", ") || "none"}`);
  for (const change of plan.setValues) {
    log.note(`change: HelmChart ${change.name} values\n  from: ${stableJson(change.from)}\n  to:   ${stableJson(change.to)}`);
  }
}

/**
 * The check and the change. Without --apply it only reads. With --apply it refuses while a
 * blocker exists, checks the confirmation code, re-plans under the lock, saves the HelmChart,
 * swaps the values, waits for istiod and checks the result.
 */
export async function runPlatform(ops: PlatformOps, log: Log, options: CommonOptions, desired: Record<IstioChart, Values[]>, pins: VersionPins): Promise<PlatformPlan> {
  return withOpsTunnel(
    () => ops.tunnel(),
    log,
    async () => {
      const facts = await ops.facts();
      log.save("facts", facts);
      const plan = planPlatform(facts, desired, pins);
      report(log, plan);
      log.save("plan", plan);
      if (plan.setValues.length === 0) {
        log.note(`istiod's values match the repository; nothing to change${plan.differences.length > 0 ? `; ${plan.differences.length} differences above need a person` : ""}`);
        return plan;
      }
      if (options.apply && plan.blockers.length > 0) {
        throw new ReleaseError(`${plan.blockers.length} blockers above; nothing changed`);
      }
      if (!checkConfirmation(log, changeOf(plan), options, "bun scripts/demo/platform.ts --ssh-host <host>")) {
        if (plan.blockers.length > 0) log.note(`--apply with this code refuses until the ${plan.blockers.length} blockers above are gone`);
        return plan;
      }

      const release = await ops.lock("platform");
      try {
        const again = planPlatform(await ops.facts(), desired, pins);
        if (stableJson(changeOf(again)) !== stableJson(changeOf(plan))) {
          throw new ReleaseError("istiod's values or the Gateways changed since the plan was made; nothing changed, run again without --apply");
        }
        if (again.blockers.length > 0) throw new ReleaseError(`blocked: ${again.blockers.join("; ")}; nothing changed`);
        const before = await ops.clusterState();
        for (const change of plan.setValues) await swapAndWait(ops, log, change, valuesContentOf(change.to), "helmchart-istiod-before");
        const after = await ops.clusterState();
        const diff = stateDiff(before, after);
        log.save("state", { before, after, diff });
        log.note(`cluster-scoped changes: ${JSON.stringify(diff)}`);
        const finalFacts = await ops.facts();
        if (planPlatform(finalFacts, desired, pins).setValues.length > 0) throw new ReleaseError("after the change istiod's values still differ from the repository");
        const rendered = finalFacts.mesh.extensionProviders;
        const wanted = (desired.istiod[0]?.meshConfig as { extensionProviders?: unknown[] } | undefined)?.extensionProviders ?? [];
        log.note(
          stableJson(rendered) === stableJson(wanted)
            ? "istiod's mesh config carries deploy/istio/istiod-values.yaml's providers exactly"
            : `WARNING: istiod's rendered providers differ from deploy/istio/istiod-values.yaml: ${stableJson(rendered)}`,
        );
      } finally {
        await release();
      }
      log.note(`done; records in ${log.dir}`);
      return plan;
    },
  );
}

/** --rollback: istiod's valuesContent back, byte for byte, to a HelmChart that --apply saved. */
export async function runRollback(ops: PlatformOps, log: Log, options: CommonOptions, saved: string): Promise<RollbackPlan> {
  const backup = JSON.parse(readFileSync(saved, "utf8")) as { kind?: string; metadata?: { name?: string; namespace?: string }; spec?: { valuesContent?: unknown } };
  if (backup.kind !== "HelmChart" || backup.metadata?.name !== "istiod" || backup.metadata.namespace !== "kube-system" || typeof backup.spec?.valuesContent !== "string") {
    throw new ReleaseError(`${saved} is not a saved HelmChart kube-system/istiod with a valuesContent`);
  }
  const content = backup.spec.valuesContent;
  return withOpsTunnel(
    () => ops.tunnel(),
    log,
    async () => {
      const plan = planRollback(await ops.facts(), content);
      log.save("rollback-plan", plan);
      for (const line of plan.blockers) log.note(`BLOCKED (--apply refuses until it is gone): ${line}`);
      if (plan.setValues.length === 0) {
        if (plan.blockers.length > 0) throw new ReleaseError(plan.blockers.join("; "));
        log.note("istiod already has the saved values; nothing to roll back");
        return plan;
      }
      for (const change of plan.setValues) {
        log.note(`rollback: HelmChart istiod values\n  from: ${stableJson(change.from)}\n  to:   ${stableJson(change.to)} (the saved text, sha256 ${change.toSha256})`);
      }
      if (options.apply && plan.blockers.length > 0) throw new ReleaseError(`${plan.blockers.length} blockers above; nothing changed`);
      if (!checkConfirmation(log, { rollback: changeOf(plan) }, options, `bun scripts/demo/platform.ts --ssh-host <host> --rollback ${shq(saved)}`)) return plan;
      const release = await ops.lock("platform-rollback");
      try {
        const again = planRollback(await ops.facts(), content);
        if (stableJson(changeOf(again)) !== stableJson(changeOf(plan))) {
          throw new ReleaseError("istiod's values or the Gateways changed since the rollback plan was made; nothing changed, run again without --apply");
        }
        if (again.blockers.length > 0) throw new ReleaseError(`blocked: ${again.blockers.join("; ")}; nothing changed`);
        for (const change of plan.setValues) await swapAndWait(ops, log, change, content, "helmchart-istiod-before-rollback");
      } finally {
        await release();
      }
      return plan;
    },
  );
}

async function boxMesh(box: Box): Promise<MeshFacts> {
  const meshConfig = await box.getJson(["-n", "istio-system", "get", "configmap", "istio"]);
  const mesh = parseValuesContent((meshConfig?.data as { mesh?: string } | undefined)?.mesh);
  const pathNormalization = (mesh.pathNormalization as { normalization?: string } | undefined)?.normalization;
  return {
    extensionProviders: Array.isArray(mesh.extensionProviders) ? (mesh.extensionProviders as { name?: string }[]) : [],
    ...(pathNormalization === undefined ? {} : { pathNormalization }),
  };
}

/** The facts of one k3s HelmChart object. */
export function helmChartFacts(name: string, chart: Record<string, unknown>): HelmChartFacts {
  const spec = (chart.spec ?? {}) as { version?: string; valuesContent?: string; set?: Record<string, unknown>; valuesSecrets?: unknown[] };
  const metadata = (chart.metadata ?? {}) as { annotations?: Record<string, string> };
  const annotations = metadata.annotations ?? {};
  const conditions = (chart.status as { conditions?: { type?: string; status?: string }[] } | undefined)?.conditions ?? [];
  return {
    name,
    ...(spec.version === undefined ? {} : { version: spec.version }),
    ...(spec.valuesContent === undefined ? {} : { valuesContent: spec.valuesContent }),
    failed: conditions.some((c) => c.type === "Failed" && c.status === "True"),
    set: spec.set !== undefined && Object.keys(spec.set).length > 0,
    valuesSecrets: Array.isArray(spec.valuesSecrets) && spec.valuesSecrets.length > 0,
    addon: Object.keys(annotations).some((key) => key.startsWith(ADDON_ANNOTATION_PREFIX)),
    ...(annotations[APPLIED_ANNOTATION] === undefined ? {} : { appliedSha256: annotations[APPLIED_ANNOTATION] }),
  };
}

/** Istio's own Gateway kind; its entries in the Gateway list carry this instead of a class. */
export const ISTIO_GATEWAY_LABEL = "networking.istio.io";

/**
 * Every Gateway a mesh-wide change applies to, as `<namespace>/<name>=<class>`: Gateway API
 * Gateways (waypoints included, by their class) and Istio's own networking.istio.io Gateways
 * (`=networking.istio.io`). Each kind is listed only when its CRD is installed, so a missing CRD
 * shows up as a difference in the plan, not as a failed read.
 */
export async function boxGateways(box: Box): Promise<string[]> {
  const kinds: [string, (gateway: Record<string, unknown>) => string][] = [
    ["gateways.gateway.networking.k8s.io", (g) => String((g.spec as { gatewayClassName?: string } | undefined)?.gatewayClassName)],
    ["gateways.networking.istio.io", () => ISTIO_GATEWAY_LABEL],
  ];
  const gateways: string[] = [];
  for (const [resource, label] of kinds) {
    if ((await box.getJson(["get", "customresourcedefinitions.apiextensions.k8s.io", resource])) === undefined) continue;
    for (const gateway of await box.items(["get", resource, "-A"])) {
      const metadata = gateway.metadata as { namespace: string; name: string };
      gateways.push(`${metadata.namespace}/${metadata.name}=${label(gateway)}`);
    }
  }
  return gateways.sort();
}

async function boxFacts(box: Box): Promise<PlatformFacts> {
  const charts: HelmChartFacts[] = [];
  for (const name of ISTIO_CHARTS) {
    const chart = await box.getJson(["-n", "kube-system", "get", "helmcharts.helm.cattle.io", name]);
    if (chart !== undefined) charts.push(helmChartFacts(name, chart));
  }
  const istioImages: PlatformFacts["istioImages"] = {};
  for (const workload of Object.keys(ISTIO_WORKLOADS) as (keyof typeof ISTIO_WORKLOADS)[]) {
    const found = await box.getJson(["-n", "istio-system", "get", workload]);
    const image = (found?.spec as { template?: { spec?: { containers?: { image?: string }[] } } } | undefined)?.template?.spec?.containers?.[0]?.image;
    if (image !== undefined) istioImages[workload] = image;
  }
  const mesh = await boxMesh(box);
  const customPolicies = (await box.items(["get", "authorizationpolicies.security.istio.io", "-A"]))
    .filter((p) => (p.spec as { action?: string }).action === "CUSTOM")
    .map((p) => {
      const metadata = p.metadata as { namespace: string; name: string };
      return `${metadata.namespace}/${metadata.name}=${String((p.spec as { provider?: { name?: string } }).provider?.name)}`;
    })
    .sort();
  const gateways = await boxGateways(box);
  const gatewayCrds: PlatformFacts["gatewayCrds"] = {};
  for (const crd of GATEWAY_API_CRDS) {
    const found = await box.getJson(["get", "customresourcedefinitions.apiextensions.k8s.io", crd]);
    if (found === undefined) continue;
    const annotations = (found.metadata as { annotations?: Record<string, string> }).annotations ?? {};
    gatewayCrds[crd] = {
      ...(annotations["gateway.networking.k8s.io/bundle-version"] === undefined ? {} : { bundleVersion: annotations["gateway.networking.k8s.io/bundle-version"] }),
      ...(annotations["gateway.networking.k8s.io/channel"] === undefined ? {} : { channel: annotations["gateway.networking.k8s.io/channel"] }),
    };
  }
  const gatewayClass = await box.getJson(["get", "gatewayclasses.gateway.networking.k8s.io", GATEWAY_CLASS]);
  const operator = await box.getJson(["-n", CNPG_NAMESPACE, "get", "deployment", CNPG_DEPLOYMENT]);
  const cnpgNs = await box.getJson(["get", "namespace", CNPG_NAMESPACE]);
  const cnpgCrds: string[] = [];
  for (const crd of CNPG_CRDS.map((c) => c.replace(/^crd\//, ""))) {
    if ((await box.getJson(["get", "customresourcedefinitions.apiextensions.k8s.io", crd])) !== undefined) cnpgCrds.push(crd);
  }
  const operatorImage = (operator?.spec as { template?: { spec?: { containers?: { image?: string }[] } } } | undefined)?.template?.spec?.containers?.[0]?.image;
  const operatorConditions = (operator?.status as { conditions?: { type?: string; status?: string }[] } | undefined)?.conditions ?? [];
  const enforce = (cnpgNs?.metadata as { labels?: Record<string, string> } | undefined)?.labels?.["pod-security.kubernetes.io/enforce"];
  const classConditions = (gatewayClass?.status as { conditions?: { type?: string; status?: string }[] } | undefined)?.conditions ?? [];
  return {
    charts,
    istioImages,
    mesh,
    customPolicies,
    gateways,
    overlays: (await box.items(["-n", "kube-system", "get", "helmchartconfigs.helm.cattle.io"]))
      .map((c) => String((c.metadata as { name: string }).name))
      .filter((name) => (ISTIO_CHARTS as readonly string[]).includes(name))
      .sort(),
    gatewayCrds,
    ...(gatewayClass === undefined
      ? {}
      : {
          gatewayClass: {
            ...((gatewayClass.spec as { controllerName?: string }).controllerName === undefined ? {} : { controller: (gatewayClass.spec as { controllerName: string }).controllerName }),
            accepted: classConditions.some((c) => c.type === "Accepted" && c.status === "True"),
          },
        }),
    cnpg: {
      ...(operatorImage === undefined ? {} : { image: operatorImage }),
      available: operatorConditions.some((c) => c.type === "Available" && c.status === "True"),
      ...(enforce === undefined ? {} : { namespaceEnforce: enforce }),
      crds: cnpgCrds,
    },
  };
}

/** The clock boxOps waits with; tests replace it. */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}

const SYSTEM_CLOCK: Clock = { now: () => Date.now(), sleep: (ms) => Bun.sleep(ms) };

/** PlatformOps on the demo machine, over ssh. */
export function boxOps(box: Box, options: CommonOptions, clock: Clock = SYSTEM_CLOCK): PlatformOps {
  return {
    tunnel: () => opsTunnel(box),
    facts: () => boxFacts(box),
    mesh: () => boxMesh(box),
    lock: (step) => takeLock(box, ISSUE, step, options.lockHeld),
    clusterState: () => clusterState(box),
    async istiodChart() {
      const chart = await box.getJson(["-n", "kube-system", "get", "helmcharts.helm.cattle.io", "istiod"]);
      if (chart === undefined) throw new ReleaseError("HelmChart kube-system/istiod is gone");
      return chart;
    },
    async swapIstiodValues(chart, next) {
      // The values are not secret, so the patch can go on the command line. The API server applies
      // a JSON patch atomically: a failed `test` changes nothing.
      const patch = JSON.stringify(istiodPatch(chart, next));
      const command = ["sudo", "k3s", "kubectl", "-n", "kube-system", "patch", "helmcharts.helm.cattle.io", "istiod", "--type", "json", "--patch", patch];
      // Never sent twice: after a patch that went through, a repeat fails the resourceVersion test
      // and would report a failure for a change that was made.
      const result = await box.sh(command.map(shq).join(" "), { retry: false });
      if (result.exitCode === 0) return;
      if (result.exitCode !== 255) {
        throw new ReleaseError(`kubectl patch helmcharts.helm.cattle.io istiod failed (exit ${result.exitCode}), nothing changed: ${lastLines(result.stderr || result.stdout)}`);
      }
      // ssh dropped the connection: the patch may or may not have reached the API server. What
      // HelmChart istiod holds now tells which.
      const now = await box.getJson(["-n", "kube-system", "get", "helmcharts.helm.cattle.io", "istiod"]);
      const content = (now?.spec as { valuesContent?: string } | undefined)?.valuesContent;
      const applied = (now?.metadata as { annotations?: Record<string, string> } | undefined)?.annotations?.[APPLIED_ANNOTATION];
      if (content === next && applied === sha256(next)) {
        box.log.note("ssh dropped the connection during the patch, but HelmChart istiod holds the new values: the patch went through");
        return;
      }
      throw new ReleaseError(
        "ssh dropped the connection during the patch and HelmChart istiod does not hold the new values; run again without --apply to see where it stands",
      );
    },
    async rolloutIstiod() {
      await box.kubectl(["-n", "istio-system", "rollout", "status", "deployment/istiod", "--timeout=300s"]);
    },
    now: () => clock.now(),
    sleep: (ms) => clock.sleep(ms),
  };
}

function readValues(file: string): Values {
  return parseValuesContent(readFileSync(join(ROOT, file), "utf8"));
}

/** The values the repository wants for each Istio chart. */
export function repoIstioValues(): Record<IstioChart, Values[]> {
  return desiredIstioValues(readValues("deploy/istio/values-k3s.yaml"), readValues("deploy/istio/istiod-values.yaml"));
}

let log: Log | undefined;

if (import.meta.main) {
  await main(
    () => log,
    async () => {
      const raw = parseOptions(Bun.argv.slice(2), { ...COMMON_SPEC, rollback: { default: "" } });
      const options = commonOptions(raw);
      log = new Log(raw.rollback ? "platform-rollback" : "platform", options.logDir);
      const ops = boxOps(new Box(options.sshHost, log), options);
      if (raw.rollback) await runRollback(ops, log, options, raw.rollback);
      else await runPlatform(ops, log, options, repoIstioValues(), repoPins());
    },
  );
}
