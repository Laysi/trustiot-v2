// The kubelet's process limit per pod on every node of a cluster (Laysi/trustiot-v2#150). The
// dashboard check runs pages a model wrote, and their code can start processes in its pod until
// the check ends; the limit keeps one pod from using up the node's PIDs (deploy/chart/README.md,
// 「儀表板檢查」). It is a node setting, outside the chart, so it is read where it takes effect:
// each kubelet's running configuration (`/configz`, through the API server's node proxy), whose
// `podPidsLimit` must be POD_PIDS_LIMIT on every node, because the dashboard check's pod may be
// scheduled on any of them.
//
// The nightly smoke checks its k3d cluster with it (smoke.ts). Before the dashboard check is
// turned on in another cluster, run it there:
//   bun scripts/nightly/pod-pids-limit.ts [--kubeconfig <file>] [--context <name>] [--kubectl <path>]
// It only reads: the node list and each node's kubelet configuration, which needs `get` on
// `nodes/proxy` (a cluster administrator's kubeconfig has it). It prints one line per node and
// exits 0 when every node has the limit, 1 when a node does not or cannot be read, and 2 when the
// arguments are wrong.
import { parseArgs } from "node:util";

import { POD_PIDS_LIMIT } from "./dashboard-check.ts";
import { exec } from "./exec.ts";

/** A node's name as Kubernetes allows it (a DNS subdomain), which goes into the proxy's path. */
const NODE_NAME = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$/;
/** How long one kubectl read may take. */
export const KUBECTL_TIMEOUT_MS = 60_000;

/** Runs `kubectl <args>` against the cluster and returns its standard output; throws on failure. */
export type Kubectl = (args: string[]) => Promise<string>;

/** The names of the nodes in `kubectl get nodes --output json`; throws when there is none or one is not a node name. */
export function nodeNamesOf(list: unknown): string[] {
  const items = (list as { items?: unknown } | null)?.items;
  if (!Array.isArray(items) || items.length === 0) throw new Error("the cluster lists no node");
  return items.map((item) => {
    const name = (item as { metadata?: { name?: unknown } } | null)?.metadata?.name;
    if (typeof name !== "string" || name.length > 253 || !NODE_NAME.test(name)) throw new Error(`not a node name: ${JSON.stringify(name)}`);
    return name;
  });
}

/** `podPidsLimit` of a kubelet's `/configz`, or undefined when it has none that is an integer. */
export function podPidsLimitOf(configz: unknown): number | undefined {
  const limit = (configz as { kubeletconfig?: { podPidsLimit?: unknown } } | null)?.kubeletconfig?.podPidsLimit;
  return typeof limit === "number" && Number.isSafeInteger(limit) ? limit : undefined;
}

/**
 * Reads every node's kubelet configuration and returns one line per node; throws, naming every
 * node whose limit differs from `expected` or cannot be read.
 */
export async function checkPodPidsLimit(kubectl: Kubectl, expected = POD_PIDS_LIMIT): Promise<string[]> {
  const nodes = nodeNamesOf(JSON.parse(await kubectl(["get", "nodes", "--output", "json"])) as unknown);
  const lines: string[] = [];
  const problems: string[] = [];
  for (const node of nodes) {
    let limit: number | undefined;
    try {
      limit = podPidsLimitOf(JSON.parse(await kubectl(["get", "--raw", `/api/v1/nodes/${node}/proxy/configz`])) as unknown);
    } catch (error) {
      problems.push(`node ${node}: its kubelet configuration cannot be read (${error instanceof Error ? error.message.split("\n")[0] : String(error)})`);
      continue;
    }
    if (limit === expected) {
      lines.push(`node ${node}: podPidsLimit ${limit}`);
    } else {
      problems.push(`node ${node}: podPidsLimit ${limit === undefined ? "not set" : limit}, expected ${expected}`);
    }
  }
  if (problems.length > 0) throw new Error(problems.join("; "));
  return lines;
}

/** The command line kubectl runs with: the context, when one is named, before the arguments. */
export function kubectlCommand(kubectl: string, context: string | undefined, args: string[]): string[] {
  return [kubectl, ...(context === undefined ? [] : ["--context", context]), ...args];
}

async function main(argv: string[]): Promise<number> {
  let values: { kubeconfig?: string; context?: string; kubectl?: string };
  try {
    ({ values } = parseArgs({
      args: argv,
      options: { kubeconfig: { type: "string" }, context: { type: "string" }, kubectl: { type: "string" } },
      strict: true,
      allowPositionals: false,
    }));
  } catch (error) {
    console.error(`${error instanceof Error ? error.message : String(error)}\nusage: bun scripts/nightly/pod-pids-limit.ts [--kubeconfig <file>] [--context <name>] [--kubectl <path>]`);
    return 2;
  }
  const env = values.kubeconfig === undefined ? undefined : { KUBECONFIG: values.kubeconfig };
  const kubectl: Kubectl = async (args) => {
    const result = await exec(kubectlCommand(values.kubectl ?? "kubectl", values.context, args), { env, timeoutMs: KUBECTL_TIMEOUT_MS });
    if (result.code !== 0) throw new Error(result.timedOut ? `kubectl timed out after ${KUBECTL_TIMEOUT_MS / 1000} s` : result.stderr.trim() || `kubectl exited with ${result.code}`);
    return result.stdout;
  };
  try {
    for (const line of await checkPodPidsLimit(kubectl)) console.log(line);
    console.log(`every node runs pods under podPidsLimit ${POD_PIDS_LIMIT}`);
    return 0;
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    return 1;
  }
}

if (import.meta.main) {
  process.exit(await main(process.argv.slice(2)));
}
