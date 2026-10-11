import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { lastLines, summarizeBunTest } from "./e2e.ts";
import { CommandError, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";
import {
  BASE_PROVIDER,
  clusterNodeNames,
  e2eNamespaceOf,
  importImageIntoCluster,
  IMPORT_ATTEMPTS,
  istiodOverlay,
  providerNameOf,
  verifySha256,
} from "./platform.ts";
import type { ImportCommands } from "./platform.ts";
import { readPins } from "./run.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const DIGEST = `sha256:${"a".repeat(64)}`;

describe("istiodOverlay", () => {
  const base = readFileSync(join(REPO_ROOT, "deploy", "istio", "istiod-values.yaml"), "utf8");
  const overlay = Bun.YAML.parse(istiodOverlay(base, ["onprem", "saas"])) as {
    meshConfig: { extensionProviders: { name: string; envoyExtAuthzHttp: Record<string, unknown> }[] };
    pilot: unknown;
  };
  const original = (Bun.YAML.parse(base) as typeof overlay).meshConfig.extensionProviders.find((p) => p.name === BASE_PROVIDER)!;

  test("repeats the repository's provider once per e2e namespace", () => {
    expect(overlay.meshConfig.extensionProviders.map((p) => p.name)).toEqual(["trustiot-e2e-onprem", "trustiot-e2e-saas"]);
    expect(overlay.meshConfig.extensionProviders.map((p) => p.envoyExtAuthzHttp.service)).toEqual([
      "trustiot-identity.nightly-e2e-onprem.svc.cluster.local",
      "trustiot-identity.nightly-e2e-saas.svc.cluster.local",
    ]);
  });

  test("keeps every other provider setting and the rest of the values", () => {
    for (const provider of overlay.meshConfig.extensionProviders) {
      expect({ ...provider.envoyExtAuthzHttp, service: "" }).toEqual({ ...original.envoyExtAuthzHttp, service: "" });
    }
    expect(overlay.pilot).toEqual((Bun.YAML.parse(base) as typeof overlay).pilot);
    expect(overlay.meshConfig.extensionProviders[0]?.envoyExtAuthzHttp.failOpen).toBe(false);
  });

  test("keeps the mesh's path normalization, which the ingress e2e's /internal slash cases need", () => {
    // Laysi/trustiot-v2#255: without it "//internal" and "/%2finternal" pass the DENY policy.
    const mesh = (Bun.YAML.parse(istiodOverlay(base, ["saas"])) as { meshConfig: { pathNormalization?: unknown } }).meshConfig;
    expect(mesh.pathNormalization).toEqual({ normalization: "DECODE_AND_MERGE_SLASHES" });
  });

  test("fails when the repository's provider is gone", () => {
    expect(() => istiodOverlay("meshConfig:\n  extensionProviders: []\n", ["saas"])).toThrow(BASE_PROVIDER);
  });

  test("names follow the shape", () => {
    expect(e2eNamespaceOf("saas")).toBe("nightly-e2e-saas");
    expect(providerNameOf("saas")).toBe("trustiot-e2e-saas");
  });
});

describe("verifySha256", () => {
  test("accepts the right hash and rejects any other", () => {
    const data = new TextEncoder().encode("hello");
    const hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    expect(() => verifySha256(data, hash, "file")).not.toThrow();
    expect(() => verifySha256(data, hash.toUpperCase(), "file")).not.toThrow();
    expect(() => verifySha256(data, "0".repeat(64), "file")).toThrow("expected");
  });
});

describe("readPins", () => {
  const good = {
    K3S_IMAGE: `rancher/k3s:v1.36.5-k3s1@${DIGEST}`,
    GATEWAY_API_VERSION: "v1.6.1",
    GATEWAY_CRDS_SHA256: "a".repeat(64),
    ISTIO_VERSION: "1.30.5",
  };

  test("accepts exact pins", () => {
    expect(readPins(good)).toMatchObject({ gatewayApiVersion: "v1.6.1", istioVersion: "1.30.5" });
  });

  test("rejects a missing or loose pin", () => {
    expect(() => readPins({ ...good, K3S_IMAGE: "rancher/k3s:latest" })).toThrow("digest");
    expect(() => readPins({ ...good, GATEWAY_API_VERSION: "main" })).toThrow("GATEWAY_API_VERSION");
    expect(() => readPins({ ...good, GATEWAY_CRDS_SHA256: "abc" })).toThrow("GATEWAY_CRDS_SHA256");
    expect(() => readPins({ ...good, ISTIO_VERSION: "1.30" })).toThrow("ISTIO_VERSION");
    expect(() => readPins({ ...good, ISTIO_VERSION: undefined })).toThrow("ISTIO_VERSION");
  });
});

describe("e2e output", () => {
  test("summarizes bun's result lines and falls back to the end of the output", () => {
    expect(summarizeBunTest("ok\n\n 52 pass\n 0 fail\n 90 expect() calls\n")).toBe("52 passed, 0 failed");
    expect(summarizeBunTest("one\ntwo\nthree\nfour")).toBe("two three four");
  });

  test("lastLines keeps the end", () => {
    expect(lastLines("a\nb\nc\nd\n", 2)).toBe("c\nd");
  });
});

describe("importImageIntoCluster", () => {
  const IMAGE = "localhost/trustiot-e2e/identity-double:nightly";
  const K3D = "/pinned/k3d";
  const CLUSTER = "nightly";
  const label = { "k3d.cluster": CLUSTER };

  /** What `k3d node list -o json` prints: this cluster's nodes, another cluster's, and non-containerd nodes. */
  const NODES = [
    { name: "k3d-nightly-server-0", role: "server", runtimeLabels: label },
    { name: "k3d-nightly-agent-0", role: "agent", runtimeLabels: label },
    { name: "k3d-nightly-serverlb", role: "loadbalancer", runtimeLabels: label },
    { name: "k3d-other-server-0", role: "server", runtimeLabels: { "k3d.cluster": "other" } },
  ];

  const ok = (stdout = ""): ExecResult => ({ code: 0, stdout, stderr: "", timedOut: false });
  const failed = (stderr: string): ExecResult => ({ code: 1, stdout: "", stderr, timedOut: false });

  let dir: string;
  let logPath: string;
  let log: RunLog;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "nightly-platform-"));
    logPath = join(dir, "run.log");
    log = new RunLog(logPath);
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  /**
   * A cluster that has the image on the nodes in `has`. Each `k3d image import` is one "import" call:
   * `onImport(n)` returns the nodes that hold the image afterwards (k3d exits 0 either way).
   */
  function fakeCluster(onImport: (call: number) => string[], nodes: unknown = NODES) {
    const state = { imports: [] as string[][], inspected: [] as string[], has: new Set<string>() };
    const commands: ImportCommands = {
      run: async (runLog, cmd) => {
        runLog.line(`$ ${cmd.join(" ")}`);
        expect(cmd.slice(0, 3)).toEqual([K3D, "image", "import"]);
        state.imports.push(cmd);
        state.has = new Set(onImport(state.imports.length));
        return ok("INFO Successfully imported 1 image(s) into 1 cluster(s)");
      },
      exec: async (cmd: string[], _options?: ExecOptions) => {
        if (cmd[0] === K3D) {
          expect(cmd.slice(1)).toEqual(["node", "list", "--output", "json"]);
          return ok(JSON.stringify(nodes));
        }
        expect(cmd.slice(0, 2)).toEqual(["docker", "exec"]);
        expect(cmd.slice(3)).toEqual(["crictl", "inspecti", IMAGE]);
        const node = cmd[2]!;
        state.inspected.push(node);
        return state.has.has(node) ? ok("{}") : failed(`FATA[0000] no such image: "${IMAGE}"`);
      },
    };
    return { state, commands };
  }

  const platform = () => ({ k3d: K3D, cluster: CLUSTER, log });
  const importWith = (commands: ImportCommands, attempts = IMPORT_ATTEMPTS) =>
    importImageIntoCluster(platform(), IMAGE, commands, { attempts, pauseMs: 0 });

  test("finds only this cluster's server and agent nodes", async () => {
    const { commands } = fakeCluster(() => []);
    expect(await clusterNodeNames(platform(), commands)).toEqual(["k3d-nightly-server-0", "k3d-nightly-agent-0"]);
  });

  test("fails when the cluster has no node to check, or k3d's answer is unusable", async () => {
    await expect(clusterNodeNames(platform(), fakeCluster(() => [], []).commands)).rejects.toThrow("no server or agent node");
    await expect(clusterNodeNames(platform(), { exec: async () => ok("not json") })).rejects.toThrow("did not print JSON");
    await expect(clusterNodeNames(platform(), { exec: async () => failed("boom") })).rejects.toThrow("boom");
  });

  test("passes and logs every node when the image is present on all of them", async () => {
    const { state, commands } = fakeCluster(() => ["k3d-nightly-server-0", "k3d-nightly-agent-0"]);
    await importWith(commands);
    expect(state.imports).toHaveLength(1);
    expect(state.inspected).toEqual(["k3d-nightly-server-0", "k3d-nightly-agent-0"]);
    const text = readFileSync(logPath, "utf8");
    expect(text).toContain(`image ${IMAGE} is present on node k3d-nightly-server-0`);
    expect(text).toContain(`image ${IMAGE} is present on node k3d-nightly-agent-0`);
    expect(text).toContain(`image ${IMAGE} confirmed on 2 node(s): k3d-nightly-server-0, k3d-nightly-agent-0`);
  });

  test("imports with --mode direct into the right cluster", async () => {
    const { state, commands } = fakeCluster(() => ["k3d-nightly-server-0", "k3d-nightly-agent-0"]);
    await importWith(commands);
    expect(state.imports[0]).toEqual([K3D, "image", "import", IMAGE, "--cluster", CLUSTER, "--mode", "direct"]);
  });

  test("retries when k3d reports success but a node lacks the image, then passes", async () => {
    // The first import loses the agent's copy, the way #335's tarball race lost the server's.
    const { state, commands } = fakeCluster((call) => (call === 1 ? ["k3d-nightly-server-0"] : ["k3d-nightly-server-0", "k3d-nightly-agent-0"]));
    await importWith(commands);
    expect(state.imports).toHaveLength(2);
    const text = readFileSync(logPath, "utf8");
    expect(text).toContain(`image ${IMAGE} is NOT present on node k3d-nightly-agent-0`);
    expect(text).toContain("attempt 1/3 failed");
    expect(text).toContain(`image ${IMAGE} confirmed on 2 node(s)`);
  });

  test("fails at the import step, naming the image and the node, when every attempt leaves it missing", async () => {
    const { state, commands } = fakeCluster(() => ["k3d-nightly-agent-0"]);
    const failure = importWith(commands);
    await expect(failure).rejects.toThrow("k3d image import succeeded, but image");
    await expect(failure).rejects.toThrow(IMAGE);
    await expect(failure).rejects.toThrow("missing on node(s) k3d-nightly-server-0 of cluster nightly");
    await failure.catch((error: Error) => expect(error.message).not.toContain("k3d-nightly-agent-0 of cluster"));
    expect(state.imports).toHaveLength(IMPORT_ATTEMPTS);
    expect(readFileSync(logPath, "utf8")).toContain(`image ${IMAGE} is NOT present on node k3d-nightly-server-0: FATA[0000] no such image`);
  });

  test("names every missing node", async () => {
    const { commands } = fakeCluster(() => []);
    await expect(importWith(commands, 1)).rejects.toThrow("k3d-nightly-server-0, k3d-nightly-agent-0 of cluster");
  });

  test("an import command that fails is retried like a missing image", async () => {
    const { state, commands } = fakeCluster(() => ["k3d-nightly-server-0", "k3d-nightly-agent-0"]);
    let calls = 0;
    const flaky: ImportCommands = {
      ...commands,
      run: async (runLog, cmd, options) => {
        if (++calls === 1) throw new CommandError(cmd, failed("ERRO failed to import"));
        return commands.run(runLog, cmd, options);
      },
    };
    await importWith(flaky);
    expect(calls).toBe(2);
    expect(state.imports).toHaveLength(1);
  });
});
