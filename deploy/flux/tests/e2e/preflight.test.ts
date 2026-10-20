// The Flux lab e2e test's decisions before and after it changes the cluster (lab.e2e.ts uses these
// for its refusals and its teardown). Run with `bun test deploy/flux`.
import { describe, expect, test } from "bun:test";
import {
  type NamespaceApi,
  RUN_LABEL,
  RunNamespace,
  type TeardownStep,
  existingFluxProblem,
  fluxCrdNames,
  issueOfNamespace,
  lockProblem,
  newRunId,
  ownsNamespace,
  runLabelProblem,
  teardownSteps,
} from "./preflight.ts";

describe("lab.e2e.ts preflight", () => {
  test("a cluster without Flux may be used", () => {
    expect(existingFluxProblem({ fluxSystemNamespace: false, fluxCrds: [] })).toBeUndefined();
  });

  test("refuses a cluster whose flux-system namespace exists, and says why", () => {
    const problem = existingFluxProblem({ fluxSystemNamespace: true, fluxCrds: [] });
    expect(problem).toStartWith("namespace flux-system already exists");
    expect(problem).toContain("removes it, CRDs included");
    expect(problem).toContain("demo environment");
    expect(problem).toContain("refuses to run");
  });

  test("refuses a cluster with Flux CRDs but no flux-system namespace, naming them", () => {
    const crds = fluxCrdNames(
      [
        "customresourcedefinition.apiextensions.k8s.io/gateways.gateway.networking.k8s.io",
        "customresourcedefinition.apiextensions.k8s.io/helmreleases.helm.toolkit.fluxcd.io",
        "customresourcedefinition.apiextensions.k8s.io/clusters.postgresql.cnpg.io",
        "customresourcedefinition.apiextensions.k8s.io/gitrepositories.source.toolkit.fluxcd.io",
        "",
      ].join("\n"),
    );
    expect(crds).toEqual([
      "customresourcedefinition.apiextensions.k8s.io/helmreleases.helm.toolkit.fluxcd.io",
      "customresourcedefinition.apiextensions.k8s.io/gitrepositories.source.toolkit.fluxcd.io",
    ]);
    const problem = existingFluxProblem({ fluxSystemNamespace: false, fluxCrds: crds });
    expect(problem).toStartWith("Flux CRDs already exist (customresourcedefinition.apiextensions.k8s.io/helmreleases.helm.toolkit.fluxcd.io, ");
    expect(problem).toContain("refuses to run");
  });

  test("other CRDs (Gateway API, CloudNativePG) are not Flux", () => {
    expect(fluxCrdNames("customresourcedefinition.apiextensions.k8s.io/clusters.postgresql.cnpg.io\n")).toEqual([]);
  });
});

describe("the lab lock", () => {
  const issue = issueOfNamespace("e2e-457");

  test("the issue comes from E2E_NAMESPACE", () => {
    expect(issue).toBe("Laysi/trustiot-v2#457");
    for (const bad of ["e2e-", "e2e-0457", "e2e-457x", "flux-e2e-457", "e2e-45 7"]) {
      expect(() => issueOfNamespace(bad)).toThrow(/E2E_NAMESPACE must be e2e-<issue number>/);
    }
  });

  test("a lock this issue holds may be used", () => {
    expect(lockProblem("Laysi/trustiot-v2#457 2026-10-20T08:00:00Z\n", issue)).toBeUndefined();
    expect(lockProblem("Laysi/trustiot-v2#457 2026-10-20T08:00:00Z", issue)).toBeUndefined();
  });

  test("refuses another issue's lock, a longer issue number, a missing or empty owner file and extra lines", () => {
    for (const owner of [
      "Laysi/trustiot-v2#464 2026-10-20T08:00:00Z\n",
      "Laysi/trustiot-v2#4570 2026-10-20T08:00:00Z\n",
      "Laysi/trustiot-v2#457\n",
      "#457 2026-10-20T08:00:00Z\n",
      "",
      "\n",
      "Laysi/trustiot-v2#457 2026-10-20T08:00:00Z\nLaysi/trustiot-v2#464 2026-10-20T08:01:00Z\n",
    ]) {
      expect(lockProblem(owner, issue)).toMatch(/held by someone else/);
    }
    expect(lockProblem(undefined, issue)).toMatch(/is not held/);
  });
});

describe("owning a namespace (the test namespace and flux-system)", () => {
  const runId = newRunId();
  const other = newRunId();

  test("a run ID is a label value no other run shares", () => {
    expect(runId).toMatch(/^run-[0-9a-f]{32}$/);
    expect(other).not.toBe(runId);
    expect(new RunNamespace("flux-system", runId).manifest()).toBe(`apiVersion: v1\nkind: Namespace\nmetadata:\n  name: flux-system\n  labels:\n    ${RUN_LABEL}: ${runId}\n`);
    expect(new RunNamespace("e2e-464", runId).manifest()).toContain("  name: e2e-464\n");
  });

  test("decides ownership from the create and, when its result is unknown, from the label", () => {
    const alreadyExists = 'Error from server (AlreadyExists): namespaces "flux-system" already exists';
    expect(ownsNamespace({ createExitCode: 0, createOutput: "namespace/flux-system created", labelOnCluster: runId, runId })).toBe(true);
    expect(ownsNamespace({ createExitCode: 1, createOutput: alreadyExists, labelOnCluster: undefined, runId })).toBe(false);
    expect(ownsNamespace({ createExitCode: 1, createOutput: alreadyExists, labelOnCluster: runId, runId })).toBe(false);
    expect(ownsNamespace({ createExitCode: 255, createOutput: "Connection reset by peer", labelOnCluster: other, runId })).toBe(false);
    expect(ownsNamespace({ createExitCode: 255, createOutput: "Connection reset by peer", labelOnCluster: undefined, runId })).toBe(false);
    expect(ownsNamespace({ createExitCode: 255, createOutput: "Connection reset by peer", labelOnCluster: runId, runId })).toBe(true);
    expect(ownsNamespace({ createExitCode: -1, createOutput: "timed out", labelOnCluster: runId, runId })).toBe(true);
  });

  /** A cluster where namespaces may already exist with some label, and creates may lose their answer. */
  function cluster(initial: Record<string, { label?: string }>, behaviour: { loseAnswer?: boolean; throwOnCreate?: boolean } = {}) {
    const namespaces: Record<string, { label?: string }> = { ...initial };
    const calls: string[] = [];
    const api: NamespaceApi = {
      async create(manifest) {
        const name = /\n {2}name: (\S+)\n/.exec(manifest)?.[1] ?? "";
        calls.push(`create ${name}`);
        if (behaviour.throwOnCreate) throw new Error("ssh: connect to host: Connection timed out");
        if (name in namespaces) return { exitCode: 1, output: `Error from server (AlreadyExists): namespaces "${name}" already exists` };
        namespaces[name] = { label: new RegExp(`${RUN_LABEL}: (\\S+)`).exec(manifest)?.[1] };
        return behaviour.loseAnswer ? { exitCode: 255, output: "Connection to host closed by remote host." } : { exitCode: 0, output: `namespace/${name} created` };
      },
      async runLabel(name) {
        calls.push(`label ${name}`);
        return namespaces[name]?.label;
      },
    };
    return { api, calls, namespaces };
  }

  test("a namespace that someone else created is never claimed, and teardown leaves it", async () => {
    for (const name of ["flux-system", "e2e-464"]) {
      for (const label of [undefined, other]) {
        const { api } = cluster({ [name]: { label } });
        const namespace = new RunNamespace(name, runId);
        await expect(namespace.claim(api)).rejects.toThrow(new RegExp(`namespace ${name} is not this run's .*AlreadyExists.*nothing is installed in it or removed from it`));
        expect(namespace.owned).toBe(false);
        expect(await namespace.mayTearDown(api)).toBe(false);
      }
    }
  });

  test("a create that succeeds, or whose answer is lost after it took effect, is this run's", async () => {
    for (const name of ["flux-system", "e2e-464"]) {
      for (const loseAnswer of [false, true]) {
        const { api, calls } = cluster({}, { loseAnswer });
        const namespace = new RunNamespace(name, runId);
        await namespace.claim(api);
        expect(namespace.owned).toBe(true);
        expect(await namespace.mayTearDown(api)).toBe(true);
        // The label is read on the namespace itself, never on another one.
        expect(calls.filter((c) => c.startsWith("label ")).every((c) => c === `label ${name}`)).toBe(true);
      }
    }
  });

  test("a create that fails before reaching the cluster owns nothing", async () => {
    const { api } = cluster({}, { throwOnCreate: true });
    const namespace = new RunNamespace("e2e-464", runId);
    await expect(namespace.claim(api)).rejects.toThrow(/not this run's/);
    expect(namespace.owned).toBe(false);
  });

  test("teardown deletes nothing when the label changed or the namespace is gone", async () => {
    const { api, namespaces } = cluster({});
    const namespace = new RunNamespace("flux-system", runId);
    await namespace.claim(api);
    namespaces["flux-system"] = { label: other };
    expect(await namespace.mayTearDown(api)).toBe(false);
    delete namespaces["flux-system"];
    expect(await namespace.mayTearDown(api)).toBe(false);
    const unreadable: NamespaceApi = { ...api, runLabel: async () => Promise.reject(new Error("ssh failed")) };
    namespaces["flux-system"] = { label: runId };
    expect(await namespace.mayTearDown(unreadable)).toBe(false);
  });

  test("after Flux applied itself, a lost or changed label stops the run and says what to remove by hand", async () => {
    const cleanup = "remove this run's Flux by hand";
    expect(runLabelProblem("flux-system", runId, runId, cleanup)).toBeUndefined();
    for (const [label, found] of [
      [undefined, "the label is gone"],
      [other, "another run's ID"],
    ] as const) {
      const problem = runLabelProblem("flux-system", label, runId, cleanup);
      expect(problem).toStartWith(`namespace flux-system no longer carries ${RUN_LABEL}=${runId}`);
      expect(problem).toContain(found);
      expect(problem).toContain("teardown will not delete it");
      expect(problem).toEndWith(cleanup);
    }
    const { api, namespaces } = cluster({});
    const namespace = new RunNamespace("flux-system", runId);
    await namespace.claim(api);
    await namespace.requireStillOurs(api, cleanup);
    // Server-side apply of gotk-components, or Flux's own reconcile, dropped the label.
    namespaces["flux-system"] = {};
    await expect(namespace.requireStillOurs(api, cleanup)).rejects.toThrow(/no longer carries .* remove this run's Flux by hand$/);
    const unreadable: NamespaceApi = { ...api, runLabel: async () => Promise.reject(new Error("ssh failed")) };
    namespaces["flux-system"] = { label: runId };
    await expect(namespace.requireStillOurs(unreadable, cleanup)).rejects.toThrow(/the label is gone/);
  });
});

describe("teardown", () => {
  const ns = "e2e-464";
  const all = teardownSteps({ fluxOurs: true, testOurs: true, testNamespace: ns, fluxManifests: true });
  /** Whether a step deletes or changes anything of Flux: its namespace, its objects or its manifests (CRDs included). */
  const touchesFlux = (step: TeardownStep) => step.fluxManifests === true || step.args.includes("flux-system");
  const touchesTest = (step: TeardownStep) => step.args.includes(ns);

  test("with both namespaces this run's, it removes everything in the order Flux needs", () => {
    expect(all.map((step) => step.what)).toEqual([
      "suspend the sync, no pruning",
      "uninstall the release",
      "delete the sources",
      "delete the sync",
      "delete Flux",
      `delete namespace ${ns}`,
      "delete namespace flux-system",
    ]);
    expect(all.filter((step) => step.fluxManifests).map((step) => step.args)).toEqual([["delete", "--ignore-not-found", "--wait", "--timeout=300s", "-f", "-"]]);
  });

  test("when Flux is not this run's, nothing of Flux, its CRDs or flux-system is deleted", () => {
    for (const testOurs of [true, false]) {
      for (const fluxManifests of [true, false]) {
        const steps = teardownSteps({ fluxOurs: false, testOurs, testNamespace: ns, fluxManifests });
        expect(steps.filter(touchesFlux)).toEqual([]);
        // The release and its sources need helm-controller and source-controller: not without our Flux.
        expect(steps.map((step) => step.what)).toEqual(testOurs ? [`delete namespace ${ns}`] : []);
      }
    }
  });

  test("when the test namespace is not this run's, it is not deleted and nothing in it is touched", () => {
    for (const fluxOurs of [true, false]) {
      const steps = teardownSteps({ fluxOurs, testOurs: false, testNamespace: ns, fluxManifests: true });
      expect(steps.filter(touchesTest)).toEqual([]);
    }
    expect(teardownSteps({ fluxOurs: true, testOurs: false, testNamespace: ns, fluxManifests: true }).map((step) => step.what)).toEqual([
      "suspend the sync, no pruning",
      "delete the sync",
      "delete Flux",
      "delete namespace flux-system",
    ]);
  });

  test("Flux's manifests are deleted only when the run got as far as rendering them", () => {
    const steps = teardownSteps({ fluxOurs: true, testOurs: true, testNamespace: ns, fluxManifests: false });
    expect(steps.some((step) => step.fluxManifests)).toBe(false);
    expect(steps.map((step) => step.what)).toEqual(all.map((step) => step.what).filter((what) => what !== "delete Flux"));
  });
});
