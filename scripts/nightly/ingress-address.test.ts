// Tests of the fixed ClusterIP of the tunnel ingress in the nightly smoke (Laysi/trustiot-v2#478),
// with no cluster: the ServiceCIDR and Service lists are written the way kubectl prints them.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import {
  clusterIpValues,
  exposureOfValues,
  generatedClusterIpProblems,
  holdersOf,
  ipv4ToNumber,
  numberToIpv4,
  serviceCidrsOf,
  SMOKE_INGRESS_CLUSTER_IPS,
  smokeClusterIpOf,
  staticBandOf,
  staticBandProblems,
} from "./ingress-address.ts";
import { discoverShapes } from "./smoke.ts";

const CHART_DIR = resolve(import.meta.dir, "..", "..", "deploy", "chart");
// k3s's default Service range, which the nightly's k3d cluster keeps (run.ts sets no --service-cidr).
const K3S_SERVICE_CIDR = "10.43.0.0/16";

describe("the static band of a Service range", () => {
  test("is the first min(max(16, size / 16), 256) addresses after the network address", () => {
    expect(staticBandOf(K3S_SERVICE_CIDR)).toEqual({ first: "10.43.0.1", last: "10.43.1.0" });
    // The examples of https://kubernetes.io/docs/concepts/services-networking/cluster-ip-allocation/.
    expect(staticBandOf("10.96.0.0/24")).toEqual({ first: "10.96.0.1", last: "10.96.0.16" });
    expect(staticBandOf("10.96.0.0/20")).toEqual({ first: "10.96.0.1", last: "10.96.1.0" });
    expect(staticBandOf("10.96.0.0/21")).toEqual({ first: "10.96.0.1", last: "10.96.0.128" });
    expect(staticBandOf("10.96.0.0/27")).toEqual({ first: "10.96.0.1", last: "10.96.0.16" });
  });

  test("does not exist in a range of 16 addresses or fewer", () => {
    expect(staticBandOf("10.96.0.0/28")).toBeUndefined();
    expect(staticBandOf("10.96.0.0/32")).toBeUndefined();
  });

  test("refuses what is not an IPv4 CIDR", () => {
    expect(() => staticBandOf("10.43.0.0")).toThrow(/not an IPv4 CIDR/);
    expect(() => staticBandOf("fd00::/108")).toThrow(/not an IPv4 CIDR/);
    expect(() => staticBandOf("10.43.0.1/16")).toThrow(/host bits set/);
    expect(() => staticBandOf("10.043.0.0/16")).toThrow(/not an IPv4 address/);
  });

  test("addresses convert both ways", () => {
    expect(ipv4ToNumber("10.43.1.0")).toBe(10 * 2 ** 24 + 43 * 2 ** 16 + 256);
    expect(numberToIpv4(ipv4ToNumber("255.255.255.255"))).toBe("255.255.255.255");
    expect(() => ipv4ToNumber("10.43.0.256")).toThrow(/not an IPv4 address/);
  });
});

describe("pinning an address in the cluster", () => {
  test("accepts an address of the static band and refuses one outside it, naming the band", () => {
    expect(staticBandProblems("10.43.0.240", [K3S_SERVICE_CIDR])).toEqual([]);
    expect(staticBandProblems("10.43.1.0", [K3S_SERVICE_CIDR])).toEqual([]);
    expect(staticBandProblems("10.43.1.1", [K3S_SERVICE_CIDR])).toEqual([
      "10.43.1.1 is outside the static band 10.43.0.1-10.43.1.0 of the Service range 10.43.0.0/16: any Service created first could take it; pick another in SMOKE_INGRESS_CLUSTER_IPS",
    ]);
    expect(staticBandProblems("10.42.0.240", [K3S_SERVICE_CIDR])[0]).toContain("outside the static band");
    expect(staticBandProblems("10.43.0.0", [K3S_SERVICE_CIDR])[0]).toContain("outside the static band");
  });

  test("reads the IPv4 range of a dual-stack cluster and refuses a cluster without exactly one", () => {
    expect(staticBandProblems("10.43.0.240", [K3S_SERVICE_CIDR, "fd00:10:43::/112"])).toEqual([]);
    expect(staticBandProblems("10.43.0.240", ["fd00:10:43::/112"])[0]).toContain("expected one IPv4 Service range");
    expect(staticBandProblems("10.43.0.240", [])[0]).toContain("expected one IPv4 Service range");
    expect(staticBandProblems("10.43.0.5", ["10.43.0.0/28"])).toEqual(["the Service range 10.43.0.0/28 is too small to have a static band"]);
  });

  test("the ranges come from the ServiceCIDR object's spec.cidrs", () => {
    const object = { apiVersion: "networking.k8s.io/v1", kind: "ServiceCIDR", metadata: { name: "kubernetes" }, spec: { cidrs: [K3S_SERVICE_CIDR] } };
    expect(serviceCidrsOf(object)).toEqual([K3S_SERVICE_CIDR]);
    expect(serviceCidrsOf({})).toEqual([]);
  });

  test("finds every Service that already holds the address, by clusterIP or clusterIPs", () => {
    const list = {
      items: [
        { metadata: { namespace: "kube-system", name: "kube-dns" }, spec: { clusterIP: "10.43.0.10", clusterIPs: ["10.43.0.10"] } },
        { metadata: { namespace: "left", name: "behind" }, spec: { clusterIP: "10.43.0.240", clusterIPs: ["10.43.0.240"] } },
        { metadata: { namespace: "dual", name: "stack" }, spec: { clusterIP: "fd00:10:43::1", clusterIPs: ["fd00:10:43::1", "10.43.0.240"] } },
        { metadata: { namespace: "default", name: "headless" }, spec: { clusterIP: "None" } },
      ],
    };
    expect(holdersOf("10.43.0.240", list)).toEqual(["left/behind", "dual/stack"]);
    expect(holdersOf("10.43.0.241", list)).toEqual([]);
    expect(holdersOf("10.43.0.240", {})).toEqual([]);
  });
});

describe("the Service Istio generated", () => {
  const service = (clusterIP: string, clusterIPs = [clusterIP]) => ({
    metadata: { name: "trustiot-ingress-istio", labels: { "gateway.networking.k8s.io/gateway-name": "trustiot-ingress" } },
    spec: { type: "ClusterIP", clusterIP, clusterIPs },
  });

  test("passes when its ClusterIP is the pinned address", () => {
    expect(generatedClusterIpProblems("trustiot-ingress", { items: [service("10.43.0.240")] }, "10.43.0.240")).toEqual([]);
  });

  test("fails with the Service, the address it got and the field when Istio did not apply the overlay", () => {
    expect(generatedClusterIpProblems("trustiot-ingress", { items: [service("10.43.121.7")] }, "10.43.0.240")).toEqual([
      'Service trustiot-ingress-istio of Gateway trustiot-ingress has ClusterIP "10.43.121.7" (clusterIPs ["10.43.121.7"]), ' +
        "but ingress.tunnel.clusterIP is 10.43.0.240: Istio did not apply the clusterIP of the ingress-options ConfigMap's service overlay",
    ]);
    // The primary address is clusterIPs[0]; a second family's address does not count.
    expect(generatedClusterIpProblems("trustiot-ingress", { items: [service("10.43.0.240", ["fd00:10:43::7", "10.43.0.240"])] }, "10.43.0.240")).toHaveLength(1);
  });

  test("fails unless there is exactly one Service", () => {
    expect(generatedClusterIpProblems("trustiot-ingress", { items: [] }, "10.43.0.240")).toEqual([
      "expected one Service generated for Gateway trustiot-ingress, found 0",
    ]);
    expect(generatedClusterIpProblems("trustiot-ingress", { items: [service("10.43.0.240"), service("10.43.0.241")] }, "10.43.0.240")[0]).toContain("found 2");
  });
});

describe("which releases pin an address", () => {
  test("every tunnel shape has its own address in the static band of k3s's default range, clear of kubernetes and kube-dns", () => {
    const shapes = discoverShapes(CHART_DIR);
    const tunnel = shapes.filter((shape) => exposureOfValues(CHART_DIR, shape.valuesFiles) === "tunnel").map((shape) => shape.name);
    expect(tunnel).toEqual(["saas"]);
    for (const shape of shapes) {
      const exposure = exposureOfValues(CHART_DIR, shape.valuesFiles);
      expect(exposure === "tunnel" || exposure === "direct").toBe(true);
      const address = smokeClusterIpOf(shape.name, exposure);
      expect(address === undefined).toBe(exposure !== "tunnel");
    }
    const addresses = Object.values(SMOKE_INGRESS_CLUSTER_IPS);
    expect(new Set(addresses).size).toBe(addresses.length);
    for (const address of addresses) {
      expect(staticBandProblems(address, [K3S_SERVICE_CIDR])).toEqual([]);
      expect(["10.43.0.1", "10.43.0.10"]).not.toContain(address);
    }
  });

  test("a tunnel shape without an address is an error, so it cannot skip the check", () => {
    expect(() => smokeClusterIpOf("third", "tunnel")).toThrow(/values-third\.yaml exposes the ingress through the tunnel, but SMOKE_INGRESS_CLUSTER_IPS/);
    expect(smokeClusterIpOf("third", "direct")).toBeUndefined();
  });

  test("the last values file that sets ingress.exposure wins", () => {
    expect(exposureOfValues(CHART_DIR, [])).toBe("");
    expect(exposureOfValues(CHART_DIR, ["values-saas.yaml", "tests/values/saas-instance.yaml"])).toBe("tunnel");
    expect(exposureOfValues(CHART_DIR, ["values-saas.yaml", "values-onprem.yaml"])).toBe("direct");
  });

  test("the smoke's values file sets only the field", () => {
    expect(clusterIpValues("10.43.0.240")).toEqual({ ingress: { tunnel: { clusterIP: "10.43.0.240" } } });
  });

  // Only the smoke release asks for an address: the e2e suites install these files (e2e.ts), and
  // a second release with the same address could not create its Service while the first exists.
  test("no values file in the chart pins an address", () => {
    const files = [
      ...readdirSync(CHART_DIR).filter((name) => /^values.*\.yaml$/.test(name)),
      ...readdirSync(join(CHART_DIR, "tests", "values")).map((name) => join("tests", "values", name)),
      ...readdirSync(join(CHART_DIR, "tests", "e2e")).filter((name) => name.endsWith(".yaml")).map((name) => join("tests", "e2e", name)),
    ];
    expect(files).toContain("values.yaml");
    expect(files).toContain(join("tests", "e2e", "values-e2e.yaml"));
    for (const file of files) {
      const values = Bun.YAML.parse(readFileSync(join(CHART_DIR, file), "utf8")) as { ingress?: { tunnel?: { clusterIP?: unknown } } } | null;
      const address = values?.ingress?.tunnel?.clusterIP;
      expect({ file, address: address ?? "" }).toEqual({ file, address: "" });
    }
  });
});
