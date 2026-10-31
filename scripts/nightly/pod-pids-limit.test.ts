import { describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { POD_PIDS_LIMIT } from "./dashboard-check.ts";
import { checkPodPidsLimit, type Kubectl, kubectlCommand, nodeNamesOf, podPidsLimitOf } from "./pod-pids-limit.ts";

const SCRIPT = join(import.meta.dir, "pod-pids-limit.ts");
const nodes = (...names: unknown[]) => JSON.stringify({ items: names.map((name) => ({ metadata: { name } })) });
const configz = (limit: unknown) => JSON.stringify({ kubeletconfig: { enableServer: true, podPidsLimit: limit, maxPods: 110 } });

/** A cluster whose kubelets answer `limits` by node name; it records every command. */
function cluster(limits: Record<string, unknown>, unreadable: string[] = []): { kubectl: Kubectl; calls: string[][] } {
  const calls: string[][] = [];
  const kubectl: Kubectl = async (args) => {
    calls.push(args);
    if (args.join(" ") === "get nodes --output json") return nodes(...Object.keys(limits));
    const match = /^\/api\/v1\/nodes\/([^/]+)\/proxy\/configz$/.exec(args[2] ?? "");
    if (args[0] !== "get" || args[1] !== "--raw" || match === null) throw new Error(`unexpected kubectl ${args.join(" ")}`);
    if (unreadable.includes(match[1]!)) throw new Error("Error from server (Forbidden): nodes \"x\" is forbidden\nmore");
    return configz(limits[match[1]!]);
  };
  return { kubectl, calls };
}

describe("reading the kubelet's process limit", () => {
  test("the node names, which go into the proxy's path, are node names", () => {
    expect(nodeNamesOf(JSON.parse(nodes("k3d-nightly-server-0", "node.example")))).toEqual(["k3d-nightly-server-0", "node.example"]);
    expect(() => nodeNamesOf({ items: [] })).toThrow("the cluster lists no node");
    expect(() => nodeNamesOf({})).toThrow("the cluster lists no node");
    expect(() => nodeNamesOf(null)).toThrow("the cluster lists no node");
    for (const name of ["../configz", "node/proxy", "Node", "", "-node", "node-", 7, undefined, "a".repeat(254)]) {
      expect(() => nodeNamesOf(JSON.parse(nodes(name))), String(name)).toThrow("not a node name");
    }
  });

  test("podPidsLimit is the kubelet configuration's integer, or nothing", () => {
    expect(podPidsLimitOf(JSON.parse(configz(16_384)))).toBe(16_384);
    expect(podPidsLimitOf(JSON.parse(configz(-1)))).toBe(-1);
    expect(podPidsLimitOf({ kubeletconfig: {} })).toBeUndefined();
    expect(podPidsLimitOf(JSON.parse(configz("16384")))).toBeUndefined();
    expect(podPidsLimitOf(JSON.parse(configz(16_384.5)))).toBeUndefined();
    expect(podPidsLimitOf({ podPidsLimit: 16_384 })).toBeUndefined();
    expect(podPidsLimitOf(null)).toBeUndefined();
  });

  test("every node with the limit passes, one line each, read through the node proxy", async () => {
    const { kubectl, calls } = cluster({ "node-a": POD_PIDS_LIMIT, "node-b": POD_PIDS_LIMIT });
    expect(await checkPodPidsLimit(kubectl)).toEqual(["node node-a: podPidsLimit 16384", "node node-b: podPidsLimit 16384"]);
    expect(calls).toEqual([
      ["get", "nodes", "--output", "json"],
      ["get", "--raw", "/api/v1/nodes/node-a/proxy/configz"],
      ["get", "--raw", "/api/v1/nodes/node-b/proxy/configz"],
    ]);
    expect(POD_PIDS_LIMIT).toBe(16_384);
  });

  test("names every node without the limit or whose configuration cannot be read", async () => {
    const { kubectl } = cluster({ "node-a": POD_PIDS_LIMIT, "node-b": -1, "node-c": undefined, "node-d": 4_096, "node-e": POD_PIDS_LIMIT }, ["node-e"]);
    const failed = checkPodPidsLimit(kubectl);
    await expect(failed).rejects.toThrow(
      "node node-b: podPidsLimit -1, expected 16384; node node-c: podPidsLimit not set, expected 16384; node node-d: podPidsLimit 4096, expected 16384; " +
        "node node-e: its kubelet configuration cannot be read (Error from server (Forbidden): nodes \"x\" is forbidden)",
    );
    await expect(checkPodPidsLimit(cluster({ "node-a": POD_PIDS_LIMIT }).kubectl, 8_192)).rejects.toThrow("node node-a: podPidsLimit 16384, expected 8192");
  });

  test("kubectl gets the context first when one is named", () => {
    expect(kubectlCommand("kubectl", undefined, ["get", "nodes"])).toEqual(["kubectl", "get", "nodes"]);
    expect(kubectlCommand("/bin/kubectl", "saas", ["get", "nodes"])).toEqual(["/bin/kubectl", "--context", "saas", "get", "nodes"]);
  });
});

describe("the command for another cluster", () => {
  /** A kubectl that answers from the environment and records its arguments and KUBECONFIG. */
  const FAKE_KUBECTL = `#!/bin/sh
printf '%s|%s\\n' "\${KUBECONFIG:-}" "$*" >> "$FAKE_LOG"
case "$*" in
  *"get nodes --output json") printf '%s' "$FAKE_NODES" ;;
  *"get --raw /api/v1/nodes/node-a/proxy/configz") printf '{"kubeletconfig":{"podPidsLimit":16384}}' ;;
  *"get --raw /api/v1/nodes/node-b/proxy/configz") printf '{"kubeletconfig":{"podPidsLimit":%s}}' "$NODE_B_LIMIT" ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
`;

  async function runCommand(args: string[], env: Record<string, string>): Promise<{ code: number; stdout: string; stderr: string; log: string }> {
    const dir = mkdtempSync(join(tmpdir(), "pod-pids-limit-test-"));
    try {
      const kubectl = join(dir, "kubectl");
      writeFileSync(kubectl, FAKE_KUBECTL);
      chmodSync(kubectl, 0o755);
      const log = join(dir, "log");
      writeFileSync(log, "");
      const proc = Bun.spawn([process.execPath, SCRIPT, "--kubectl", kubectl, ...args], {
        env: { PATH: process.env.PATH ?? "", FAKE_LOG: log, FAKE_NODES: nodes("node-a", "node-b"), ...env },
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, stderr, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
      return { code, stdout, stderr, log: readFileSync(log, "utf8") };
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }

  test("exits 0 and prints every node when each has the limit, with the kubeconfig and context given", async () => {
    const result = await runCommand(["--kubeconfig", "/clusters/demo.yaml", "--context", "demo"], { NODE_B_LIMIT: "16384" });
    expect(result).toMatchObject({ code: 0, stderr: "" });
    expect(result.stdout).toBe("node node-a: podPidsLimit 16384\nnode node-b: podPidsLimit 16384\nevery node runs pods under podPidsLimit 16384\n");
    expect(result.log).toBe(
      "/clusters/demo.yaml|--context demo get nodes --output json\n" +
        "/clusters/demo.yaml|--context demo get --raw /api/v1/nodes/node-a/proxy/configz\n" +
        "/clusters/demo.yaml|--context demo get --raw /api/v1/nodes/node-b/proxy/configz\n",
    );
  });

  test("exits 1 naming the node without the limit", async () => {
    const result = await runCommand([], { NODE_B_LIMIT: "-1" });
    expect(result.code).toBe(1);
    expect(result.stderr).toContain("node node-b: podPidsLimit -1, expected 16384");
    // Without --kubeconfig, kubectl's own default applies.
    expect(result.log.split("\n")[0]).toBe("|get nodes --output json");
  });

  test("exits 1 when kubectl fails, with its reason", async () => {
    const result = await runCommand([], { FAKE_NODES: "not json", NODE_B_LIMIT: "16384" });
    expect(result.code).toBe(1);
    expect(result.stderr).toContain("JSON");
  });

  test("exits 2 on an argument it does not know", async () => {
    const result = await runCommand(["--namespace", "x"], {});
    expect(result.code).toBe(2);
    expect(result.stderr).toContain("usage: bun scripts/nightly/pod-pids-limit.ts");
    expect(result.log).toBe("");
  });
});
