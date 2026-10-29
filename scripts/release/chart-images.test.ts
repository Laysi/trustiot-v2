import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  type ChartPin,
  NOT_DEPLOYED_BY_CHART,
  SHAPE_VALUES_FILES,
  chartImage,
  chartPinProblems,
  chartPins,
  imageReference,
  mergeValues,
  packagePinnedChart,
  parseValues,
  pinChartValues,
  pinnedReference,
  readChartValues,
  unpackChart,
} from "./chart-images.ts";
import { protoTool, run, systemTool } from "./exec.ts";
import { CLUSTERS, appDir, parseShapeValues, shapeValuesFile } from "./flux.ts";
import { CHART_DIR, REGISTRY, discoverImages, isMap, planImage, sha256Digest } from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");
const CHART = join(ROOT, CHART_DIR);
const VALUES_SOURCE = readFileSync(join(CHART, "values.yaml"), "utf8");
const VALUES = parseValues(VALUES_SOURCE, "values.yaml");
const VERSION = "1.2.3";
const LOCAL_REGISTRY = "127.0.0.1:5068/laysi/trustiot-v2";
/** helm runs several times per test; the bound is far above what it needs on a loaded runner. */
const HELM_TEST_MS = 180_000;

const digest = (char: string): string => `sha256:${char.repeat(64)}`;
const IDENTITY = { name: "identity", kind: "service" as const };
const POSTGRES = { name: "postgres", kind: "postgres" as const };
const MIGRATE = { name: "migrate", kind: "migrate" as const };

/** The pins a release of the repository's images would write, with made-up digests. */
function repositoryPins(registry = REGISTRY): ChartPin[] {
  const planned = discoverImages(ROOT).map((spec) => planImage(spec, VERSION, registry));
  const published = planned.map((image, i) => ({ name: image.name, ref: image.ref, digest: digest("0123456789abcdef"[i % 16] ?? "0") }));
  return chartPins(planned, published, VALUES);
}

const workDirs: string[] = [];
function workDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "trustiot-chart-images-test-"));
  workDirs.push(dir);
  return dir;
}
afterAll(() => {
  for (const dir of workDirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("chartImage", () => {
  test("maps the Postgres and migration Job images and service keys, skips images the chart does not deploy", () => {
    expect(chartImage("postgres", VALUES)).toEqual(POSTGRES);
    expect(chartImage("migrate", VALUES)).toEqual(MIGRATE);
    expect(chartImage("identity", VALUES)).toEqual(IDENTITY);
    expect(() => chartImage("factory-tool", VALUES)).toThrow(/neither deployed by the chart/);
    // No published image is undeployed at the moment, so the skip is checked with a set of its own.
    const notDeployed = new Set(["factory-tool"]);
    expect(chartImage("factory-tool", VALUES, notDeployed)).toBeUndefined();
    expect(chartImage("identity", VALUES, notDeployed)).toEqual(IDENTITY);
  });

  test("chartPins pins every published image the chart deploys and skips the ones it does not", () => {
    const specs = [...discoverImages(ROOT), { kind: "rust" as const, name: "factory-tool", package: "factory-tool" }];
    const planned = specs.map((spec) => planImage(spec, VERSION, REGISTRY));
    const published = planned.map((image) => ({ name: image.name, ref: image.ref, digest: digest("e") }));
    expect(() => chartPins(planned, published, VALUES)).toThrow(/factory-tool/);
    const pins = chartPins(planned, published, VALUES, new Set(["factory-tool"]));
    expect(pins.map((pin) => pin.image.name).sort()).toEqual(discoverImages(ROOT).map((spec) => spec.name).sort());
  });

  test("every image under deploy/images is either deployed by the chart or listed as not deployed", () => {
    const names = discoverImages(ROOT).map((spec) => spec.name);
    const deployed = names.filter((name) => chartImage(name, VALUES) !== undefined);
    expect(deployed).toContain("identity");
    expect(deployed).toContain("postgres");
    expect(deployed).toContain("migrate");
    for (const name of NOT_DEPLOYED_BY_CHART) {
      expect(names).toContain(name);
    }
  });
});

describe("imageReference", () => {
  test("follows the templates: registry/name or repository, tag defaults to appVersion, digest when set", () => {
    expect(imageReference(VALUES, IDENTITY, VERSION).ref).toBe(`${REGISTRY}/identity:${VERSION}`);
    expect(imageReference(VALUES, POSTGRES, VERSION).ref).toBe(`${REGISTRY}/postgres:${VERSION}`);
    const values = mergeValues(VALUES, {
      serviceDefaults: { image: { registry: "mirror.test/trustiot" } },
      services: { identity: { image: { tag: "hotfix", digest: digest("d") } } },
    });
    expect(imageReference(values, IDENTITY, VERSION).ref).toBe(`mirror.test/trustiot/identity:hotfix@${digest("d")}`);
    const own = mergeValues(VALUES, { services: { identity: { image: { repository: "mirror.test/id" } } } });
    expect(imageReference(own, IDENTITY, VERSION).repository).toBe("mirror.test/id");
  });

  test("mergeValues layers like Helm: maps merge, scalars and lists are replaced, null removes", () => {
    expect(mergeValues({ a: { b: 1, c: [1] }, d: 1 }, { a: { c: [2], e: 3 }, d: null })).toEqual({ a: { b: 1, c: [2], e: 3 } });
  });
});

describe("pinChartValues", () => {
  test("a release writes only the digests into the repository's values.yaml, keeping its comments", () => {
    const pins = [
      { image: IDENTITY, repository: `${REGISTRY}/identity`, digest: digest("a") },
      { image: POSTGRES, repository: `${REGISTRY}/postgres`, digest: digest("b") },
    ];
    const pinned = pinChartValues(VALUES_SOURCE, pins, { appVersion: VERSION, rewriteRepositories: false });
    // Exactly two edits: the identity digest added under services.identity, the Postgres digest set.
    const identity = "\n  identity:\n    enabled: false\n";
    const postgres = '\n      # sha256 digest; when set, the image is pinned to it.\n      digest: ""\n';
    expect(VALUES_SOURCE.split(identity)).toHaveLength(2);
    expect(VALUES_SOURCE.split(postgres)).toHaveLength(2);
    const expected = VALUES_SOURCE.replace(
      identity,
      `\n  identity:\n    # Added by the release (scripts/release/chart-images.ts).\n    image:\n      digest: ${digest("a")}\n    enabled: false\n`,
    ).replace(postgres, `\n      # sha256 digest; when set, the image is pinned to it.\n      digest: ${digest("b")}\n`);
    expect(pinned).toBe(expected);
    const values = parseValues(pinned, "pinned");
    expect(imageReference(values, IDENTITY, VERSION).ref).toBe(`${REGISTRY}/identity:${VERSION}@${digest("a")}`);
    expect(imageReference(values, POSTGRES, VERSION).ref).toBe(`${REGISTRY}/postgres:${VERSION}@${digest("b")}`);
    // Deterministic: the same input gives the same text.
    expect(pinChartValues(VALUES_SOURCE, pins, { appVersion: VERSION, rewriteRepositories: false })).toBe(pinned);
  });

  test("a local test also points the chart at the registry it pushed to", () => {
    const pins = repositoryPins(LOCAL_REGISTRY);
    const values = parseValues(pinChartValues(VALUES_SOURCE, pins, { appVersion: VERSION, rewriteRepositories: true }), "pinned");
    for (const pin of pins) {
      expect(imageReference(values, pin.image, VERSION).ref).toBe(`${LOCAL_REGISTRY}/${pin.image.name}:${VERSION}@${pin.digest}`);
    }
    expect(values.serviceDefaults).toMatchObject({ image: { registry: LOCAL_REGISTRY } });
  });

  test("refuses a chart that already pins, sets a tag, or names another registry", () => {
    const pins = repositoryPins();
    const once = pinChartValues(VALUES_SOURCE, pins, { appVersion: VERSION, rewriteRepositories: false });
    expect(() => pinChartValues(once, pins, { appVersion: VERSION, rewriteRepositories: false })).toThrow(/already pins/);
    const tagged = VALUES_SOURCE.replace('      # Empty means Chart.appVersion.\n      tag: ""', '      # Empty means Chart.appVersion.\n      tag: "18"');
    expect(tagged).not.toBe(VALUES_SOURCE);
    expect(() => pinChartValues(tagged, pins, { appVersion: VERSION, rewriteRepositories: false })).toThrow(/with tag 18/);
    expect(() => pinChartValues(VALUES_SOURCE, repositoryPins(LOCAL_REGISTRY), { appVersion: VERSION, rewriteRepositories: false })).toThrow(
      /but the release pushed it to 127\.0\.0\.1:5068/,
    );
    const bad = [{ image: IDENTITY, repository: `${REGISTRY}/identity`, digest: "sha256:short" }];
    expect(() => pinChartValues(VALUES_SOURCE, bad, { appVersion: VERSION, rewriteRepositories: false })).toThrow(/not a sha256 digest/);
  });

  test("edits nested keys only at their own path, keeps inline comments, and adds missing keys", () => {
    const source = [
      "# header",
      "serviceDefaults:",
      "  image:",
      "    registry: ghcr.io/laysi/trustiot-v2",
      '    digest: "" # not this one',
      "    tag: \"\"",
      "  ports:",
      "    - name: http",
      "      digest: x",
      "  notes: |",
      "    services:",
      "      identity:",
      "services:",
      "  # the identity service",
      "  identity:",
      "    enabled: false",
      "    image:",
      "      pullPolicy: Always",
      "  other:",
      "    enabled: false",
      "postgres:",
      "  bundled:",
      "    image:",
      "      repository: ghcr.io/laysi/trustiot-v2/postgres",
      "      tag: ''",
      '      digest: ""   # pinned by the release',
      "",
    ].join("\n");
    const pins = [
      { image: IDENTITY, repository: `${REGISTRY}/identity`, digest: digest("a") },
      { image: { name: "other", kind: "service" as const }, repository: `${REGISTRY}/other`, digest: digest("c") },
      { image: POSTGRES, repository: `${REGISTRY}/postgres`, digest: digest("b") },
    ];
    const pinned = pinChartValues(source, pins, { appVersion: VERSION, rewriteRepositories: false });
    expect(pinned).toContain(`    image:\n      # Added by the release (scripts/release/chart-images.ts).\n      digest: ${digest("a")}\n      pullPolicy: Always`);
    expect(pinned).toContain(`  other:\n    # Added by the release (scripts/release/chart-images.ts).\n    image:\n      digest: ${digest("c")}\n    enabled: false`);
    expect(pinned).toContain(`      digest: ${digest("b")}   # pinned by the release`);
    expect(pinned).toContain('    digest: "" # not this one');
    expect(pinned).toContain("      digest: x");
    expect(pinned).toContain("  notes: |\n    services:\n      identity:\n");
  });

  test("refuses a target that is not a block mapping or appears twice", () => {
    const flow = 'serviceDefaults:\n  image: {registry: r, repository: "", tag: "", digest: ""}\nservices:\n  identity:\n    image: {}\n';
    expect(() => pinChartValues(flow, [{ image: IDENTITY, repository: "r/identity", digest: digest("a") }], { appVersion: VERSION, rewriteRepositories: false })).toThrow(
      /services\.identity\.image in values\.yaml is not a block mapping/,
    );
  });
});

describe("chartPinProblems", () => {
  const pins = repositoryPins();
  const pinned = parseValues(pinChartValues(VALUES_SOURCE, pins, { appVersion: VERSION, rewriteRepositories: false }), "pinned");

  test("accepts a chart that renders every pinned reference", () => {
    expect(pins.length).toBeGreaterThanOrEqual(2);
    expect(chartPinProblems(pinned, [], pins, VERSION)).toEqual([]);
  });

  test("names every image whose digest is not this release's (what verify refuses)", () => {
    const wrong = pins.map((pin) => (pin.image.name === "postgres" ? { ...pin, digest: digest("f") } : pin));
    expect(chartPinProblems(pinned, [], wrong, VERSION)).toEqual([
      `values.yaml: postgres is ${pinnedReference(pins.find((pin) => pin.image.name === "postgres")!, VERSION)}, not ${REGISTRY}/postgres:${VERSION}@${digest("f")}`,
    ]);
    expect(chartPinProblems(VALUES, [], pins, VERSION).length).toBe(pins.length);
  });

  test("catches an overlay that unpins, retags or moves an image", () => {
    const overlays = [
      { file: "empty-digest.yaml", values: { services: { identity: { image: { digest: "" } } } } },
      { file: "null-digest.yaml", values: { postgres: { bundled: { image: { digest: null } } } } },
      { file: "tag.yaml", values: { serviceDefaults: { image: { tag: "latest" } } } },
      { file: "registry.yaml", values: { serviceDefaults: { image: { registry: "mirror.test/x" } } } },
    ];
    const problems = chartPinProblems(pinned, overlays, pins, VERSION);
    for (const overlay of overlays) {
      expect(problems.some((problem) => problem.startsWith(`values.yaml + ${overlay.file}:`))).toBe(true);
    }
  });

  test("no shipped shape file and no values file of a GitOps tree overrides an image of the release", () => {
    const overlay = (file: string) => ({ file, values: parseValues(readFileSync(join(ROOT, file), "utf8"), file) });
    const overlays = [
      ...SHAPE_VALUES_FILES.map((file) => ({ file: `deploy/chart/${file}`, values: parseValues(readFileSync(join(CHART, file), "utf8"), file) })),
      ...CLUSTERS.map((cluster) => ({
        file: shapeValuesFile(cluster),
        values: parseValues(parseShapeValues(readFileSync(join(ROOT, shapeValuesFile(cluster)), "utf8")).source, shapeValuesFile(cluster)),
      })),
      overlay(`${appDir("saas")}/values-instance.yaml`),
      overlay(`${appDir("demo")}/values-demo.yaml`),
    ];
    expect(chartPinProblems(pinned, overlays, pins, VERSION)).toEqual([]);
  });
});

/** Every `image:` string in rendered manifests. */
function renderedImages(manifests: string): string[] {
  const images: string[] = [];
  const walk = (node: unknown): void => {
    if (Array.isArray(node)) {
      node.forEach(walk);
    } else if (isMap(node)) {
      for (const [key, value] of Object.entries(node)) {
        if (key === "image" && typeof value === "string") {
          images.push(value);
        } else {
          walk(value);
        }
      }
    }
  };
  for (const doc of manifests.split(/^---$/m)) {
    walk(Bun.YAML.parse(doc));
  }
  return images;
}

describe("packagePinnedChart", () => {
  test(
    "packages the same archive every time, with the pins and nothing else changed",
    async () => {
      const helm = await protoTool("helm");
      const pins = repositoryPins();
      const options = { helm, chartDir: CHART, appVersion: VERSION, sourceDateEpoch: 1_760_000_000, rewriteRepositories: false };
      const first = await packagePinnedChart(pins, { ...options, workDir: workDir() });
      const second = await packagePinnedChart(pins, { ...options, workDir: workDir() });
      expect(sha256Digest(new Uint8Array(readFileSync(first)))).toBe(sha256Digest(new Uint8Array(readFileSync(second))));
      // The repository's chart is not touched.
      expect(readFileSync(join(CHART, "values.yaml"), "utf8")).toBe(VALUES_SOURCE);

      // What verify reads back from the archive.
      const unpacked = await unpackChart(systemTool("tar"), first, join(workDir(), "unpacked"), "trustiot");
      const { values, shapes } = readChartValues(unpacked);
      expect(chartPinProblems(values, shapes, pins, VERSION)).toEqual([]);
      const other = pins.map((pin) => ({ ...pin, digest: digest("e") }));
      expect(chartPinProblems(values, shapes, other, VERSION)).toHaveLength(other.length * (1 + SHAPE_VALUES_FILES.length));
    },
    HELM_TEST_MS,
  );

  for (const shape of ["saas", "onprem"]) {
    test(
      `helm template of the packaged chart with values-${shape}.yaml renders every pinned reference`,
      async () => {
        const helm = await protoTool("helm");
        const pins = repositoryPins();
        const tgz = await packagePinnedChart(pins, {
          helm,
          chartDir: CHART,
          workDir: workDir(),
          appVersion: VERSION,
          sourceDateEpoch: 1_760_000_000,
          rewriteRepositories: false,
        });
        const services = pins.filter((pin) => pin.image.kind === "service").flatMap((pin) => ["--set", `services.${pin.image.name}.enabled=true`]);
        // The AI gateway renders only with a provider key and a model role (deploy/chart/README.md,
        // "AI 閘道"); on-prem's instance values set none.
        const aiGateway = ["--set", "ai.providers.anthropic.apiKeySecret.name=trustiot-anthropic", "--set-json", 'ai.roles.test={"models":["anthropic/claude-test"]}'];
        // ingest renders only with the in-cluster MQTT broker (deploy/chart/README.md, "ingest").
        const broker = ["--set", "mqttBroker.enabled=true"];
        const { stdout } = await run([
          helm,
          "template",
          "trustiot",
          tgz,
          "--namespace",
          "trustiot",
          "-f",
          join(CHART, `values-${shape}.yaml`),
          "-f",
          join(CHART, "tests", "values", `${shape}-instance.yaml`),
          ...services,
          ...aiGateway,
          ...broker,
        ]);
        const images = renderedImages(stdout);
        for (const pin of pins) {
          expect(images).toContain(pinnedReference(pin, VERSION));
          // No object of the release names the image without its digest.
          expect(images.filter((image) => image.startsWith(`${pin.repository}:`) && image !== pinnedReference(pin, VERSION))).toEqual([]);
        }
      },
      HELM_TEST_MS,
    );
  }
});
