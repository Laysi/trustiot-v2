// The demo environment's public files name no private host (Laysi/trustiot-v2#466): the demo host
// names and their zone stay in the private Cloudflare change list
// (docs/architecture/secrets.md, 「公開 repo 的資訊規則」).
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { ROOT } from "./lib.ts";

/**
 * The files this rule covers: the runbook, everything under scripts/demo, the other documents the
 * demo environment's tunnel changes (Laysi/trustiot-v2#466), and the demo environment's test reports.
 */
export const DEMO_DOCS = [
  "docs/runbooks/demo-environment.md",
  "deploy/host/README.md",
  "docs/README.md",
  "docs/architecture/secrets.md",
  "docs/runbooks/secrets.md",
  "docs/test-reports/2026-10-21-466-demo-tunnel-access.md",
  "docs/test-reports/2026-10-22-468-demo-mail-anthropic-keys.md",
  "docs/test-reports/2026-11-02-961-demo-web.md",
];

/** Directories whose every file the rule covers: the scripts and the test report's raw data. */
export const DEMO_DIRS = ["scripts/demo", "docs/test-reports/data/2026-10-21-466-demo-tunnel-access"];

export function publicFiles(root: string = ROOT): string[] {
  const walk = (dir: string): string[] =>
    readdirSync(dir).flatMap((name) => {
      const path = join(dir, name);
      return statSync(path).isDirectory() ? walk(path) : [path];
    });
  return [...DEMO_DOCS.map((doc) => join(root, doc)), ...DEMO_DIRS.flatMap((dir) => walk(join(root, dir)))];
}

/**
 * Host names that may appear, each with its reason. A name matches when it is the entry or ends
 * with "." and the entry. Reserved names (RFC 2606: .test, .invalid, example.com) never resolve.
 */
export const ALLOWED_NAMES: Record<string, string> = {
  "example.com": "RFC 2606 example names in tests",
  "cloudflareaccess.com": "the Access login domain the probes recognize (<team>.cloudflareaccess.com)",
  "cfargotunnel.com": "the target of a tunnel's DNS records",
  "pkg.cloudflare.com": "Cloudflare's RPM repository, pinned in deploy/host",
  "developers.cloudflare.com": "links to Cloudflare's documentation",
  "github.com": "links to this repository",
  "k8s.io": "Kubernetes API group names (rbac.authorization.k8s.io and others)",
  "cattle.io": "the API group of k3s's HelmChart (helm.cattle.io)",
  "kubernetes.io": "Kubernetes label and annotation prefixes (app.kubernetes.io/...)",
  "istio.io": "Istio's API groups and label prefixes",
  "fluxcd.io": "Flux's annotation prefixes (reconcile.fluxcd.io/...)",
  "cnpg.io": "CloudNativePG's API group (postgresql.cnpg.io)",
  "trustiot.io": "the chart's own label and annotation prefix (trustiot.io/filled-by and others)",
  "rpm.rancher.io": "the k3s RPM repository, pinned in deploy/host",
  "ghcr.io": "the GitHub container registry",
  "hosts.app": "the chart values key hosts.app (deploy/chart/values.yaml), not a host name",
  "quay.io": "the registry of curl's official image, pinned by digest in test-mail.ts",
  "resend.com": "Resend's SMTP host in values-demo.yaml (smtp.resend.com) and links to its documentation",
  "platform.claude.com": "links to Anthropic's Console documentation",
  "api.anthropic.com": "the Anthropic API host in the key check of the #468 test report",
};

/**
 * Names with a public top-level domain, in any letter case (DNS ignores case); `.test` and
 * `.invalid` are reserved and never match. Code must not read a property named like a top-level
 * domain with a dot (write `hosts["app"]`), or this check reports it.
 */
const NAME = /(?<![\w.-])((?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+(?:ai|app|biz|cloud|co|com|dev|info|io|me|net|one|org|site|tech|tw|xyz))(?![\w(-])/gi;

/** An escape in source code (`\t`, `\n`, `\r`) separates words; its letter is not part of a name. */
const ESCAPE = /\\[nrt]/g;

export function privateNames(text: string): string[] {
  return [...text.replace(ESCAPE, " ").matchAll(NAME)].map((m) => m[1] ?? "")
    .filter((name) => {
      const lower = name.toLowerCase();
      return !Object.keys(ALLOWED_NAMES).some((allowed) => lower === allowed || lower.endsWith(`.${allowed}`));
    });
}

describe("public text of the demo environment", () => {
  test("names only allowed hosts", () => {
    const found = publicFiles().flatMap((file) =>
      readFileSync(file, "utf8")
        .split("\n")
        .flatMap((line, index) => privateNames(line).map((name) => `${relative(ROOT, file)}:${index + 1}: ${name}`)),
    );
    expect(found).toEqual([]);
  });

  test("the test reports and their data hold no object ID and no cf-ray", () => {
    const report = [...DEMO_DOCS, ...DEMO_DIRS].filter((path) => path.startsWith("docs/test-reports/"));
    const files = publicFiles().filter((file) => report.some((path) => relative(ROOT, file).startsWith(path)));
    expect(files.length).toBeGreaterThan(1);
    // Tunnel, application and policy IDs are UUIDs; account and zone IDs are 32 hex digits; a
    // cf-ray is 16 hex digits and the code of the Cloudflare location that served it.
    const ids = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b|\b[0-9a-f]{32}\b|\b[0-9a-f]{16}-[A-Z]{3}\b/gi;
    const found = files.flatMap((file) => [...readFileSync(file, "utf8").matchAll(ids)].map((m) => `${relative(ROOT, file)}: ${m[0]}`));
    expect(found).toEqual([]);
  });

  test("the runbook has each section once", () => {
    const headings = readFileSync(join(ROOT, "docs/runbooks/demo-environment.md"), "utf8")
      .replace(/```[\s\S]*?```/g, "")
      .split("\n")
      .filter((line) => /^#{1,3} /.test(line));
    expect(headings.length).toBeGreaterThan(5);
    expect(headings.filter((h, i) => headings.indexOf(h) !== i)).toEqual([]);
  });

  test("every runbook section the scripts name exists", () => {
    const runbook = readFileSync(join(ROOT, "docs/runbooks/demo-environment.md"), "utf8").replace(/```[\s\S]*?```/g, "");
    const sections = new Set(runbook.split("\n").flatMap((line) => /^#{2,3} (.+)$/.exec(line)?.[1] ?? []));
    const named = publicFiles()
      .filter((file) => file.endsWith(".ts"))
      .flatMap((file) => [...readFileSync(file, "utf8").matchAll(/demo-environment\.md\s*「([^」]+)」/g)].map((m) => `${relative(ROOT, file)}: ${m[1]}`));
    expect(named.length).toBeGreaterThan(2);
    expect(named.filter((ref) => !sections.has(ref.slice(ref.indexOf(": ") + 2)))).toEqual([]);
  });

  test("the check finds a host name of a private zone, and leaves reserved and allowed names", () => {
    // Built at run time, so this file passes its own check.
    const zone = ["private-zone", "ai"].join(".");
    expect(privateNames(`app 主機名稱（host-a.${zone}）`)).toEqual([`host-a.${zone}`]);
    expect(privateNames(`sender.${zone}, https://host-b.${zone}/mqtt`)).toEqual([`sender.${zone}`, `host-b.${zone}`]);
    expect(privateNames(`都是 ${zone} 的一層子網域`)).toEqual([zone]);
    // DNS ignores letter case, so neither does the check.
    expect(privateNames(`https://Host-B.${zone.toUpperCase()}/mqtt`)).toEqual([`Host-B.${zone.toUpperCase()}`]);
    expect(privateNames("GW.Example.COM Team-X.CloudflareAccess.com")).toEqual([]);
    expect(privateNames("gw.example.com app.test x.invalid team-x.cloudflareaccess.com <id>.cfargotunnel.com https://pkg.cloudflare.com/cloudflared/rpm")).toEqual([]);
    // Code is not a host name: properties and calls.
    expect(privateNames("crypto.subtle.encrypt(x); HOSTS[\"app\"]; plan.cloudflared.candidate; Bun.file(p)")).toEqual([]);
    // An escape in source code separates the name after it, which is still checked.
    expect(privateNames(String.raw`"v1\tSecret\t\tkubernetes.io/service-account-token\n"`)).toEqual([]);
    expect(privateNames(String.raw`"a\thost-c.${zone}\n", "\nhost-d.${zone}\r"`)).toEqual([`host-c.${zone}`, `host-d.${zone}`]);
  });
});
