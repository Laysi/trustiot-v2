// Walks a mounted file system and compares two of them: the check lists what the build changed
// in the source image, and the manifest lets two builds be compared file by file.
import { createReadStream } from "node:fs";
import { lstat, readdir, readlink } from "node:fs/promises";
import { join } from "node:path";

export interface Entry {
  type: "file" | "dir" | "link" | "other";
  /** Permission bits including setuid, setgid and sticky. */
  mode: number;
  uid: number;
  gid: number;
  size: number;
  mtimeMs: number;
  link?: string;
}

/** Every path under `root` (as "/etc/fstab"), without descending into other mounts. */
export async function walk(root: string): Promise<Map<string, Entry>> {
  const entries = new Map<string, Entry>();
  const rootDev = (await lstat(root)).dev;
  const pending = ["/"];
  while (pending.length > 0) {
    const dir = pending.pop() ?? "/";
    const names = (await readdir(join(root, dir))).sort();
    for (const name of names) {
      const path = dir === "/" ? `/${name}` : `${dir}/${name}`;
      const info = await lstat(join(root, path));
      const type = info.isFile() ? "file" : info.isDirectory() ? "dir" : info.isSymbolicLink() ? "link" : "other";
      const entry: Entry = { type, mode: info.mode & 0o7777, uid: info.uid, gid: info.gid, size: info.size, mtimeMs: info.mtimeMs };
      if (type === "link") entry.link = await readlink(join(root, path));
      entries.set(path, entry);
      if (type === "dir" && info.dev === rootDev) pending.push(path);
    }
  }
  return entries;
}

export async function hashFile(path: string): Promise<string> {
  const hasher = new Bun.CryptoHasher("sha256");
  for await (const chunk of createReadStream(path, { highWaterMark: 1024 * 1024 })) hasher.update(chunk as Buffer);
  return hasher.digest("hex");
}

export interface Delta {
  added: string[];
  changed: string[];
  removed: string[];
}

function sameMeta(a: Entry, b: Entry): boolean {
  return a.type === b.type && a.mode === b.mode && a.uid === b.uid && a.gid === b.gid && a.size === b.size && a.link === b.link;
}

/** Files hashed at the same time: fuse2fs answers reads one by one, so a few in flight help. */
const HASH_CONCURRENCY = 8;

/**
 * What differs between two trees. Regular files with the same metadata are compared by content,
 * whatever their modification times: the check scans what this finds, so a change that kept the
 * size and the time must not slip through.
 */
export async function compareTrees(
  baseRoot: string,
  base: Map<string, Entry>,
  newRoot: string,
  next: Map<string, Entry>,
): Promise<Delta> {
  const delta: Delta = { added: [], changed: [], removed: [] };
  const toHash: string[] = [];
  for (const [path, entry] of next) {
    const old = base.get(path);
    if (!old) {
      delta.added.push(path);
    } else if (!sameMeta(old, entry)) {
      delta.changed.push(path);
    } else if (entry.type === "file") {
      toHash.push(path);
    }
  }
  let index = 0;
  const worker = async () => {
    while (index < toHash.length) {
      const path = toHash[index++] ?? "";
      if ((await hashFile(join(baseRoot, path))) !== (await hashFile(join(newRoot, path)))) delta.changed.push(path);
    }
  };
  await Promise.all(Array.from({ length: HASH_CONCURRENCY }, worker));
  for (const path of base.keys()) if (!next.has(path)) delta.removed.push(path);
  for (const list of [delta.added, delta.changed, delta.removed]) list.sort();
  return delta;
}

/** One line per path: type, mode, uid, gid, size, then the SHA-256 or the link target. */
export async function manifest(root: string, entries: Map<string, Entry>): Promise<string> {
  const lines: string[] = [];
  for (const path of [...entries.keys()].sort()) {
    const e = entries.get(path);
    if (!e) continue;
    const content = e.type === "file" ? await hashFile(join(root, path)) : e.type === "link" ? (e.link ?? "") : "-";
    lines.push([path, e.type, e.mode.toString(8).padStart(4, "0"), e.uid, e.gid, e.type === "dir" ? "-" : e.size, content].join("\t"));
  }
  return `${lines.join("\n")}\n`;
}

/** Private, link-local and shared address space, and names that only resolve on a LAN. */
export const LAN_PATTERNS: readonly RegExp[] = [
  /\b10\.\d{1,3}\.\d{1,3}\.\d{1,3}\b/,
  /\b192\.168\.\d{1,3}\.\d{1,3}\b/,
  /\b172\.(1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}\b/,
  /\b169\.254\.\d{1,3}\.\d{1,3}\b/,
  /\b100\.(6[4-9]|[7-9]\d|1[01]\d|12[0-7])\.\d{1,3}\.\d{1,3}\b/,
  /\b[fF][cdCD][0-9a-fA-F]{2}:[0-9a-fA-F:]+\b/,
  /\b[\w-]+\.(local|lan|internal|home\.arpa|localdomain)\b/i,
];

export const PRIVATE_KEY_PATTERN = /-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----/;

/** Text: no NUL byte in the first 64 KiB. */
export function isText(bytes: Uint8Array): boolean {
  return !bytes.subarray(0, 64 * 1024).includes(0);
}

/**
 * Problems found in one text file: a private key block, or a LAN address or host name. Only the
 * line and the pattern are named, never the match: the report must not repeat what it guards
 * against (reports can end up in the public repository).
 */
export function textProblems(path: string, text: string, lan: boolean): string[] {
  const problems: string[] = [];
  if (PRIVATE_KEY_PATTERN.test(text)) problems.push(`${path} holds a private key block`);
  if (lan) {
    LAN_PATTERNS.forEach((pattern, index) => {
      const match = pattern.exec(text);
      if (match) {
        const line = text.slice(0, match.index).split("\n").length;
        problems.push(`${path} names a LAN address or host (line ${line}, LAN_PATTERNS[${index}])`);
      }
    });
  }
  return problems;
}
