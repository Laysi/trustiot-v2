// Comparing two file trees: the check scans exactly what compareTrees reports as added or changed.
import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type Entry, compareTrees, resolveInImage, walk } from "../src/tree.ts";

describe("links resolved inside the image", () => {
  const entry = (type: Entry["type"], link?: string): Entry => ({ type, mode: 0o644, uid: 0, gid: 0, size: 0, mtimeMs: 0, ...(link ? { link } : {}) });
  const entries = new Map<string, Entry>([
    ["/usr/lib/udev/rules.d/80-real.rules", entry("file")],
    ["/etc/udev/rules.d/90-abs.rules", entry("link", "/usr/lib/udev/rules.d/80-real.rules")],
    ["/etc/udev/rules.d/91-rel.rules", entry("link", "../../../usr/lib/udev/rules.d/80-real.rules")],
    ["/etc/udev/rules.d/92-chain.rules", entry("link", "90-abs.rules")],
    ["/etc/udev/rules.d/93-null.rules", entry("link", "/dev/null")],
    ["/etc/udev/rules.d/94-loop.rules", entry("link", "95-loop.rules")],
    ["/etc/udev/rules.d/95-loop.rules", entry("link", "94-loop.rules")],
    ["/etc/udev/rules.d/96-gone.rules", entry("link", "/opt/missing.rules")],
    ["/etc/udev/rules.d/97-escape.rules", entry("link", "../../../../../../home/host/x.rules")],
  ]);

  test("follows absolute, relative and chained links within the tree", () => {
    for (const name of ["90-abs", "91-rel", "92-chain"]) expect(resolveInImage(entries, `/etc/udev/rules.d/${name}.rules`)).toBe("/usr/lib/udev/rules.d/80-real.rules");
    expect(resolveInImage(entries, "/etc/udev/rules.d/93-null.rules")).toBe("/dev/null");
    expect(resolveInImage(entries, "/usr/lib/udev/rules.d/80-real.rules")).toBe("/usr/lib/udev/rules.d/80-real.rules");
  });

  test("gives up on loops and targets outside the tree, never reaching the host", () => {
    expect(resolveInImage(entries, "/etc/udev/rules.d/94-loop.rules")).toBeUndefined();
    expect(resolveInImage(entries, "/etc/udev/rules.d/96-gone.rules")).toBeUndefined();
    // ".." stops at the image's root, as it does for the kernel inside a chroot.
    expect(resolveInImage(entries, "/etc/udev/rules.d/97-escape.rules")).toBeUndefined();
  });
});

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
