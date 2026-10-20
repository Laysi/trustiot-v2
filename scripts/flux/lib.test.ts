import { describe, expect, test } from "bun:test";
import { parseShapeValues, renderChartSource } from "../release/flux.ts";
import { CHART_REGISTRY } from "../release/lib.ts";
import { crossGoEnvironment, goEnvironment, parsePlatform } from "./age.ts";
import {
  DEMO_PRIVATE_VALUES,
  DEMO_VALUES_NAME,
  type ValueFormat,
  checkedSecretValue,
  checkedTextValue,
  controllerImages,
  dockerConfigJson,
  isK8sName,
  parseCluster,
  parseDemoValues,
  renderDemoValuesSecret,
  renderFluxSystemKustomization,
  renderOpaqueSecret,
  renderPullSecret,
  sopsRecipientProblems,
  sopsRuleTexts,
} from "./lib.ts";
import { pinnedFiles } from "./pin-chart.ts";
import { checkName, parseFormat, parsePlain } from "./sops-secret.ts";

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

describe("Opaque Secret", () => {
  // Made up at run time, never a literal that looks like a key.
  const fakeKey = `k${crypto.randomUUID().replaceAll("-", "")}`;

  test("puts the stdin value and the plain values under stringData, with the watch label when asked", () => {
    const text = renderOpaqueSecret({
      namespace: "trustiot",
      name: "trustiot-smtp",
      key: "password",
      value: `${fakeKey}\n`,
      format: "token",
      plain: { username: "resend" },
      labels: { "reconcile.fluxcd.io/watch": "Enabled" },
    });
    expect(Bun.YAML.parse(text)).toEqual({
      apiVersion: "v1",
      kind: "Secret",
      metadata: { name: "trustiot-smtp", namespace: "trustiot", labels: { "reconcile.fluxcd.io/watch": "Enabled" } },
      type: "Opaque",
      stringData: { password: fakeKey, username: "resend" },
    });
  });

  test("--format text keeps one line as written: spaces, <, > and @ included, one trailing newline dropped", () => {
    const sender = "TrustIoT Demo <no-reply@mail.example.test>";
    const secret = Bun.YAML.parse(renderOpaqueSecret({ namespace: "trustiot", name: "x", key: "from", value: `${sender}\n`, format: "text" })) as {
      stringData: Record<string, string>;
    };
    expect(secret.stringData).toEqual({ from: sender });
    expect(checkedTextValue("a, b [c] {d} \\e = f")).toBe("a, b [c] {d} \\e = f");
  });

  test("rejects bad values without quoting them", () => {
    const cases: [string, ValueFormat, string][] = [
      ["", "token", "empty or has spaces"],
      [`${fakeKey} x`, "token", "empty or has spaces"],
      [`${fakeKey}\n2`, "token", "empty or has spaces"],
      ["", "text", "is empty"],
      ["\n", "text", "is empty"],
      [`${fakeKey}\n\n`, "text", "more than one line"],
      [`${fakeKey}\nx`, "text", "more than one line"],
      [`${fakeKey}\r\n`, "text", "carriage return"],
      [`${fakeKey}\r`, "text", "carriage return"],
      [`${fakeKey}\tx`, "text", "control or invisible format character"],
      [`${fakeKey}\u0000`, "text", "control or invisible format character"],
      [`${fakeKey}\u007f`, "text", "control or invisible format character"],
      [`${fakeKey}\u0085`, "text", "control or invisible format character"],
      [`${fakeKey} x`, "text", "control or invisible format character"],
      [`${fakeKey}‮x`, "text", "control or invisible format character"],
      [`${fakeKey}​`, "text", "control or invisible format character"],
      [` ${fakeKey}`, "text", "starts or ends with whitespace"],
      [`${fakeKey} \n`, "text", "starts or ends with whitespace"],
    ];
    for (const [value, format, message] of cases) {
      try {
        checkedSecretValue(value, format);
        throw new Error(`accepted ${JSON.stringify(value)}`);
      } catch (error) {
        expect(String(error)).toContain(message);
        expect(String(error)).not.toContain(fakeKey);
      }
    }
  });

  test("rejects names, keys and labels that are not valid, and a key that is both secret and plain", () => {
    const base = { namespace: "trustiot", name: "trustiot-smtp", key: "password", value: fakeKey, format: "token" as const };
    for (const name of ["Bad_Name", "foo..bar", ".foo", "foo.", "-foo", `${"a".repeat(64)}.x`, `${"a.".repeat(126)}ab`]) {
      expect(() => renderOpaqueSecret({ ...base, name })).toThrow(/must be a Kubernetes name and the namespace a DNS label/);
    }
    expect(() => renderOpaqueSecret({ ...base, namespace: "trust.iot" })).toThrow(/namespace a DNS label/);
    expect(() => renderOpaqueSecret({ ...base, namespace: "a".repeat(64) })).toThrow(/namespace a DNS label/);
    for (const name of ["trustiot-smtp", "a.b-c", `${"a".repeat(63)}.${"b".repeat(63)}`]) {
      expect(isK8sName(name)).toBe(true);
    }
    expect(isK8sName(`${"a.".repeat(126)}a`)).toBe(true);
    expect(() => renderOpaqueSecret({ ...base, key: "pass word" })).toThrow(/not a valid key/);
    expect(() => renderOpaqueSecret({ ...base, plain: { password: "x" } })).toThrow(/both the secret value and a plain one/);
    expect(() => renderOpaqueSecret({ ...base, plain: { username: "a\nb" } })).toThrow(/one line/);
    expect(() => renderOpaqueSecret({ ...base, labels: { "bad key!": "x" } })).toThrow(/not a valid label/);
  });
});

describe("the demo's private values", () => {
  // Made up at run time, so a test can tell whether an error quotes a value.
  const marker = `m${crypto.randomUUID().replaceAll("-", "").slice(0, 12)}`;
  const good: Record<string, string> = {
    "app-host": "app.demo.example.test",
    "gateway-host": "gw.demo.example.test",
    "ota-host": "ota.demo.example.test",
    "smtp-from": "TrustIoT Demo <no-reply@mail.example.test>",
  };
  const lines = (values: Record<string, string>) => Object.entries(values).map(([key, value]) => `${key}=${value}\n`).join("");

  test("are the four values the chart needs, each with its own key", () => {
    expect(DEMO_PRIVATE_VALUES).toEqual({ "app-host": "hosts.app", "gateway-host": "hosts.gateway", "ota-host": "hosts.ota", "smtp-from": "smtp.from" });
  });

  test("the generator writes exactly those keys, verbatim, with the watch label", () => {
    const sender = `Demo, Lab <no-reply@mail.example.test>,serviceDefaults.image.registry=${marker}.test`;
    const secret = Bun.YAML.parse(renderDemoValuesSecret(lines({ ...good, "smtp-from": sender }))) as Record<string, unknown>;
    expect(secret).toEqual({
      apiVersion: "v1",
      kind: "Secret",
      metadata: { name: DEMO_VALUES_NAME, namespace: "trustiot", labels: { "reconcile.fluxcd.io/watch": "Enabled" } },
      type: "Opaque",
      stringData: { ...good, "smtp-from": sender },
    });
    // Order does not matter; one final newline is optional.
    const reversed = Object.entries(good).reverse().map(([k, value]) => `${k}=${value}`).join("\n");
    expect(parseDemoValues(reversed)).toEqual(good);
  });

  test("refuses a missing, unknown or repeated key, a bad host and a bad line, without quoting values", () => {
    const { "ota-host": _ota, ...missing } = good;
    const cases: [string, string][] = [
      [lines(missing), "missing ota-host"],
      [`${lines(good)}extra=${marker}\n`, "line 5 is not <key>=<value>"],
      [`${lines(good)}app-host=${marker}.example.test\n`, "app-host is given more than once"],
      [lines({ ...good, "app-host": `${marker}_x.example.test` }), "app-host is not a lowercase DNS name"],
      [lines({ ...good, "gateway-host": `${marker}.Example.test` }), "gateway-host is not a lowercase DNS name"],
      [lines({ ...good, "ota-host": marker }), "ota-host is not a lowercase DNS name"],
      [lines({ ...good, "smtp-from": `${marker}\r` }), "smtp-from has more than one line or a carriage return"],
      [lines({ ...good, "smtp-from": `${marker}‮` }), "smtp-from has a control or invisible format character"],
      [lines({ ...good, "smtp-from": "" }), "smtp-from is empty"],
      [`${lines(good)}\n`, "line 5 is not <key>=<value>"],
      [`${marker}\n${lines(good)}`, "line 1 is not <key>=<value>"],
    ];
    for (const [text, message] of cases) {
      try {
        parseDemoValues(text);
        throw new Error(`accepted: ${message}`);
      } catch (error) {
        expect(String(error)).toContain(message);
        expect(String(error)).not.toContain(marker);
      }
    }
  });
});

describe(".sops.yaml recipients", () => {
  // Valid age public keys in shape only (bech32 characters), made up for the fixtures.
  const key = (letter: string) => `age1${letter.repeat(58)}`;
  const [saasKey, demoKey, operatorA, operatorB] = [key("q"), key("p"), key("z"), key("r")];
  const config = (saas: string[], demo: string[]) =>
    [
      "creation_rules:",
      "  - path_regex: (^|/)deploy/flux/clusters/saas/[^/]+(/[^/]+)*\\.sops\\.yaml$",
      "    encrypted_regex: ^(data|stringData)$",
      ...(saas.length === 0 ? ["    age: []"] : ["    age:", ...saas.map((line) => `      - ${line}`)]),
      "  - path_regex: (^|/)deploy/flux/clusters/demo/[^/]+(/[^/]+)*\\.sops\\.yaml$",
      "    encrypted_regex: ^(data|stringData)$",
      ...(demo.length === 0 ? ["    age: []"] : ["    age:", ...demo.map((line) => `      - ${line}`)]),
      "",
    ].join("\n");
  const problems = (saas: string[], demo: string[]) => sopsRecipientProblems(sopsRuleTexts(config(saas, demo)));

  test("empty rules and rules with their own cluster's key and every operator key pass", () => {
    expect(problems([], [])).toEqual([]);
    expect(problems([], [`${demoKey} # cluster: demo`, `${operatorA} # operator`])).toEqual([]);
    expect(problems([`${saasKey} # cluster: saas`, `${operatorA} # operator`, `${operatorB} # operator`], [`${demoKey} # cluster: demo`, `${operatorB} # operator`, `${operatorA} # operator`])).toEqual([]);
    const rules = sopsRuleTexts(config([], [`${demoKey} # cluster: demo`, `${operatorA} # operator`]));
    expect(rules[1]?.recipients).toEqual([
      { key: demoKey, role: "cluster: demo" },
      { key: operatorA, role: "operator" },
    ]);
  });

  test("another cluster's key in a rule fails", () => {
    expect(problems([`${demoKey} # cluster: demo`, `${operatorA} # operator`], [`${demoKey} # cluster: demo`, `${operatorA} # operator`])).toEqual([
      `the saas rule holds ${demoKey}, the key of cluster: demo`,
      "the saas rule has recipients but not the saas cluster's key",
    ]);
    expect(problems([`${saasKey} # cluster: saas`, `${demoKey} # cluster: demo`, `${operatorA} # operator`], [`${demoKey} # cluster: demo`, `${operatorA} # operator`])).toEqual([
      `the saas rule holds ${demoKey}, the key of cluster: demo`,
    ]);
  });

  test("a missing operator key, no operator at all, no cluster key, a repeated key and two roles for one key fail", () => {
    expect(problems([`${saasKey} # cluster: saas`, `${operatorA} # operator`], [`${demoKey} # cluster: demo`, `${operatorB} # operator`])).toEqual([
      `the saas rule lacks the operator key ${operatorB}`,
      `the demo rule lacks the operator key ${operatorA}`,
    ]);
    expect(problems([], [`${demoKey} # cluster: demo`])).toEqual(["the demo rule has no operator key, so no person could edit its files"]);
    expect(problems([], [`${operatorA} # operator`])).toEqual(["the demo rule has recipients but not the demo cluster's key"]);
    expect(problems([], [`${demoKey} # cluster: demo`, `${operatorA} # operator`, `${operatorA} # operator`])).toEqual(["the demo rule lists a recipient twice"]);
    expect(problems([`${saasKey} # cluster: saas`, `${operatorA} # operator`], [`${demoKey} # cluster: demo`, `${operatorA} # cluster: demo`])).toEqual([
      `${operatorA} is listed as operator and as cluster: demo`,
    ]);
    expect(problems([], [`${demoKey} # cluster: lab`, `${operatorA} # operator`])).toEqual([
      `${demoKey} has the role cluster: lab, which names no cluster`,
      "the demo rule has recipients but not the demo cluster's key",
    ]);
  });

  test("a recipient without its role, with a name instead of a role, or written another way is refused", () => {
    expect(() => sopsRuleTexts(config([], [demoKey, `${operatorA} # operator`]))).toThrow(/line 8: write a recipient as/);
    expect(() => sopsRuleTexts(config([], [`${demoKey} # cluster: demo`, `${operatorA} # someone's laptop`]))).toThrow(/line 9: write a recipient as/);
    expect(() => sopsRuleTexts(config([], [`not-a-key # operator`]))).toThrow(/write a recipient as/);
    expect(() => sopsRuleTexts(config([], []).replace("    age: []\n  - path_regex: (^|/)deploy/flux/clusters/demo", `    age: ${demoKey}\n  - path_regex: (^|/)deploy/flux/clusters/demo`))).toThrow(
      /line 4: write age as/,
    );
  });
});

describe("parseCluster", () => {
  test("accepts the clusters in deploy/flux/clusters only", () => {
    expect(parseCluster("saas")).toBe("saas");
    expect(parseCluster("demo")).toBe("demo");
    expect(() => parseCluster("../saas")).toThrow(/--cluster must be one of saas, demo/);
    expect(() => parseCluster(undefined)).toThrow(/--cluster/);
  });
});

describe("pinnedFiles", () => {
  test("are exactly what the deploy PR writes for that cluster", () => {
    const files = pinnedFiles("demo", "1.2.0", DIGEST, "global:\n  deploymentMode: saas\n");
    expect([...files.keys()]).toEqual(["deploy/flux/clusters/demo/trustiot/chart.yaml", "deploy/flux/clusters/demo/trustiot/values-saas.yaml"]);
    expect(files.get("deploy/flux/clusters/demo/trustiot/chart.yaml")).toBe(
      renderChartSource({ version: "1.2.0", digest: DIGEST, repository: "Laysi/trustiot-v2", chartRepository: `${CHART_REGISTRY}/trustiot` }),
    );
    expect(parseShapeValues(files.get("deploy/flux/clusters/demo/trustiot/values-saas.yaml") ?? "")).toEqual({ version: "1.2.0", source: "global:\n  deploymentMode: saas\n" });
    expect(() => pinnedFiles("demo", "1.2.0", "latest", "")).toThrow(/sha256 digest/);
    expect(() => pinnedFiles("demo", "latest", DIGEST, "")).toThrow(/not a release tag/);
  });
});

describe("sops-secret options", () => {
  test("--set takes one key=value, --format token or text, and the demo values have their own script", () => {
    expect(parsePlain("username=resend")).toEqual({ username: "resend" });
    expect(parsePlain("a=b=c")).toEqual({ a: "b=c" });
    expect(parsePlain("")).toEqual({});
    expect(() => parsePlain("=x")).toThrow(/<key>=<value>/);
    expect(() => parsePlain("novalue")).toThrow(/<key>=<value>/);
    expect(parseFormat("text")).toBe("text");
    expect(() => parseFormat("yaml")).toThrow(/token or text/);
    expect(checkName("trustiot-smtp")).toBe("trustiot-smtp");
    expect(() => checkName(DEMO_VALUES_NAME)).toThrow(/demo-values\.ts/);
  });
});

describe("age build environment", () => {
  test("uses only the public Go proxy and checksum database and drops overrides", () => {
    const env = goEnvironment("/cache/bin");
    // GOENV=off: a `go env -w GONOSUMDB=...` in the user's go env file would otherwise apply,
    // because go falls back to that file for every variable missing from the environment.
    expect(env).toMatchObject({ GOENV: "off", GOBIN: "/cache/bin", GOPROXY: "https://proxy.golang.org", GOSUMDB: "sum.golang.org", GOTOOLCHAIN: "local" });
    for (const name of ["GONOSUMDB", "GONOSUMCHECK", "GONOPROXY", "GOPRIVATE", "GOINSECURE"]) {
      expect(name in env && env[name] === undefined).toBe(true);
    }
    expect(env.GOFLAGS).toContain("-mod=readonly");
  });

  test("a build for another platform keeps the verification and leaves GOBIN unset", () => {
    const env = crossGoEnvironment("/cache/gopath", { os: "linux", arch: "arm64" });
    expect(env).toMatchObject({ GOENV: "off", GOPATH: "/cache/gopath", GOOS: "linux", GOARCH: "arm64", GOPROXY: "https://proxy.golang.org", GOSUMDB: "sum.golang.org" });
    expect("GOBIN" in env && env.GOBIN === undefined).toBe(true);
    for (const name of ["GONOSUMDB", "GONOSUMCHECK", "GONOPROXY", "GOPRIVATE", "GOINSECURE"]) {
      expect(name in env && env[name] === undefined).toBe(true);
    }
  });

  test("--target takes a Linux platform only", () => {
    expect(parsePlatform("linux/arm64")).toEqual({ os: "linux", arch: "arm64" });
    expect(parsePlatform("linux/amd64")).toEqual({ os: "linux", arch: "amd64" });
    for (const bad of [undefined, "", "darwin/arm64", "linux/arm64 ", "linux/../arm64"]) {
      expect(() => parsePlatform(bad)).toThrow(/--target must be linux\/arm64 or linux\/amd64/);
    }
  });
});
