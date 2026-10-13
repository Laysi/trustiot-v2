// Makes a copy of the repository look as if some pinned tags had moved since they were pinned:
// every copy of the listed digests is replaced by the same made-up digest (the first 8 hex digits
// zeroed), so Renovate proposes a digest update for each and its branch shows which files it edits.
// Usage: bun make-stale.ts <snapshot dir>
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const root = Bun.argv[2]!;
const DIGESTS = {
  "gcr.io/distroless/cc-debian13:nonroot": "e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2",
  "docker.io/library/eclipse-mosquitto:2.1.2-alpine": "38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408",
  "docker.io/traefik/whoami:v1.12.0": "c4717a8d1f0134a7444e24f881160e033991f23027c6c5a9a3f8fd22e70d1d44",
  "docker.io/otel/opentelemetry-collector-contrib:0.161.0": "fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1",
  "docker.io/library/registry (3.1.2 and 3)": "ddf754342cfc8acc51a56d5d0ab6af06826461864460636d8bd5c546dab2a7b8",
  "docker.io/apache/age:release_PG18_1.8.0": "47a0b054c3663a3e64a25fc0c5a982f1b58348019274b326a691cdf77f78eb58",
};
const stale = (digest: string) => `00000000${digest.slice(8)}`;
const counts = new Map<string, string[]>();
function walk(dir: string) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      walk(path);
      continue;
    }
    let text = readFileSync(path, "utf8");
    let changed = false;
    for (const [image, digest] of Object.entries(DIGESTS)) {
      if (!text.includes(digest)) continue;
      text = text.replaceAll(digest, stale(digest));
      changed = true;
      counts.set(image, [...(counts.get(image) ?? []), path.slice(root.length + 1)]);
    }
    if (changed) writeFileSync(path, text);
  }
}
walk(root);
for (const [image, files] of counts) console.log(`${image}: ${files.sort().join(", ")}`);
