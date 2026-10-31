// Comparing two file trees: the check scans exactly what compareTrees reports as added or changed.
import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { compareTrees, walk } from "../src/tree.ts";

const dir = mkdtempSync(join(tmpdir(), "image-tree-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

function tree(name: string, files: Record<string, string>, time: number): string {
  const root = join(dir, name);
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(join(root, path, ".."), { recursive: true });
    writeFileSync(join(root, path), content);
    utimesSync(join(root, path), time, time);
  }
  return root;
}

describe("compareTrees", () => {
  test("same size and same modification time, other content: changed", async () => {
    const base = tree("a", { "etc/x.conf": "value=aaaa\n", "etc/same": "same\n", "etc/gone": "1" }, 1_700_000_000);
    const next = tree("b", { "etc/x.conf": "value=bbbb\n", "etc/same": "same\n", "etc/new": "2" }, 1_700_000_000);
    symlinkSync("/etc/same", join(next, "etc/link"));
    const nextTree = await walk(next);
    const delta = await compareTrees(base, await walk(base), next, nextTree);
    // A directory's size can follow its entry count (tmpfs, btrfs), so only files are asserted here.
    expect(delta.changed.filter((p) => nextTree.get(p)?.type !== "dir")).toEqual(["/etc/x.conf"]);
    expect(delta.added).toEqual(["/etc/link", "/etc/new"]);
    expect(delta.removed).toEqual(["/etc/gone"]);
  });

  test("another modification time alone is not a change", async () => {
    const base = tree("c", { "f": "same\n" }, 1_700_000_000);
    const next = tree("d", { "f": "same\n" }, 1_800_000_000);
    expect(await compareTrees(base, await walk(base), next, await walk(next))).toEqual({ added: [], changed: [], removed: [] });
  });

  test("many files with the same metadata are all compared", async () => {
    const files = Object.fromEntries(Array.from({ length: 40 }, (_, i) => [`d/f${i}`, `content ${String(i).padStart(2, "0")}\n`]));
    const base = tree("e", files, 1_700_000_000);
    const next = tree("g", { ...files, "d/f7": "content XX\n", "d/f33": "content YY\n" }, 1_700_000_000);
    expect((await compareTrees(base, await walk(base), next, await walk(next))).changed).toEqual(["/d/f33", "/d/f7"]);
  });
});
