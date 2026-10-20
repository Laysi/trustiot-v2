// The rules and the run of scripts/demo/platform.ts against a fake cluster, without ssh
// (Laysi/trustiot-v2#465).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type CommonOptions, Log, ROOT, confirmCode } from "./lib.ts";
import {
  type PlatformFacts,
  type PlatformOps,
  mergeValues,
  parseValuesContent,
  planPlatform,
  repoIstioValues,
  repoPins,
  runPlatform,
  runRollback,
} from "./platform.ts";

const pins = repoPins();
const desired = repoIstioValues();
const k3s = parseValuesContent(readFileSync(join(ROOT, "deploy/istio/values-k3s.yaml"), "utf8"));
/** istiod's values on the demo machine on 2026-10-20: the edge PoC's provider, no path normalization. */
const pocIstiod = mergeValues(k3s, {
  pilot: { resources: { requests: { cpu: "100m", memory: "256Mi" } } },
  meshConfig: { extensionProviders: [{ name: "identity", envoyExtAuthzHttp: { service: "identity.poc.svc.cluster.local", port: 8080, pathPrefix: "/check" } }] },
});

function labFacts(overrides: Partial<PlatformFacts> = {}): PlatformFacts {
  const istio = pins.istioVersion;
  return {
    charts: [
      { name: "istio-base", version: istio, values: {}, failed: false },
      { name: "istiod", version: istio, values: pocIstiod, failed: false },
      { name: "istio-cni", version: istio, values: k3s, failed: false },
      { name: "ztunnel", version: istio, values: k3s, failed: false },
    ],
    istioImages: {
      "deployment/istiod": `registry.istio.io/release/pilot:${istio}-distroless`,
      "daemonset/ztunnel": `registry.istio.io/release/ztunnel:${istio}-distroless`,
      "daemonset/istio-cni-node": `registry.istio.io/release/install-cni:${istio}-distroless`,
    },
    mesh: { extensionProviders: [{ name: "identity" }] },
    customPolicies: ["istio-ingress/edge-cookie-auth=identity"],
    overlays: [],
    gatewayCrds: {
      "gateways.gateway.networking.k8s.io": { bundleVersion: pins.gatewayApiVersion, channel: "standard" },
      "httproutes.gateway.networking.k8s.io": { bundleVersion: pins.gatewayApiVersion, channel: "standard" },
    },
    gatewayClass: { controller: "istio.io/gateway-controller", accepted: true },
    cnpg: { image: `ghcr.io/cloudnative-pg/cloudnative-pg:${pins.cnpgVersion}`, available: true, namespaceEnforce: "restricted", crds: ["clusters.postgresql.cnpg.io", "databases.postgresql.cnpg.io", "imagecatalogs.postgresql.cnpg.io"] },
    ...overrides,
  };
}

type Values = Record<string, unknown>;

/** A cluster in memory: patching istiod's HelmChart renders its mesh config, as helm-controller would. */
class FakeCluster implements PlatformOps {
  readonly calls: string[] = [];
  constructor(public state: PlatformFacts) {}

  async tunnel() {
    return { active: "active", pid: "4242" };
  }
  async facts() {
    return structuredClone(this.state);
  }
  async mesh() {
    return structuredClone(this.state.mesh);
  }
  async lock(step: string) {
    this.calls.push(`lock ${step}`);
    return async () => {
      this.calls.push("unlock");
    };
  }
  async clusterState() {
    return { "helmcharts.helm.cattle.io": { names: ["istiod"], sha256: JSON.stringify(this.state.charts) } };
  }
  async istiodChart() {
    const chart = this.state.charts.find((c) => c.name === "istiod")!;
    return { kind: "HelmChart", metadata: { name: "istiod", namespace: "kube-system" }, spec: { valuesContent: JSON.stringify(chart.values) } };
  }
  async setIstiodValues(values: Values) {
    this.calls.push(`patch istiod ${JSON.stringify(values)}`);
    this.state.charts = this.state.charts.map((c) => (c.name === "istiod" ? { ...c, values: structuredClone(values) } : c));
    const mesh = (values.meshConfig ?? {}) as { extensionProviders?: { name?: string }[]; pathNormalization?: { normalization?: string } };
    this.state.mesh = {
      extensionProviders: structuredClone(mesh.extensionProviders ?? []),
      ...(mesh.pathNormalization?.normalization === undefined ? {} : { pathNormalization: mesh.pathNormalization.normalization }),
    };
  }
  async rolloutIstiod() {
    this.calls.push("rollout istiod");
  }
  async sleep() {}

  get changes(): string[] {
    return this.calls.filter((c) => c.startsWith("lock") || c.startsWith("patch"));
  }
}

describe("platform.ts", () => {
  let dir: string;
  let log: Log;
  const options = (overrides: Partial<CommonOptions> = {}): CommonOptions => ({ sshHost: "box", apply: false, confirm: "", lockHeld: false, ...overrides });
  const afterCleanup = () => labFacts({ customPolicies: [] });

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "demo-platform-"));
    log = new Log("test", dir);
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  describe("pins and values", () => {
    test("the pins are the nightly's, which the platform READMEs name", () => {
      expect(readFileSync(join(ROOT, "deploy/istio/README.md"), "utf8")).toContain(`--version ${pins.istioVersion}`);
      expect(readFileSync(join(ROOT, "deploy/cloudnative-pg/README.md"), "utf8")).toContain(`cloudnative-pg:${pins.cnpgVersion}`);
      expect(pins.gatewayApiVersion).toMatch(/^v\d+\.\d+\.\d+$/);
    });

    test("merges values files the way Helm does: maps recursively, lists replaced", () => {
      expect(mergeValues({ a: { b: 1, c: [1, 2] }, d: 1 }, { a: { c: [3] }, e: 2 })).toEqual({ a: { b: 1, c: [3] }, d: 1, e: 2 });
    });

    test("istiod's values are values-k3s.yaml plus istiod-values.yaml: the chart's provider and path normalization", () => {
      const istiod = desired.istiod[0]!;
      const mesh = istiod.meshConfig as { extensionProviders: { name: string; envoyExtAuthzHttp: { service: string } }[]; pathNormalization: { normalization: string } };
      expect(mesh.extensionProviders.map((p) => p.name)).toEqual(["trustiot-identity"]);
      expect(mesh.extensionProviders[0]?.envoyExtAuthzHttp.service).toBe("trustiot-identity.trustiot.svc.cluster.local");
      expect(mesh.pathNormalization.normalization).toBe("DECODE_AND_MERGE_SLASHES");
      expect((istiod.global as { platform: string }).platform).toBe("k3s");
    });
  });

  describe("plan", () => {
    test("the lab as found: replaces istiod's values, blocked while the PoC's policy uses the old provider", () => {
      const plan = planPlatform(labFacts(), desired, pins);
      expect(plan.setValues).toEqual([{ name: "istiod", from: pocIstiod, to: desired.istiod[0]! }]);
      expect(plan.blockers).toEqual([expect.stringMatching(/istio-ingress\/edge-cookie-auth=identity.*cleanup\.ts/)]);
      expect(plan.differences).toEqual([]);
      const ready = planPlatform(afterCleanup(), desired, pins);
      expect(ready.blockers).toEqual([]);
      expect(ready.setValues).toEqual(plan.setValues);
    });

    test("blocks the change while a lab e2e test overlays istiod, or istiod is not at the pin or failed", () => {
      expect(planPlatform(labFacts({ customPolicies: [], overlays: ["istiod"] }), desired, pins).blockers).toEqual([expect.stringMatching(/HelmChartConfig kube-system\/istiod/)]);
      const old = labFacts({ customPolicies: [], charts: labFacts().charts.map((c) => (c.name === "istiod" ? { ...c, version: "1.29.0", failed: true } : c)) });
      expect(planPlatform(old, desired, pins).blockers).toEqual([expect.stringMatching(/istiod is at 1\.29\.0/), expect.stringMatching(/helm-install-istiod/)]);
    });

    test("aligned: nothing to change, but the mesh config must really show the values", () => {
      const charts = labFacts().charts.map((c) => (c.name === "istiod" ? { ...c, values: desired.istiod[0]! } : c));
      const shown = { extensionProviders: [{ name: "trustiot-identity" }], pathNormalization: "DECODE_AND_MERGE_SLASHES" };
      expect(planPlatform(labFacts({ charts, customPolicies: [], mesh: shown }), desired, pins)).toMatchObject({ setValues: [], blockers: [], differences: [] });
      expect(planPlatform(labFacts({ charts, customPolicies: [] }), desired, pins).differences).toEqual([expect.stringMatching(/does not show its values/)]);
    });

    test("reports, never changes, the other charts, images, CRDs and the operator", () => {
      const plan = planPlatform(
        labFacts({
          customPolicies: [],
          charts: labFacts().charts.map((c) => (c.name === "ztunnel" ? { ...c, version: "1.29.0", values: {} } : c)),
          istioImages: { ...labFacts().istioImages, "daemonset/ztunnel": "registry.istio.io/release/ztunnel:1.29.0-distroless" },
          gatewayCrds: { "gateways.gateway.networking.k8s.io": { bundleVersion: "v1.5.0", channel: "standard" } },
          gatewayClass: { accepted: false },
          cnpg: { image: "ghcr.io/cloudnative-pg/cloudnative-pg:1.29.0", available: true, namespaceEnforce: "baseline", crds: [] },
        }),
        desired,
        pins,
      );
      expect(plan.setValues.map((s) => s.name)).toEqual(["istiod"]);
      expect(plan.blockers).toEqual([]);
      const text = plan.differences.join("\n");
      for (const pattern of [/ztunnel is at 1\.29\.0/, /ztunnel has other values/, /ztunnel runs .*1\.29\.0/, /Gateway API v1\.5\.0, the pin is/, /httproutes.*missing/, /GatewayClass istio is not accepted/, /operator runs .*1\.29\.0/, /CRDs missing: clusters/, /restricted/]) {
        expect(text).toMatch(pattern);
      }
    });
  });

  describe("run", () => {
    test("a dry run before cleanup.ts gives the code that --apply takes after it", async () => {
      const cluster = new FakeCluster(labFacts());
      const before = await runPlatform(cluster, log, options(), desired, pins);
      expect(cluster.changes).toEqual([]);
      const code = confirmCode({ setValues: before.setValues });
      expect(await Bun.file(log.file).text()).toContain(`--apply --confirm ${code}`);
      expect(await Bun.file(log.file).text()).toContain("refuses until the 1 blockers above are gone");
      // Still blocked: --apply refuses before the lock and the patch.
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(/blockers above; nothing changed/);
      expect(cluster.changes).toEqual([]);
      // After cleanup.ts the same code applies.
      cluster.state.customPolicies = [];
      await runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins);
      expect(cluster.changes).toEqual(["lock platform", `patch istiod ${JSON.stringify(desired.istiod[0])}`]);
      expect(cluster.calls.slice(-2)).toEqual(["rollout istiod", "unlock"]);
      expect(cluster.state.mesh).toEqual({
        extensionProviders: (desired.istiod[0]!.meshConfig as { extensionProviders: unknown[] }).extensionProviders as { name?: string }[],
        pathNormalization: "DECODE_AND_MERGE_SLASHES",
      });
    });

    test("--apply without the code, or with another one, changes nothing", async () => {
      const cluster = new FakeCluster(afterCleanup());
      await expect(runPlatform(cluster, log, options({ apply: true }), desired, pins)).rejects.toThrow(/needs --confirm/);
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: "0123456789ab" }), desired, pins)).rejects.toThrow(/plan changed/);
      expect(cluster.changes).toEqual([]);
    });

    test("runs again: once istiod has the values there is nothing to change", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = confirmCode({ setValues: (await runPlatform(cluster, log, options(), desired, pins)).setValues });
      await runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins);
      const calls = cluster.calls.length;
      for (const again of [options(), options({ apply: true, confirm: code })]) {
        expect(await runPlatform(cluster, log, again, desired, pins)).toMatchObject({ setValues: [], blockers: [], differences: [] });
      }
      expect(cluster.calls.length).toBe(calls);
    });

    test("stops under the lock when a lab e2e test added its overlay after the plan was shown", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = confirmCode({ setValues: (await runPlatform(cluster, log, options(), desired, pins)).setValues });
      const lock = cluster.lock.bind(cluster);
      cluster.lock = async (step) => {
        cluster.state.overlays = ["istiod"];
        return lock(step);
      };
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(/blocked: HelmChartConfig/);
      expect(cluster.changes).toEqual(["lock platform"]);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("--apply saves the HelmChart first, and --rollback puts its values back", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = confirmCode({ setValues: (await runPlatform(cluster, log, options(), desired, pins)).setValues });
      await runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins);
      const saved = (await Bun.file(log.file).text()).match(/saved the HelmChart as it was: (\S+)/)?.[1];
      expect(saved).toBeDefined();
      expect(parseValuesContent((JSON.parse(readFileSync(saved!, "utf8")) as { spec: { valuesContent: string } }).spec.valuesContent)).toEqual(pocIstiod);

      const shown = await runRollback(cluster, log, options(), saved!);
      expect(shown).toEqual({ setValues: [{ name: "istiod", from: desired.istiod[0]!, to: pocIstiod }], blockers: [] });
      expect(cluster.state.charts.find((c) => c.name === "istiod")?.values).toEqual(desired.istiod[0]!);
      const rollbackCode = confirmCode({ rollback: shown.setValues });
      await expect(runRollback(cluster, log, options({ apply: true, confirm: confirmCode({ rollback: "istiod", to: pocIstiod }) }), saved!)).rejects.toThrow(/plan changed/);
      await runRollback(cluster, log, options({ apply: true, confirm: rollbackCode }), saved!);
      expect(cluster.state.charts.find((c) => c.name === "istiod")?.values).toEqual(pocIstiod);
      expect(cluster.state.mesh.extensionProviders.map((p) => p.name)).toEqual(["identity"]);
      expect(await runRollback(cluster, log, options({ apply: true, confirm: rollbackCode }), saved!)).toEqual({ setValues: [], blockers: [] });

      const other = join(dir, "other.json");
      writeFileSync(other, JSON.stringify({ kind: "HelmChart", metadata: { name: "ztunnel", namespace: "kube-system" } }));
      await expect(runRollback(cluster, log, options(), other)).rejects.toThrow(/not a saved HelmChart kube-system\/istiod/);
    });

    test("--rollback refuses to take a provider from a policy in use, and stops when istiod changed after the plan", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = confirmCode({ setValues: (await runPlatform(cluster, log, options(), desired, pins)).setValues });
      await runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins);
      const saved = (await Bun.file(log.file).text()).match(/saved the HelmChart as it was: (\S+)/)?.[1]!;
      const shown = await runRollback(cluster, log, options(), saved);
      const rollbackCode = confirmCode({ rollback: shown.setValues });

      // The demo's ingress now uses the provider that the saved values do not have.
      cluster.state.customPolicies = ["trustiot/trustiot-ingress=trustiot-identity"];
      await expect(runRollback(cluster, log, options({ apply: true, confirm: rollbackCode }), saved)).rejects.toThrow(/blockers above; nothing changed/);
      cluster.state.customPolicies = [];

      // Someone changed istiod's values after the rollback plan was shown.
      const lock = cluster.lock.bind(cluster);
      cluster.lock = async (step) => {
        cluster.state.charts = cluster.state.charts.map((c) => (c.name === "istiod" ? { ...c, values: { ...c.values, extra: true } } : c));
        return lock(step);
      };
      await expect(runRollback(cluster, log, options({ apply: true, confirm: rollbackCode }), saved)).rejects.toThrow(/changed since the rollback plan was made/);
      expect(cluster.calls.filter((c) => c.startsWith("patch"))).toHaveLength(1);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });
  });
});
