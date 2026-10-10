// Finding the seed files. `scripts/seed/<store>/*.sql` is the platform's smoke data;
// `services/<name>/seed/<store>/*.sql` is a service's own development data. A service adds seed
// data by dropping files there; no script change is needed.
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";

export const STORES = ["postgres", "clickhouse"] as const;
export type Store = (typeof STORES)[number];

export interface SeedFile {
  store: Store;
  /** `platform` or the service's directory name. */
  origin: string;
  path: string;
}

function sqlFiles(dir: string): string[] {
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true })
    .filter((entry) => entry.isFile() && entry.name.endsWith(".sql"))
    .map((entry) => entry.name)
    .sort();
}

/** Seed files in the order they are applied: the platform first, then each service by name. */
export function discoverSeeds(repoRoot: string): SeedFile[] {
  const roots: { origin: string; dir: string }[] = [
    { origin: "platform", dir: join(repoRoot, "scripts", "seed") },
  ];
  const servicesDir = join(repoRoot, "services");
  if (existsSync(servicesDir)) {
    const names = readdirSync(servicesDir, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name)
      .sort();
    for (const name of names) roots.push({ origin: name, dir: join(servicesDir, name, "seed") });
  }
  const files: SeedFile[] = [];
  for (const { origin, dir } of roots) {
    for (const store of STORES) {
      for (const name of sqlFiles(join(dir, store))) {
        files.push({ store, origin, path: join(dir, store, name) });
      }
    }
  }
  return files;
}
