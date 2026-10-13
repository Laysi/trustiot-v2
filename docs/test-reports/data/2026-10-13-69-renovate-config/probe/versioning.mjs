// Runs Renovate's own versioning modules and release filter (the code `renovate` runs during a
// lookup) on k3s and Kubernetes tag lists, to show which tags each config could propose.
const D = "/usr/local/renovate/dist";
const { get } = await import(`${D}/modules/versioning/index.js`);
const { filterVersions } = await import(`${D}/workers/repository/process/lookup/filter.js`);
const fs = await import("node:fs");
const JSON5 = (await import("/usr/local/renovate/node_modules/json5/lib/index.js")).default;

function rulesFor(config, packageName) {
  return Object.assign({}, ...config.packageRules.filter((r) => r.matchPackageNames?.includes(packageName)));
}

function run(label, packageName, rule, current, tags) {
  const versioning = rule.versioning ?? "semver-coerced";
  const api = get(versioning);
  const releases = tags.map((version) => ({ version }));
  const kept = filterVersions(
    { depName: packageName, allowedVersions: rule.allowedVersions, ignoreUnstable: true, ignoreDeprecated: true, versioning },
    current,
    undefined,
    releases,
    api,
  );
  const invalid = tags.filter((t) => !api.isVersion(t));
  console.log(`${label}: ${packageName} versioning=${versioning} allowedVersions=${rule.allowedVersions}`);
  console.log(`  current ${current}; not a version under this versioning: ${invalid.join(", ") || "(none)"}`);
  console.log(`  can be proposed: ${kept.map((r) => r.version).join(", ") || "(nothing)"}`);
}

const lines = (file) => fs.readFileSync(file, "utf8").trim().split("\n");
const sets = [
  // The real v1.36 and v1.37 tags on 2026-10-13: rancher/k3s on Docker Hub (skopeo list-tags) and
  // kubernetes/kubernetes on GitHub (git ls-remote), the latter with the "v" stripped as the
  // .prototools annotation's extractVersion does.
  ["real tags 2026-10-13", lines("/probe/k3s-tags.txt"), lines("/probe/k8s-tags.txt")],
  // Tags that do not exist yet but will: the next patch's release candidates and final release,
  // and a per-architecture tag.
  [
    "next patch (synthetic)",
    ["v1.36.5-k3s1", "v1.36.5-k3s1-amd64", "v1.36.5-k3s2", "v1.36.6-rc1-k3s1", "v1.36.6-k3s1", "v1.37.0-k3s1"],
    ["1.36.5", "1.36.6-alpha.0", "1.36.6-beta.0", "1.36.6-rc.0", "1.36.6-rc.1", "1.36.6", "1.37.0"],
  ],
];

for (const [setLabel, k3sTags, k8sTags] of sets) {
  console.log(`#### ${setLabel}`);
  for (const [label, path] of [["before", "/repo-before/renovate.json5"], ["after", "/repo-after/renovate.json5"]]) {
    if (!fs.existsSync(path)) continue;
    const config = JSON5.parse(fs.readFileSync(path, "utf8"));
    run(label, "rancher/k3s", rulesFor(config, "rancher/k3s"), "v1.36.5-k3s1", k3sTags);
    run(label, "kubernetes/kubernetes", rulesFor(config, "kubernetes/kubernetes"), "1.36.5", k8sTags);
  }
}
