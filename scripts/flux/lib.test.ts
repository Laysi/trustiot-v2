import { describe, expect, test } from "bun:test";
import { goEnvironment } from "./age.ts";
import { controllerImages, dockerConfigJson, renderFluxSystemKustomization, renderPullSecret } from "./lib.ts";

const DIGEST = `sha256:${"d".repeat(64)}`;

describe("controllerImages", () => {
  const exported = [
    "        image: ghcr.io/fluxcd/source-controller:v1.9.6",
    "        image: ghcr.io/fluxcd/kustomize-controller:v1.9.6",
    "        image: ghcr.io/fluxcd/helm-controller:v1.6.5",
    "",
  ].join("\n");

  test("lists the three controllers with their tags", () => {
    expect(controllerImages(exported)).toEqual([
      { name: "ghcr.io/fluxcd/source-controller", tag: "v1.9.6" },
      { name: "ghcr.io/fluxcd/kustomize-controller", tag: "v1.9.6" },
      { name: "ghcr.io/fluxcd/helm-controller", tag: "v1.6.5" },
    ]);
  });

  test("rejects other images and missing controllers", () => {
    expect(() => controllerImages(`${exported}        image: docker.io/library/busybox:1\n`)).toThrow(/unexpected images/);
    expect(() => controllerImages(exported.split("\n").slice(1).join("\n"))).toThrow(/expected 3 controller images/);
  });
});

describe("renderFluxSystemKustomization", () => {
  test("pins each image by digest", () => {
    const text = renderFluxSystemKustomization([{ name: "ghcr.io/fluxcd/source-controller", tag: "v1.9.6", digest: DIGEST }]);
    expect(Bun.YAML.parse(text)).toEqual({
      apiVersion: "kustomize.config.k8s.io/v1beta1",
      kind: "Kustomization",
      resources: ["gotk-components.yaml", "gotk-sync.yaml"],
      images: [{ name: "ghcr.io/fluxcd/source-controller", newTag: "v1.9.6", digest: DIGEST }],
    });
    expect(() => renderFluxSystemKustomization([{ name: "x", tag: "v1", digest: "latest" }])).toThrow(/no sha256 digest/);
  });
});

describe("pull Secret", () => {
  // Made up at run time, never a literal that looks like a token.
  const fakeToken = `t${crypto.randomUUID().replaceAll("-", "")}`;

  test("is a dockerconfigjson Secret for the registry", () => {
    const secret = Bun.YAML.parse(renderPullSecret("trustiot", "ghcr.io", "trustiot-bot", fakeToken)) as {
      metadata: { name: string; namespace: string };
      type: string;
      data: Record<string, string>;
    };
    expect(secret.metadata).toEqual({ name: "ghcr-pull", namespace: "trustiot" });
    expect(secret.type).toBe("kubernetes.io/dockerconfigjson");
    const config = JSON.parse(Buffer.from(secret.data[".dockerconfigjson"] ?? "", "base64").toString());
    expect(config.auths["ghcr.io"].username).toBe("trustiot-bot");
    expect(config.auths["ghcr.io"].password).toBe(fakeToken);
    expect(Buffer.from(config.auths["ghcr.io"].auth, "base64").toString()).toBe(`trustiot-bot:${fakeToken}`);
  });

  test("rejects a bad user name or a token with whitespace, without quoting the token", () => {
    expect(() => dockerConfigJson("ghcr.io", "-bad", fakeToken)).toThrow(/user name/);
    for (const bad of ["", `${fakeToken} x`, `${fakeToken}\n2`]) {
      try {
        dockerConfigJson("ghcr.io", "trustiot-bot", bad);
        throw new Error("accepted");
      } catch (error) {
        expect(String(error)).toContain("the token is empty or has spaces");
        expect(String(error)).not.toContain(fakeToken);
      }
    }
  });
});

describe("age build environment", () => {
  test("uses only the public Go proxy and checksum database and drops overrides", () => {
    const env = goEnvironment("/cache/bin");
    expect(env).toMatchObject({ GOBIN: "/cache/bin", GOPROXY: "https://proxy.golang.org", GOSUMDB: "sum.golang.org", GOTOOLCHAIN: "local" });
    for (const name of ["GONOSUMDB", "GONOSUMCHECK", "GONOPROXY", "GOPRIVATE", "GOINSECURE"]) {
      expect(name in env && env[name] === undefined).toBe(true);
    }
    expect(env.GOFLAGS).toContain("-mod=readonly");
  });
});
