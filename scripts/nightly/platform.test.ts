import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { lastLines, summarizeBunTest } from "./e2e.ts";
import { BASE_PROVIDER, e2eNamespaceOf, istiodOverlay, providerNameOf, verifySha256 } from "./platform.ts";
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
