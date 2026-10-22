// The login roles the local migration Job binds a password to (lib/product-logins.ts,
// Laysi/trustiot-v2#407) against what the product has: the services of `PRODUCT`
// (tools/migrate/src/registry.rs) and the platform roles the chart binds
// ("trustiot.migrate.platformLogins" in deploy/chart/templates/_migrate.tpl). A service or a bound
// platform role that the local list does not have fails here, with the file to change.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { PLATFORM_LOGIN_SERVICES, PRODUCT_SERVICES, productLogins } from "../lib/product-logins.ts";

const REPO = join(import.meta.dir, "..", "..");
const REGISTRY_PATH = "tools/migrate/src/registry.rs";
const CHART_PATH = "deploy/chart/templates/_migrate.tpl";
const LIST_PATH = "scripts/lib/product-logins.ts";

interface RegistryService {
  name: string;
  platformRole: boolean;
  consumes: string[];
}

/**
 * The services of `PRODUCT`, in order. The parser reads the registry's literal shape and refuses
 * what it does not understand, so a change of that shape fails loudly instead of passing with
 * fewer services.
 */
export function registryServices(source: string): RegistryService[] {
  const start = source.indexOf("pub const PRODUCT: Registry");
  if (start < 0) throw new Error(`${REGISTRY_PATH} has no \`pub const PRODUCT: Registry\`; update ${import.meta.file}`);
  const product = source
    .slice(start)
    .split("\n")
    .filter((line) => !line.trimStart().startsWith("//"))
    .join("\n");
  const shape =
    /\bService\s*\{\s*name:\s*"([^"]+)",\s*platform_role:\s*(true|false),\s*outbox:\s*Outbox\s*\{\s*producer:\s*(?:true|false),\s*consumes:\s*&\[([^\]]*)\],?\s*\}/g;
  const services = [...product.matchAll(shape)].map((match) => ({
    name: match[1]!,
    platformRole: match[2] === "true",
    consumes: [...match[3]!.matchAll(/"([^"]+)"/g)].map((quoted) => quoted[1]!),
  }));
  const declared = [...product.matchAll(/\bService\s*\{/g)].length;
  if (services.length !== declared || services.length === 0) {
    throw new Error(
      `${import.meta.file} understands ${services.length} of the ${declared} \`Service {\` entries of PRODUCT in ` +
        `${REGISTRY_PATH}: update the parser in this test for the new shape`,
    );
  }
  return services;
}

/** The services in the chart's `trustiot.migrate.platformLogins` list. */
export function chartPlatformLogins(source: string): string[] {
  const header = 'define "trustiot.migrate.platformLogins" -}}\n';
  const start = source.indexOf(header);
  if (start < 0) throw new Error(`${CHART_PATH} has no trustiot.migrate.platformLogins; update ${import.meta.file}`);
  const body = source.slice(start + header.length).split("{{- end -}}")[0]!;
  const quoted = body.split('"').filter((_, index) => index % 2 === 1);
  if (!body.includes("{{- list") || body.includes("(")) {
    throw new Error(`trustiot.migrate.platformLogins in ${CHART_PATH} is no longer a literal list; update ${import.meta.file}: ${body.trim()}`);
  }
  return quoted;
}

const roleOf = (service: string, kind: "app" | "platform") => `${service.replaceAll("-", "_")}_${kind}`;

/**
 * What differs between the local list and the product, one message per difference, each saying
 * which file to change. Empty when the local Job binds exactly the roles the chart binds.
 */
export function drift(
  registrySource: string,
  chartSource: string,
  localServices: readonly string[],
  localPlatform: readonly string[],
): string[] {
  const registry = registryServices(registrySource);
  const chart = chartPlatformLogins(chartSource);
  const messages: string[] = [];
  const registered = registry.map((service) => service.name);
  for (const name of registered.filter((candidate) => !localServices.includes(candidate))) {
    messages.push(
      `${REGISTRY_PATH} has the service \`${name}\` (login role ${roleOf(name, "app")}), but PRODUCT_SERVICES in ` +
        `${LIST_PATH} does not list it, so \`bun scripts/dev.ts up\` would leave the role NOLOGIN. ` +
        `Add "${name}" to PRODUCT_SERVICES in ${LIST_PATH}.`,
    );
  }
  for (const name of localServices.filter((candidate) => !registered.includes(candidate))) {
    messages.push(
      `PRODUCT_SERVICES in ${LIST_PATH} lists \`${name}\`, which ${REGISTRY_PATH} does not register: the Job would refuse ` +
        `its password binding. Remove it from ${LIST_PATH} or add it to PRODUCT.`,
    );
  }
  for (const name of chart.filter((candidate) => !localPlatform.includes(candidate))) {
    messages.push(
      `${CHART_PATH} binds ${roleOf(name, "platform")} (trustiot.migrate.platformLogins), but PLATFORM_LOGIN_SERVICES in ` +
        `${LIST_PATH} does not list \`${name}\`, so the local environment would leave it NOLOGIN. ` +
        `Add "${name}" to PLATFORM_LOGIN_SERVICES in ${LIST_PATH}.`,
    );
  }
  for (const name of localPlatform.filter((candidate) => !chart.includes(candidate))) {
    messages.push(
      `PLATFORM_LOGIN_SERVICES in ${LIST_PATH} lists \`${name}\`, but ${CHART_PATH} does not bind ${roleOf(name, "platform")}: ` +
        `the local environment must bind what the chart binds. Remove it from ${LIST_PATH} or add it to ` +
        `trustiot.migrate.platformLogins.`,
    );
  }
  for (const name of localPlatform) {
    const service = registry.find((candidate) => candidate.name === name);
    if (service && !service.platformRole) {
      messages.push(
        `PLATFORM_LOGIN_SERVICES in ${LIST_PATH} lists \`${name}\`, but ${REGISTRY_PATH} creates no platform role for it ` +
          `(\`platform_role: false\`): the Job would refuse the binding. Remove it from ${LIST_PATH}.`,
      );
    }
  }
  return messages;
}

/**
 * The registry takes each service's name and producer flag from the service's own `src/schema.rs`
 * (`<name>_schema::SCHEMA.service`, Laysi/trustiot-v2#402). Replace those references with the
 * literals of the file the `#[path]` attribute names, so the parser below keeps reading literals
 * and a reference it cannot resolve fails instead of being skipped.
 */
export function resolveSchemaRefs(source: string, read: (path: string) => string): string {
  const files = new Map<string, string>();
  for (const match of source.matchAll(/#\[path = "([^"]+)"\]\s*mod\s+(\w+);/g)) {
    files.set(match[2]!, read(match[1]!));
  }
  const resolved = source.replace(/\b(\w+)::SCHEMA\.(service|outbox_producer)\b/g, (_, mod: string, field: string) => {
    const schema = files.get(mod);
    if (schema === undefined) throw new Error(`${REGISTRY_PATH} refers to ${mod}::SCHEMA without a #[path] module; update ${import.meta.file}`);
    const value = field === "service" ? /\bservice:\s*("[^"]+")/.exec(schema)?.[1] : /\boutbox_producer:\s*(true|false)/.exec(schema)?.[1];
    if (value === undefined) throw new Error(`${mod}'s schema.rs has no literal ${field}; update ${import.meta.file}`);
    return value;
  });
  return resolved;
}

const registrySource = resolveSchemaRefs(readFileSync(join(REPO, REGISTRY_PATH), "utf8"), (path) =>
  readFileSync(join(REPO, "tools/migrate/src", path), "utf8"),
);
const chartSource = readFileSync(join(REPO, CHART_PATH), "utf8");

describe("the local login roles against the product", () => {
  test("bind every login role the chart binds, and no other", () => {
    const messages = drift(registrySource, chartSource, PRODUCT_SERVICES, PLATFORM_LOGIN_SERVICES);
    if (messages.length > 0) throw new Error(`\n${messages.join("\n")}`);
  });

  test("the parsers read the real files, so the comparison above cannot pass on an empty read", () => {
    const registry = registryServices(registrySource);
    expect(registry.find((service) => service.name === "identity")).toEqual({ name: "identity", platformRole: true, consumes: [] });
    expect(registry.find((service) => service.name === "assets")).toEqual({ name: "assets", platformRole: true, consumes: ["identity"] });
    expect(chartPlatformLogins(chartSource)).toContain("assets");
  });

  test("every role the local Job binds is a login role the registry creates", () => {
    const registry = registryServices(registrySource);
    for (const login of productLogins()) {
      const service = registry.find((candidate) => candidate.name === login.service);
      expect(service, `${login.role}: ${login.service} is not in PRODUCT`).toBeDefined();
      if (login.kind === "platform") expect(service!.platformRole, login.role).toBe(true);
    }
  });
});

describe("a change of the product that the local list does not follow", () => {
  // The local lists of the synthetic product below (identity and assets), apart from the real
  // lists, so a service added to the real product does not change what these cases test.
  const LOCAL_SERVICES: readonly string[] = ["identity", "assets"];
  const LOCAL_PLATFORM: readonly string[] = ["assets"];
  const service = (name: string, platformRole: boolean, consumes: string[] = []) =>
    `        Service {\n            name: "${name}",\n            platform_role: ${platformRole},\n            outbox: Outbox {\n                producer: false,\n                consumes: &[${consumes.map((one) => `"${one}"`).join(", ")}],\n            },\n            migrations: &[],\n        },\n`;
  const registryWith = (...entries: string[]) =>
    `// Service { name: "commented-out", platform_role: true }\npub const PRODUCT: Registry = Registry {\n    services: &[\n${entries.join("")}    ],\n};\n`;
  const chartWith = (...names: string[]) =>
    `{{/* comment with "quoted" text */}}\n{{- define "trustiot.migrate.platformLogins" -}}\n{{- list ${names.map((name) => `"${name}"`).join(" ")} | toYaml -}}\n{{- end -}}\n\n{{- define "other" -}}"never read"{{- end -}}\n`;

  test("a service added to PRODUCT fails, naming the file to change", () => {
    const messages = drift(
      registryWith(service("identity", true), service("assets", true, ["identity"]), service("telemetry-query", false)),
      chartWith("assets"),
      LOCAL_SERVICES,
      LOCAL_PLATFORM,
    );
    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("telemetry-query");
    expect(messages[0]).toContain("telemetry_query_app");
    expect(messages[0]).toContain(`PRODUCT_SERVICES in ${LIST_PATH}`);
  });

  test("a platform role the chart starts to bind fails, naming the file to change", () => {
    const messages = drift(
      registryWith(service("identity", true), service("assets", true, ["identity"])),
      chartWith("assets", "identity"),
      LOCAL_SERVICES,
      LOCAL_PLATFORM,
    );
    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("identity_platform");
    expect(messages[0]).toContain(`PLATFORM_LOGIN_SERVICES in ${LIST_PATH}`);
  });

  test("a local role the product does not have fails the other way round", () => {
    const registry = registryWith(service("identity", true), service("assets", true, ["identity"]));
    expect(drift(registry, chartWith(), LOCAL_SERVICES, LOCAL_PLATFORM).join("\n")).toContain(
      "does not bind assets_platform",
    );
    expect(drift(registry, chartWith("assets"), [...LOCAL_SERVICES, "ghost"], LOCAL_PLATFORM).join("\n")).toContain(
      "which tools/migrate/src/registry.rs does not register",
    );
  });

  test("agrees when both follow", () => {
    expect(
      drift(
        registryWith(service("identity", true), service("assets", true, ["identity"]), service("telemetry-query", true, ["identity"])),
        chartWith("assets", "telemetry-query"),
        [...LOCAL_SERVICES, "telemetry-query"],
        [...LOCAL_PLATFORM, "telemetry-query"],
      ),
    ).toEqual([]);
  });

  test("a registry the parser does not understand fails instead of passing", () => {
    expect(() => registryServices("pub const PRODUCT: Registry = Registry { services: &[ Service::new(\"identity\") ] };")).toThrow(
      "update the parser",
    );
    expect(() => registryServices("const OTHER: u8 = 1;")).toThrow("no `pub const PRODUCT: Registry`");
    expect(() => chartPlatformLogins("{{- define \"other\" -}}{{- end -}}")).toThrow("no trustiot.migrate.platformLogins");
  });
});

describe("the logins", () => {
  test("are each service's own role, then its platform role when the chart binds it", () => {
    expect(productLogins()).toEqual([
      { service: "identity", kind: "app", role: "identity_app", secret: "dev-identity-password", stateKey: "identity" },
      { service: "assets", kind: "app", role: "assets_app", secret: "dev-assets-password", stateKey: "assets" },
      { service: "assets", kind: "platform", role: "assets_platform", secret: "dev-assets-password", stateKey: "assets-platform" },
      {
        service: "telemetry-query",
        kind: "app",
        role: "telemetry_query_app",
        secret: "dev-telemetry-query-password",
        stateKey: "telemetry-query",
      },
      {
        service: "telemetry-query",
        kind: "platform",
        role: "telemetry_query_platform",
        secret: "dev-telemetry-query-password",
        stateKey: "telemetry-query-platform",
      },
      { service: "certs", kind: "app", role: "certs_app", secret: "dev-certs-password", stateKey: "certs" },
    ]);
  });

  test("write a dashed service name as the registry and the chart do", () => {
    const logins = productLogins(["telemetry-query"], ["telemetry-query"]);
    expect(logins.map((login) => login.role)).toEqual(["telemetry_query_app", "telemetry_query_platform"]);
    expect(logins.map((login) => login.stateKey)).toEqual(["telemetry-query", "telemetry-query-platform"]);
    expect(logins.every((login) => login.secret === "dev-telemetry-query-password")).toBe(true);
  });
});
