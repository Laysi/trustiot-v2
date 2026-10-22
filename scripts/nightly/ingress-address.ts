// The fixed ClusterIP of the tunnel ingress (Laysi/trustiot-v2#478): cloudflared on the host
// forwards to the ingress gateway Service's ClusterIP, so the chart's ingress.tunnel.clusterIP
// pins it, and Istio writes it into the Service it generates from the Gateway (the `service`
// overlay of the ingress-options ConfigMap). The smoke deployment of every tunnel shape sets an
// address from the static band of the cluster's Service range and checks that the generated
// Service really has it.
//
// Only the smoke release of a tunnel shape sets the address; the e2e suites (e2e.ts) install
// their own releases without it (ingress-address.test.ts checks every values file of the chart),
// and each shape has its own address. The shapes run one after another, each in a namespace that
// is deleted, with --wait, before the next starts. So no two Services in the cluster ever ask for
// the same address; before the install the smoke still checks that no Service holds it, so a
// Service left behind fails with its name instead of a Gateway that never becomes programmed.
import { readFileSync } from "node:fs";
import { join } from "node:path";

/**
 * The address each tunnel shape's smoke release pins: in the static band of k3s's default Service
 * range 10.43.0.0/16 (10.43.0.1 through 10.43.1.0), away from kubernetes (10.43.0.1) and kube-dns
 * (10.43.0.10). One address per shape, so no two releases of the run ever ask for the same one.
 */
export const SMOKE_INGRESS_CLUSTER_IPS: Readonly<Record<string, string>> = {
  saas: "10.43.0.240",
};

/** The values field the chart reads (deploy/chart/values.yaml). */
export const CLUSTER_IP_FIELD = "ingress.tunnel.clusterIP";

const IPV4 = /^(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}$/;

type YamlMap = Record<string, unknown>;

function isMap(value: unknown): value is YamlMap {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function asMap(value: unknown): YamlMap {
  return isMap(value) ? value : {};
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

/** A dotted-quad IPv4 address as a number; throws on anything else. */
export function ipv4ToNumber(address: string): number {
  if (!IPV4.test(address)) throw new Error(`${JSON.stringify(address)} is not an IPv4 address`);
  return address.split(".").reduce((value, octet) => value * 256 + Number(octet), 0);
}

export function numberToIpv4(value: number): string {
  return [24, 16, 8, 0].map((shift) => Math.floor(value / 2 ** shift) % 256).join(".");
}

/**
 * The static band of an IPv4 Service range, or undefined when the range has none: Kubernetes keeps
 * the first min(max(16, size / 16), 256) addresses after the network address for Services that ask
 * for one, and hands out dynamic addresses from the rest first; a range of 16 addresses or fewer
 * has no band (https://kubernetes.io/docs/concepts/services-networking/cluster-ip-allocation/).
 * In the Kubernetes v1.36.5 source the band size is calculateRangeOffset in
 * pkg/registry/core/service/ipallocator/bitmap.go. Clusters with the ServiceCIDR API (stable and
 * locked on since Kubernetes 1.33, so k3s 1.36) allocate through the IPAddress-based allocator:
 * the MetaAllocator in cidrallocator.go creates one per ServiceCIDR with NewIPAllocator in
 * ipallocator.go, which calls the same calculateRangeOffset, so both allocators keep the same band.
 * For 10.43.0.0/16 that is 10.43.0.1 through 10.43.1.0.
 */
export function staticBandOf(cidr: string): { first: string; last: string } | undefined {
  const match = /^([0-9.]+)\/([0-9]{1,2})$/.exec(cidr);
  const prefix = match === null ? NaN : Number(match[2]);
  if (match === null || !(prefix >= 1 && prefix <= 32)) throw new Error(`${JSON.stringify(cidr)} is not an IPv4 CIDR`);
  const size = 2 ** (32 - prefix);
  const network = ipv4ToNumber(match[1]!);
  if (network % size !== 0) throw new Error(`${cidr} has host bits set`);
  if (size <= 16) return undefined;
  const offset = Math.min(Math.max(16, size / 16), 256);
  return { first: numberToIpv4(network + 1), last: numberToIpv4(network + offset) };
}

/**
 * ingress.exposure as helm sees it: values.yaml, then each of `valuesFiles` (relative to the
 * chart, in `helm -f` order); the last file that sets it wins.
 */
export function exposureOfValues(chartDir: string, valuesFiles: readonly string[]): string {
  let exposure = "";
  for (const file of ["values.yaml", ...valuesFiles]) {
    const values = Bun.YAML.parse(readFileSync(join(chartDir, file), "utf8")) as { ingress?: { exposure?: unknown } } | null;
    const set = values?.ingress?.exposure;
    if (set !== undefined && set !== null) exposure = String(set);
  }
  return exposure;
}

/**
 * The address the smoke release of `shape` pins, or undefined when the shape does not expose the
 * ingress through the tunnel (the chart refuses the field then). A tunnel shape without an entry
 * in SMOKE_INGRESS_CLUSTER_IPS is an error, so a new shape cannot skip the check.
 */
export function smokeClusterIpOf(shape: string, exposure: string): string | undefined {
  if (exposure !== "tunnel") return undefined;
  const address = SMOKE_INGRESS_CLUSTER_IPS[shape];
  if (address === undefined) {
    throw new Error(
      `values-${shape}.yaml exposes the ingress through the tunnel, but SMOKE_INGRESS_CLUSTER_IPS ` +
        "(scripts/nightly/ingress-address.ts) has no address for it; add an unused one of the static band",
    );
  }
  return address;
}

/** The values file that pins the address for the smoke release. */
export function clusterIpValues(address: string): Record<string, unknown> {
  return { ingress: { tunnel: { clusterIP: address } } };
}

/** The ranges of a ServiceCIDR object (`kubectl get servicecidr kubernetes --output json`). */
export function serviceCidrsOf(serviceCidr: unknown): string[] {
  return asArray(asMap(asMap(serviceCidr).spec).cidrs).filter((cidr): cidr is string => typeof cidr === "string");
}

/** What is wrong with pinning `address` in a cluster whose Service ranges are `cidrs`. */
export function staticBandProblems(address: string, cidrs: readonly string[]): string[] {
  const ipv4 = cidrs.filter((cidr) => cidr.includes("."));
  if (ipv4.length !== 1) return [`expected one IPv4 Service range in the cluster's ServiceCIDR, found ${JSON.stringify(cidrs)}`];
  const cidr = ipv4[0]!;
  const band = staticBandOf(cidr);
  if (band === undefined) return [`the Service range ${cidr} is too small to have a static band`];
  const value = ipv4ToNumber(address);
  if (value < ipv4ToNumber(band.first) || value > ipv4ToNumber(band.last)) {
    return [
      `${address} is outside the static band ${band.first}-${band.last} of the Service range ${cidr}: ` +
        "any Service created first could take it; pick another in SMOKE_INGRESS_CLUSTER_IPS",
    ];
  }
  return [];
}

/** The Services (`namespace/name`) of a `kubectl get services --all-namespaces --output json` list that hold `address`. */
export function holdersOf(address: string, serviceList: unknown): string[] {
  return asArray(asMap(serviceList).items)
    .filter(isMap)
    .filter((service) => {
      const spec = asMap(service.spec);
      return spec.clusterIP === address || asArray(spec.clusterIPs).includes(address);
    })
    .map((service) => {
      const metadata = asMap(service.metadata);
      return `${String(metadata.namespace)}/${String(metadata.name)}`;
    });
}

/**
 * What is wrong with the Services Istio generated for `gateway` (`kubectl get services
 * --selector gateway.networking.k8s.io/gateway-name=<gateway> --output json`) when the chart asked
 * for `expected`: there must be exactly one, and its ClusterIP must be that address.
 */
export function generatedClusterIpProblems(gateway: string, serviceList: unknown, expected: string): string[] {
  const services = asArray(asMap(serviceList).items).filter(isMap);
  if (services.length !== 1) return [`expected one Service generated for Gateway ${gateway}, found ${services.length}`];
  const service = services[0]!;
  const name = String(asMap(service.metadata).name);
  const spec = asMap(service.spec);
  const clusterIp = spec.clusterIP;
  const clusterIps = asArray(spec.clusterIPs);
  if (clusterIp !== expected || clusterIps[0] !== expected) {
    return [
      `Service ${name} of Gateway ${gateway} has ClusterIP ${JSON.stringify(clusterIp)} (clusterIPs ${JSON.stringify(clusterIps)}), ` +
        `but ${CLUSTER_IP_FIELD} is ${expected}: Istio did not apply the clusterIP of the ingress-options ConfigMap's service overlay`,
    ];
  }
  return [];
}
