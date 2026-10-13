// Applies the packageRules of renovate.json5 with Renovate's own rule matcher to dependencies
// that the local dry run cannot look up without a GitHub token, and prints the settings that
// decide whether an update is proposed.
const D = "/usr/local/renovate/dist";
const { applyPackageRules } = await import(`${D}/util/package-rules/index.js`);
const fs = await import("node:fs");
const JSON5 = (await import("/usr/local/renovate/node_modules/json5/lib/index.js")).default;

// Each dependency as its manager extracts it (fields from the dry-run log and the managers' code).
const deps = [
  { manager: "proto", packageFile: ".prototools", depName: "go", packageName: "golang/go", datasource: "github-tags", currentValue: "1.27.2" },
  { manager: "proto", packageFile: ".prototools", depName: "bun", packageName: "oven-sh/bun", datasource: "github-releases", currentValue: "1.4.2" },
  { manager: "flux", packageFile: "deploy/flux/clusters/saas/flux-system/gotk-components.yaml", depName: "fluxcd/flux2", packageName: "fluxcd/flux2", datasource: "github-releases", currentValue: "v2.9.6" },
  { manager: "regex", packageFile: ".prototools", depName: "fluxcd/flux2", packageName: "fluxcd/flux2", datasource: "github-releases", currentValue: "2.9.6" },
  { manager: "regex", packageFile: ".prototools", depName: "kubernetes/kubernetes", packageName: "kubernetes/kubernetes", datasource: "github-tags", currentValue: "1.36.5" },
  { manager: "github-actions", packageFile: ".github/workflows/ci.yml", depName: "actions/checkout", packageName: "actions/checkout", datasource: "github-tags", currentValue: "v7.0.1", currentDigest: "3d3c42e5aac5ba805825da76410c181273ba90b1" },
  { manager: "github-actions", packageFile: ".github/workflows/ci.yml", depName: "ubuntu", packageName: "ubuntu", datasource: "github-runners", currentValue: "24.04-arm" },
  { manager: "flux", packageFile: "deploy/flux/clusters/saas/trustiot/chart.yaml", depName: "ghcr.io/example/oci-source", packageName: "ghcr.io/example/oci-source", datasource: "docker", currentValue: "1.0.0" },
  { manager: "regex", packageFile: ".github/workflows/nightly.yml", depName: "rancher/k3s", packageName: "rancher/k3s", datasource: "docker", currentValue: "v1.36.5-k3s1" },
];

for (const [label, path] of [["before", "/repo-before/renovate.json5"], ["after", "/repo-after/renovate.json5"]]) {
  const repo = JSON5.parse(fs.readFileSync(path, "utf8"));
  console.log(`== ${label}`);
  for (const dep of deps) {
    // Top-level settings, then the custom regex manager's own default (pinDigests: false), then rules.
    const base = { pinDigests: repo.pinDigests, enabled: true, packageRules: repo.packageRules, ...(dep.manager === "regex" ? { pinDigests: false } : {}), ...dep };
    const out = await applyPackageRules(base);
    console.log(`${dep.manager.padEnd(14)} ${dep.depName.padEnd(28)} ${dep.datasource.padEnd(15)} enabled=${out.enabled} pinDigests=${out.pinDigests}${out.versioning ? ` versioning=${out.versioning}` : ""}${out.allowedVersions ? ` allowedVersions=${out.allowedVersions}` : ""}`);
  }
}
