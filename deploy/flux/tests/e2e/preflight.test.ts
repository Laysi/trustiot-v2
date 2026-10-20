// The Flux lab e2e test's decisions before and after it changes the cluster (lab.e2e.ts uses these
// for its refusals and its teardown). Run with `bun test deploy/flux`.
import { describe, expect, test } from "bun:test";
import {
  FluxNamespace,
  type NamespaceApi,
  RUN_LABEL,
  existingFluxProblem,
  fluxCrdNames,
  issueOfNamespace,
  lockProblem,
  newRunId,
  ownsFluxNamespace,
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

describe("owning flux-system", () => {
  const runId = newRunId();
  const other = newRunId();

  test("a run ID is a label value no other run shares", () => {
    expect(runId).toMatch(/^run-[0-9a-f]{32}$/);
    expect(other).not.toBe(runId);
    expect(new FluxNamespace(runId).manifest()).toContain(`    ${RUN_LABEL}: ${runId}\n`);
  });

  test("decides ownership from the create and, when its result is unknown, from the label", () => {
    const alreadyExists = 'Error from server (AlreadyExists): namespaces "flux-system" already exists';
    expect(ownsFluxNamespace({ createExitCode: 0, createOutput: "namespace/flux-system created", labelOnCluster: runId, runId })).toBe(true);
    expect(ownsFluxNamespace({ createExitCode: 1, createOutput: alreadyExists, labelOnCluster: undefined, runId })).toBe(false);
    expect(ownsFluxNamespace({ createExitCode: 1, createOutput: alreadyExists, labelOnCluster: runId, runId })).toBe(false);
    expect(ownsFluxNamespace({ createExitCode: 255, createOutput: "Connection reset by peer", labelOnCluster: other, runId })).toBe(false);
    expect(ownsFluxNamespace({ createExitCode: 255, createOutput: "Connection reset by peer", labelOnCluster: undefined, runId })).toBe(false);
    expect(ownsFluxNamespace({ createExitCode: 255, createOutput: "Connection reset by peer", labelOnCluster: runId, runId })).toBe(true);
    expect(ownsFluxNamespace({ createExitCode: -1, createOutput: "timed out", labelOnCluster: runId, runId })).toBe(true);
  });

  /** A cluster where flux-system may already exist with some label, and creates may lose their answer. */
  function cluster(state: { label?: string; exists: boolean; loseAnswer?: boolean; throwOnCreate?: boolean }) {
    const calls: string[] = [];
    const api: NamespaceApi = {
      async create(manifest) {
        calls.push("create");
        if (state.throwOnCreate) throw new Error("ssh: connect to host: Connection timed out");
        if (state.exists) return { exitCode: 1, output: 'Error from server (AlreadyExists): namespaces "flux-system" already exists' };
        state.exists = true;
        state.label = new RegExp(`${RUN_LABEL}: (\\S+)`).exec(manifest)?.[1];
        return state.loseAnswer ? { exitCode: 255, output: "Connection to host closed by remote host." } : { exitCode: 0, output: "namespace/flux-system created" };
      },
      async runLabel() {
        calls.push("label");
        return state.exists ? state.label : undefined;
      },
    };
    return { api, calls, state };
  }

  test("a namespace that someone else created is never claimed, and teardown leaves it", async () => {
    for (const label of [undefined, other]) {
      const { api } = cluster({ exists: true, label });
      const namespace = new FluxNamespace(runId);
      await expect(namespace.claim(api)).rejects.toThrow(/namespace flux-system is not this run's .*AlreadyExists.*nothing of Flux is installed or removed/);
      expect(namespace.owned).toBe(false);
      expect(await namespace.mayTearDown(api)).toBe(false);
    }
  });

  test("a create that succeeds, or whose answer is lost after it took effect, is this run's", async () => {
    for (const loseAnswer of [false, true]) {
      const { api } = cluster({ exists: false, loseAnswer });
      const namespace = new FluxNamespace(runId);
      await namespace.claim(api);
      expect(namespace.owned).toBe(true);
      expect(await namespace.mayTearDown(api)).toBe(true);
    }
  });

  test("a create that fails before reaching the cluster owns nothing", async () => {
    const { api } = cluster({ exists: false, throwOnCreate: true });
    const namespace = new FluxNamespace(runId);
    await expect(namespace.claim(api)).rejects.toThrow(/not this run's/);
    expect(namespace.owned).toBe(false);
  });

  test("teardown deletes nothing when the label changed or the namespace is gone", async () => {
    const { api, state } = cluster({ exists: false });
    const namespace = new FluxNamespace(runId);
    await namespace.claim(api);
    state.label = other;
    expect(await namespace.mayTearDown(api)).toBe(false);
    state.exists = false;
    expect(await namespace.mayTearDown(api)).toBe(false);
    const unreadable: NamespaceApi = { ...api, runLabel: async () => Promise.reject(new Error("ssh failed")) };
    state.exists = true;
    state.label = runId;
    expect(await namespace.mayTearDown(unreadable)).toBe(false);
  });
});
