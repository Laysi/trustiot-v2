// The rules and the run of scripts/demo/platform.ts against a fake cluster, without ssh
// (Laysi/trustiot-v2#465).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunResult } from "../release/exec.ts";
import { Box, type CommonOptions, Log, ROOT, confirmCode, sha256, stableJson } from "./lib.ts";
import {
  APPLIED_ANNOTATION,
  type HelmChartFacts,
  type PlatformFacts,
  type PlatformOps,
  boxGateways,
  boxOps,
  helmChartFacts,
  istiodPatch,
  mergeValues,
  parseValuesContent,
  planPlatform,
  planRollback,
  repoIstioValues,
  repoPins,
  runPlatform,
  runRollback,
  valuesContentOf,
} from "./platform.ts";

const pins = repoPins();
const desired = repoIstioValues();
const desiredContent = valuesContentOf(desired.istiod[0]!);
const K3S_CONTENT = "profile: ambient\nglobal:\n  platform: k3s\n";
/** istiod's valuesContent on the demo machine on 2026-10-20: the edge PoC's provider, no path normalization. */
const POC_CONTENT = [
  "profile: ambient",
  "global:",
  "  platform: k3s",
  "pilot:",
  "  resources:",
  "    requests: {cpu: 100m, memory: 256Mi}",
  "meshConfig:",
  "  extensionProviders:",
  "    - name: identity",
  "      envoyExtAuthzHttp:",
  "        service: identity.poc.svc.cluster.local",
  "        port: 8080",
  "        timeout: 0.5s",
  "        failOpen: false",
  "        pathPrefix: /check",
  "        includeRequestHeadersInCheck: [cookie]",
  "        headersToUpstreamOnAllow: [x-internal-identity]",
].join("\n");

const chart = (name: string, valuesContent: string | undefined, overrides: Partial<HelmChartFacts> = {}): HelmChartFacts => ({
  name,
  version: pins.istioVersion,
  ...(valuesContent === undefined ? {} : { valuesContent }),
  failed: false,
  set: false,
  valuesSecrets: false,
  addon: false,
  ...overrides,
});

function labFacts(overrides: Partial<PlatformFacts> = {}): PlatformFacts {
  const istio = pins.istioVersion;
  return {
    charts: [chart("istio-base", ""), chart("istiod", POC_CONTENT), chart("istio-cni", K3S_CONTENT), chart("ztunnel", K3S_CONTENT)],
    istioImages: {
      "deployment/istiod": `registry.istio.io/release/pilot:${istio}-distroless`,
      "daemonset/ztunnel": `registry.istio.io/release/ztunnel:${istio}-distroless`,
      "daemonset/istio-cni-node": `registry.istio.io/release/install-cni:${istio}-distroless`,
    },
    mesh: { extensionProviders: [{ name: "identity" }] },
    customPolicies: ["istio-ingress/edge-cookie-auth=identity"],
    gateways: ["istio-ingress/edge=istio"],
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

/** The lab after cleanup.ts: no PoC policy, no Gateway. */
const afterCleanup = (overrides: Partial<PlatformFacts> = {}) => labFacts({ customPolicies: [], gateways: [], ...overrides });
const withIstiod = (facts: PlatformFacts, valuesContent: string | undefined, overrides: Partial<HelmChartFacts> = {}): PlatformFacts => ({
  ...facts,
  charts: facts.charts.map((c) => (c.name === "istiod" ? chart("istiod", valuesContent, overrides) : c)),
});

/** RFC 6902 `test`, `replace` and `add`, applied all or nothing like the API server does. */
function applyJsonPatch(target: Record<string, unknown>, patch: object[]): Record<string, unknown> {
  const doc = structuredClone(target);
  for (const op of patch as { op: string; path: string; value?: unknown }[]) {
    const keys = op.path
      .split("/")
      .slice(1)
      .map((key) => key.replaceAll("~1", "/").replaceAll("~0", "~"));
    const last = keys.pop()!;
    let parent: unknown = doc;
    for (const key of keys) parent = (parent as Record<string, unknown> | undefined)?.[key];
    if (parent === null || typeof parent !== "object") throw new Error(`the JSON patch failed: no parent for ${op.path}`);
    const object = parent as Record<string, unknown>;
    if (op.op === "test") {
      if (!(last in object) || stableJson(object[last]) !== stableJson(op.value)) throw new Error(`the JSON patch test failed: ${op.path}`);
    } else if (op.op === "replace") {
      if (!(last in object)) throw new Error(`the JSON patch failed: nothing to replace at ${op.path}`);
      object[last] = structuredClone(op.value);
    } else if (op.op === "add") {
      object[last] = structuredClone(op.value);
    } else {
      throw new Error(`unsupported JSON patch op ${op.op}`);
    }
  }
  return doc;
}

/**
 * A cluster in memory. HelmChart istiod is an object whose resourceVersion changes with its
 * content, and swapIstiodValues applies the JSON patch boxOps would send (istiodPatch) to it: a
 * failed `test` changes nothing. helm-controller renders the mesh config from the new values
 * unless `frozenMesh` is set.
 */
class FakeCluster implements PlatformOps {
  readonly calls: string[] = [];
  clock = 0;
  frozenMesh = false;
  /** Runs right after istiodChart() returns: a change made between the read and the patch. */
  afterIstiodChart: (() => void) | undefined;
  swapError: Error | undefined;
  /** Annotations on HelmChart istiod besides APPLIED_ANNOTATION. */
  otherAnnotations: Record<string, string> = {};

  constructor(public state: PlatformFacts) {}

  private istiod(): HelmChartFacts {
    return this.state.charts.find((c) => c.name === "istiod")!;
  }
  get content(): string | undefined {
    return this.istiod().valuesContent;
  }

  async tunnel() {
    return { active: "active", pid: "4242", restarts: "0", started: "t0" };
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
    return { "helmcharts.helm.cattle.io": { names: ["istiod"], sha256: sha256(JSON.stringify(this.state.charts)) } };
  }
  /** HelmChart istiod as the API server returns it; without annotations it has no annotations map. */
  private istiodObject(): Record<string, unknown> {
    const c = this.istiod();
    const annotations = { ...this.otherAnnotations, ...(c.appliedSha256 === undefined ? {} : { [APPLIED_ANNOTATION]: c.appliedSha256 }) };
    const metadata: Record<string, unknown> = { name: "istiod", namespace: "kube-system", ...(Object.keys(annotations).length === 0 ? {} : { annotations }) };
    const object = { kind: "HelmChart", metadata, spec: c.valuesContent === undefined ? {} : { valuesContent: c.valuesContent } };
    metadata.resourceVersion = sha256(stableJson(object)).slice(0, 8);
    return object;
  }
  async istiodChart() {
    const object = this.istiodObject();
    this.afterIstiodChart?.();
    this.afterIstiodChart = undefined;
    return object;
  }
  async swapIstiodValues(read: Record<string, unknown>, next: string) {
    if (this.swapError !== undefined) throw this.swapError;
    this.calls.push(`swap ${next}`);
    const patched = applyJsonPatch(this.istiodObject(), istiodPatch(read, next));
    const { [APPLIED_ANNOTATION]: applied, ...others } = (patched.metadata as { annotations?: Record<string, string> }).annotations ?? {};
    const c = this.istiod();
    c.valuesContent = (patched.spec as { valuesContent?: string }).valuesContent;
    c.appliedSha256 = applied;
    this.otherAnnotations = others;
    if (!this.frozenMesh) {
      const mesh = (parseValuesContent(next).meshConfig ?? {}) as { extensionProviders?: { name?: string }[]; pathNormalization?: { normalization?: string } };
      this.state.mesh = {
        extensionProviders: structuredClone(mesh.extensionProviders ?? []),
        ...(mesh.pathNormalization?.normalization === undefined ? {} : { pathNormalization: mesh.pathNormalization.normalization }),
      };
    }
  }
  async rolloutIstiod() {
    this.calls.push("rollout istiod");
  }
  now() {
    return this.clock;
  }
  async sleep(ms: number) {
    this.clock += ms;
  }

  get changes(): string[] {
    return this.calls.filter((c) => c.startsWith("lock") || c.startsWith("swap"));
  }
}

describe("platform.ts", () => {
  let dir: string;
  let log: Log;
  const options = (overrides: Partial<CommonOptions> = {}): CommonOptions => ({ sshHost: "box", apply: false, confirm: "", lockHeld: false, ...overrides });
  const codeOf = (plan: { setValues: unknown; gateways: unknown }) => confirmCode({ setValues: plan.setValues, gateways: plan.gateways });
  const rollbackCodeOf = (plan: { setValues: unknown; gateways: unknown }) => confirmCode({ rollback: { setValues: plan.setValues, gateways: plan.gateways } });
  /** A dry run, then --apply with its code; returns the path of the saved HelmChart. */
  const applyAll = async (cluster: FakeCluster) => {
    const code = codeOf(await runPlatform(cluster, log, options(), desired, pins));
    await runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins);
    return (await Bun.file(log.file).text()).match(/saved the HelmChart as it was: (\S+)/)![1]!;
  };

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

    test("reads spec.set, spec.valuesSecrets, k3s AddOn annotations and the applied-values annotation from a HelmChart", () => {
      const facts = helmChartFacts("istiod", {
        metadata: { annotations: { "objectset.rio.cattle.io/hash": "x", [APPLIED_ANNOTATION]: "abc" } },
        spec: { version: "1.30.5", valuesContent: "a: 1\n", set: { "global.hub": "x" }, valuesSecrets: [{ name: "s" }] },
        status: { conditions: [{ type: "Failed", status: "True" }] },
      });
      expect(facts).toEqual({ name: "istiod", version: "1.30.5", valuesContent: "a: 1\n", failed: true, set: true, valuesSecrets: true, addon: true, appliedSha256: "abc" });
      expect(helmChartFacts("istiod", { metadata: {}, spec: { set: {}, valuesSecrets: [] } })).toMatchObject({ set: false, valuesSecrets: false, addon: false });
    });
  });

  describe("plan", () => {
    test("the lab as found: replaces istiod's values, blocked while the PoC's policy uses the old provider", () => {
      const plan = planPlatform(labFacts(), desired, pins);
      expect(plan.setValues).toEqual([{ name: "istiod", from: parseValuesContent(POC_CONTENT), to: desired.istiod[0]!, fromSha256: sha256(POC_CONTENT), toSha256: sha256(desiredContent) }]);
      expect(plan.blockers).toEqual([expect.stringMatching(/istio-ingress\/edge-cookie-auth=identity.*cleanup\.ts/)]);
      expect(plan.gateways).toEqual(["istio-ingress/edge=istio"]);
      expect(plan.differences).toEqual([]);
      const ready = planPlatform(afterCleanup(), desired, pins);
      expect(ready.blockers).toEqual([]);
      expect(ready.setValues).toEqual(plan.setValues);
    });

    test("blocks the change while a lab e2e test overlays istiod, or istiod is not at the pin or failed", () => {
      expect(planPlatform(afterCleanup({ overlays: ["istiod"] }), desired, pins).blockers).toEqual([expect.stringMatching(/HelmChartConfig kube-system\/istiod/)]);
      const old = withIstiod(afterCleanup(), POC_CONTENT, { version: "1.29.0", failed: true });
      expect(planPlatform(old, desired, pins).blockers).toEqual([expect.stringMatching(/istiod is at 1\.29\.0/), expect.stringMatching(/helm-install-istiod/)]);
    });

    test("blocks the change while something else also sets istiod's values: spec.set, spec.valuesSecrets, a k3s AddOn", () => {
      for (const [override, pattern] of [
        [{ set: true }, /spec\.set/],
        [{ valuesSecrets: true }, /spec\.valuesSecrets/],
        [{ addon: true }, /k3s AddOn/],
      ] as const) {
        expect(planPlatform(withIstiod(afterCleanup(), POC_CONTENT, override), desired, pins).blockers).toEqual([expect.stringMatching(pattern)]);
      }
      expect(planPlatform(withIstiod(afterCleanup(), undefined), desired, pins).blockers).toEqual([expect.stringMatching(/no spec\.valuesContent/)]);
    });

    test("lists every Gateway and waypoint, and a new one changes the confirmation code the dry run prints", async () => {
      const printed = async (facts: PlatformFacts) => {
        const plan = await runPlatform(new FakeCluster(facts), log, options(), desired, pins);
        const codes = [...(await Bun.file(log.file).text()).matchAll(/--apply --confirm ([0-9a-f]{12})/g)];
        return { plan, code: codes.at(-1)![1] };
      };
      const without = await printed(afterCleanup());
      const waypoint = await printed(afterCleanup({ gateways: ["trustiot/waypoint=istio-waypoint"] }));
      expect(waypoint.plan.gateways).toEqual(["trustiot/waypoint=istio-waypoint"]);
      expect(await Bun.file(log.file).text()).toContain("a mesh-wide change applies to them): trustiot/waypoint=istio-waypoint");
      expect(waypoint.code).not.toBe(without.code);
    });

    test("aligned: nothing to change, but the mesh config must really show the values", () => {
      const aligned = withIstiod(afterCleanup(), desiredContent);
      const shown = { extensionProviders: [{ name: "trustiot-identity" }], pathNormalization: "DECODE_AND_MERGE_SLASHES" };
      expect(planPlatform({ ...aligned, mesh: shown }, desired, pins)).toMatchObject({ setValues: [], blockers: [], differences: [] });
      expect(planPlatform(aligned, desired, pins).differences).toEqual([expect.stringMatching(/does not show its values/)]);
    });

    test("reports, never changes, the other charts, images, CRDs and the operator", () => {
      const plan = planPlatform(
        afterCleanup({
          charts: labFacts().charts.map((c) => (c.name === "ztunnel" ? chart("ztunnel", "", { version: "1.29.0" }) : c)),
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

  describe("rollback plan", () => {
    const applied = (facts: PlatformFacts) => withIstiod(facts, desiredContent, { appliedSha256: sha256(desiredContent) });
    /** Saved values that keep the provider but drop the path normalization. */
    const withoutNormalization = valuesContentOf(mergeValues(parseValuesContent(K3S_CONTENT), { meshConfig: { extensionProviders: (desired.istiod[0]!.meshConfig as { extensionProviders: unknown[] }).extensionProviders } }));

    test("refuses to drop the path normalization while a Gateway or waypoint exists", () => {
      const plan = planRollback(applied(afterCleanup({ customPolicies: ["trustiot/x=trustiot-identity"], gateways: ["trustiot/trustiot=istio"] })), withoutNormalization);
      expect(plan.blockers).toEqual([expect.stringMatching(/change the mesh's path normalization \(DECODE_AND_MERGE_SLASHES -> default\) while Gateways or waypoints exist \(trustiot\/trustiot=istio\)/)]);
      expect(planRollback(applied(afterCleanup()), withoutNormalization).blockers).toEqual([]);
    });

    test("refuses when istiod's values are not what the last --apply wrote", () => {
      expect(planRollback(withIstiod(afterCleanup(), desiredContent), POC_CONTENT).blockers).toEqual([expect.stringMatching(/not what the last --apply of this script wrote \(no trustiot\.io\/demo-platform-values-sha256\)/)]);
      expect(planRollback(withIstiod(afterCleanup(), desiredContent, { appliedSha256: sha256("older") }), POC_CONTENT).blockers).toEqual([expect.stringMatching(/they changed since/)]);
    });

    test("refuses to take a provider away from a policy in use, and has nothing to do when the values are already the saved ones", () => {
      expect(planRollback(applied(afterCleanup({ customPolicies: ["trustiot/ingress=trustiot-identity"] })), POC_CONTENT).blockers).toEqual([expect.stringMatching(/providers the saved values do not have \(trustiot\/ingress=trustiot-identity\)/)]);
      expect(planRollback(applied(afterCleanup()), desiredContent)).toEqual({ setValues: [], gateways: [], blockers: [] });
    });
  });

  describe("run", () => {
    test("a blocker that goes away keeps the code: --apply refuses while the PoC's policy remains, and applies once it is gone", async () => {
      // The PoC's Gateway is gone already (cleanup.ts deletes it first); only its policy remains.
      const cluster = new FakeCluster(labFacts({ gateways: [] }));
      const before = await runPlatform(cluster, log, options(), desired, pins);
      expect(cluster.changes).toEqual([]);
      const code = codeOf(before);
      expect(await Bun.file(log.file).text()).toContain(`--apply --confirm ${code}`);
      expect(await Bun.file(log.file).text()).toContain("refuses until the 1 blockers above are gone");
      // Still blocked: --apply refuses before the lock and the patch.
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(/blockers above; nothing changed/);
      expect(cluster.changes).toEqual([]);
      // Once the policy is gone the same code applies.
      cluster.state.customPolicies = [];
      await runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins);
      expect(cluster.changes).toEqual(["lock platform", `swap ${desiredContent}`]);
      expect(cluster.calls.slice(-2)).toEqual(["rollout istiod", "unlock"]);
      expect(cluster.content).toBe(desiredContent);
      expect(cluster.state.charts.find((c) => c.name === "istiod")?.appliedSha256).toBe(sha256(desiredContent));
      expect(cluster.state.mesh.pathNormalization).toBe("DECODE_AND_MERGE_SLASHES");
    });

    test("--apply without the code, or with another one, changes nothing", async () => {
      const cluster = new FakeCluster(afterCleanup());
      await expect(runPlatform(cluster, log, options({ apply: true }), desired, pins)).rejects.toThrow(/needs --confirm/);
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: "0123456789ab" }), desired, pins)).rejects.toThrow(/plan changed/);
      expect(cluster.changes).toEqual([]);
    });

    test("runs again: once istiod has the values there is nothing to change", async () => {
      const cluster = new FakeCluster(afterCleanup());
      await applyAll(cluster);
      const calls = cluster.calls.length;
      for (const again of [options(), options({ apply: true, confirm: "0123456789ab" })]) {
        expect(await runPlatform(cluster, log, again, desired, pins)).toMatchObject({ setValues: [], blockers: [], differences: [] });
      }
      expect(cluster.calls.length).toBe(calls);
    });

    test("stops under the lock when a lab e2e test added its overlay after the plan was shown", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = codeOf(await runPlatform(cluster, log, options(), desired, pins));
      const lock = cluster.lock.bind(cluster);
      cluster.lock = async (step) => {
        cluster.state.overlays = ["istiod"];
        return lock(step);
      };
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(/blocked: HelmChartConfig/);
      expect(cluster.changes).toEqual(["lock platform"]);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("a change between reading the HelmChart and patching it fails the patch and overwrites nothing", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = codeOf(await runPlatform(cluster, log, options(), desired, pins));
      cluster.afterIstiodChart = () => {
        cluster.state.charts.find((c) => c.name === "istiod")!.valuesContent = "someone: else\n";
      };
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(/JSON patch test failed/);
      expect(cluster.content).toBe("someone: else\n");
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("an annotation added after the HelmChart was read without any fails the patch and stays", async () => {
      const cluster = new FakeCluster(afterCleanup());
      expect((await cluster.istiodChart()).metadata).not.toHaveProperty("annotations");
      const code = codeOf(await runPlatform(cluster, log, options(), desired, pins));
      const before = cluster.content;
      cluster.afterIstiodChart = () => {
        cluster.otherAnnotations = { "example.com/owner": "someone" };
      };
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(
        "the JSON patch test failed: /metadata/resourceVersion",
      );
      expect(cluster.otherAnnotations).toEqual({ "example.com/owner": "someone" });
      expect(cluster.content).toBe(before);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("the mesh config that never shows the values times out after 600 s and releases the lock", async () => {
      const cluster = new FakeCluster(afterCleanup());
      cluster.frozenMesh = true;
      const code = codeOf(await runPlatform(cluster, log, options(), desired, pins));
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow(/timed out after 600 s waiting for istiod's mesh config/);
      // One 5 s poll past the 600 s deadline, not more.
      expect(cluster.clock).toBe(605_000);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("a failing patch still releases the lock and reports the ops tunnel", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const code = codeOf(await runPlatform(cluster, log, options(), desired, pins));
      cluster.swapError = new Error("connection refused");
      await expect(runPlatform(cluster, log, options({ apply: true, confirm: code }), desired, pins)).rejects.toThrow("connection refused");
      expect(cluster.calls.at(-1)).toBe("unlock");
      expect((await Bun.file(log.file).text()).trimEnd().split("\n").at(-1)).toContain("cloudflared.service kept running (same process)");
    });

    test("--rollback puts the saved valuesContent back byte for byte", async () => {
      const original = "global:\n  tag: 1.30\n";
      const cluster = new FakeCluster(withIstiod(afterCleanup(), original));
      const saved = await applyAll(cluster);
      expect((JSON.parse(readFileSync(saved, "utf8")) as { spec: { valuesContent: string } }).spec.valuesContent).toBe(original);

      const shown = await runRollback(cluster, log, options(), saved);
      expect(shown.setValues[0]).toMatchObject({ fromSha256: sha256(desiredContent), toSha256: sha256(original) });
      expect(cluster.content).toBe(desiredContent);
      await expect(runRollback(cluster, log, options({ apply: true, confirm: "0123456789ab" }), saved)).rejects.toThrow(/plan changed/);
      await runRollback(cluster, log, options({ apply: true, confirm: rollbackCodeOf(shown) }), saved);
      expect(cluster.content).toBe(original);
      expect(await runRollback(cluster, log, options({ apply: true, confirm: rollbackCodeOf(shown) }), saved)).toEqual({ setValues: [], gateways: [], blockers: [] });

      const other = join(dir, "other.json");
      writeFileSync(other, JSON.stringify({ kind: "HelmChart", metadata: { name: "ztunnel", namespace: "kube-system" }, spec: { valuesContent: "" } }));
      await expect(runRollback(cluster, log, options(), other)).rejects.toThrow(/not a saved HelmChart kube-system\/istiod/);
    });

    test("--rollback refuses to take a provider from a policy in use, and stops when istiod changed after the plan", async () => {
      const cluster = new FakeCluster(afterCleanup());
      const saved = await applyAll(cluster);
      const shown = await runRollback(cluster, log, options(), saved);
      const code = rollbackCodeOf(shown);

      cluster.state.customPolicies = ["trustiot/trustiot-ingress=trustiot-identity"];
      await expect(runRollback(cluster, log, options({ apply: true, confirm: code }), saved)).rejects.toThrow(/blockers above; nothing changed/);
      cluster.state.customPolicies = [];

      const lock = cluster.lock.bind(cluster);
      cluster.lock = async (step) => {
        const istiod = cluster.state.charts.find((c) => c.name === "istiod")!;
        istiod.valuesContent = `${istiod.valuesContent}# edited\n`;
        return lock(step);
      };
      await expect(runRollback(cluster, log, options({ apply: true, confirm: code }), saved)).rejects.toThrow(/changed since the rollback plan was made/);
      expect(cluster.calls.filter((c) => c.startsWith("swap"))).toHaveLength(1);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });
  });

  describe("boxOps over ssh", () => {
    test("the patch is a JSON patch on HelmChart kube-system/istiod: test the resourceVersion and the old text, replace it, record what was written", async () => {
      const commands: string[] = [];
      const run = async (cmd: string[]): Promise<RunResult> => {
        commands.push(cmd.at(-1)!);
        return { stdout: "", stderr: "", exitCode: 0 };
      };
      const ops = boxOps(new Box("demo-box", log, { run, sleep: async () => {} }), options());
      await ops.swapIstiodValues({ metadata: { resourceVersion: "4711", annotations: { a: "b" } }, spec: { valuesContent: "old: 1\n" } }, "new: 2\n");
      const command = commands[0]!;
      expect(command.startsWith("sudo k3s kubectl -n kube-system patch helmcharts.helm.cattle.io istiod --type json --patch '")).toBe(true);
      const patch = JSON.parse(command.slice(command.indexOf("--patch '") + "--patch '".length, -1)) as unknown;
      expect(patch).toEqual([
        { op: "test", path: "/metadata/resourceVersion", value: "4711" },
        { op: "test", path: "/spec/valuesContent", value: "old: 1\n" },
        { op: "replace", path: "/spec/valuesContent", value: "new: 2\n" },
        { op: "add", path: "/metadata/annotations/trustiot.io~1demo-platform-values-sha256", value: sha256("new: 2\n") },
      ]);
      expect(istiodPatch({ metadata: { resourceVersion: "4711" }, spec: { valuesContent: "old: 1\n" } }, "new: 2\n")[3]).toEqual({
        op: "add",
        path: "/metadata/annotations",
        value: { [APPLIED_ANNOTATION]: sha256("new: 2\n") },
      });
      await expect(ops.swapIstiodValues({ metadata: { resourceVersion: "4711" }, spec: {} }, "new: 2\n")).rejects.toThrow(/no spec\.valuesContent/);
      await expect(ops.swapIstiodValues({ metadata: {}, spec: { valuesContent: "old: 1\n" } }, "new: 2\n")).rejects.toThrow(/no metadata\.resourceVersion/);
      expect(commands).toHaveLength(1);
    });

    test("the Gateway list holds Gateway API and Istio's own Gateways, and reads a kind only while its CRD is installed", async () => {
      const box = (crds: string[]) => {
        const commands: string[] = [];
        const run = async (cmd: string[]): Promise<RunResult> => {
          const command = cmd.at(-1)!;
          commands.push(command);
          const crd = /get customresourcedefinitions\.apiextensions\.k8s\.io (\S+) -o json$/.exec(command)?.[1];
          if (crd !== undefined) {
            return crds.includes(crd)
              ? { stdout: JSON.stringify({ metadata: { name: crd } }), stderr: "", exitCode: 0 }
              : { stdout: "", stderr: `Error from server (NotFound): customresourcedefinitions.apiextensions.k8s.io "${crd}" not found`, exitCode: 1 };
          }
          const item = (namespace: string, name: string, spec: Record<string, unknown>) => ({ metadata: { namespace, name }, spec });
          if (command.includes("get gateways.gateway.networking.k8s.io -A")) {
            return { stdout: JSON.stringify({ items: [item("trustiot", "waypoint", { gatewayClassName: "istio-waypoint" }), item("istio-ingress", "edge", { gatewayClassName: "istio" })] }), stderr: "", exitCode: 0 };
          }
          if (command.includes("get gateways.networking.istio.io -A")) {
            return { stdout: JSON.stringify({ items: [item("istio-system", "ingressgateway", { selector: { istio: "ingressgateway" } })] }), stderr: "", exitCode: 0 };
          }
          return { stdout: "", stderr: "unexpected", exitCode: 1 };
        };
        return { box: new Box("demo-box", log, { run, sleep: async () => {} }), commands };
      };
      const both = box(["gateways.gateway.networking.k8s.io", "gateways.networking.istio.io"]);
      expect(await boxGateways(both.box)).toEqual(["istio-ingress/edge=istio", "istio-system/ingressgateway=networking.istio.io", "trustiot/waypoint=istio-waypoint"]);
      const istioOnly = box(["gateways.networking.istio.io"]);
      expect(await boxGateways(istioOnly.box)).toEqual(["istio-system/ingressgateway=networking.istio.io"]);
      expect(istioOnly.commands.filter((c) => c.includes("get gateways.gateway.networking.k8s.io"))).toEqual([]);
    });

    test("a patch whose ssh connection drops is never sent again; reading the HelmChart back tells whether it went through", async () => {
      const read = { metadata: { resourceVersion: "4711" }, spec: { valuesContent: "old: 1\n" } };
      const dropped = (holds: string, annotation: string | undefined) => {
        const commands: string[] = [];
        const run = async (cmd: string[]): Promise<RunResult> => {
          const command = cmd.at(-1)!;
          commands.push(command);
          if (command.includes(" patch ")) return { stdout: "", stderr: "Connection reset by peer", exitCode: 255 };
          const metadata = { resourceVersion: "4712", ...(annotation === undefined ? {} : { annotations: { [APPLIED_ANNOTATION]: annotation } }) };
          return { stdout: JSON.stringify({ metadata, spec: { valuesContent: holds } }), stderr: "", exitCode: 0 };
        };
        return { ops: boxOps(new Box("demo-box", log, { run, sleep: async () => {} }), options()), commands };
      };
      const through = dropped("new: 2\n", sha256("new: 2\n"));
      await through.ops.swapIstiodValues(read, "new: 2\n");
      expect(through.commands.filter((c) => c.includes(" patch "))).toHaveLength(1);
      expect(through.commands.at(-1)).toContain("get helmcharts.helm.cattle.io istiod -o json");
      const lost = dropped("old: 1\n", undefined);
      await expect(lost.ops.swapIstiodValues(read, "new: 2\n")).rejects.toThrow("ssh dropped the connection during the patch and HelmChart istiod does not hold the new values");
      expect(lost.commands.filter((c) => c.includes(" patch "))).toHaveLength(1);
    });
  });
});
