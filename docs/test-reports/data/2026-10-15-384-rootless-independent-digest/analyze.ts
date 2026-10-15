// Laysi/trustiot-v2#384: summarizes OCI archives of one-image builds, so that images packed as
// root and rootless (or with other settings) can be compared field by field.
//   bun analyze.ts <archive.oci.tar | directory of *.oci.tar> ... [--diff <label> <label>] ...
// For each archive: the image (manifest) and config digests, the diff_id of the last layer (the
// COPY of rust-binary.Containerfile), the COPY step's `created_by`, the history comments that
// name intermediate images, the io.buildah.version label, and every PAX record of the last layer
// that carries an xattr. Then, for each pair of archives whose names differ only in a leading
// `root` / `rootless`, and for each pair named with --diff, the config fields and the layers that
// differ. Exits non-zero when a pair names an archive that is not there.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { gunzipSync } from "node:zlib";

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

interface Summary {
  label: string;
  image: string;
  config: string;
  configJson: Json;
  layers: string[];
  copyDiffId: string;
  copyStep: string;
  fromComments: string[];
  buildahVersion: string;
  xattrs: string[];
}

const sha = (bytes: Uint8Array) => `sha256:${createHash("sha256").update(bytes).digest("hex")}`;

function archives(args: string[]): string[] {
  return args.flatMap((arg) =>
    statSync(arg).isDirectory()
      ? readdirSync(arg)
          .filter((name) => name.endsWith(".oci.tar"))
          .sort()
          .map((name) => join(arg, name))
      : [arg],
  );
}

/** Every PAX record of a tar stream that names an xattr, with the entry it belongs to. */
function paxXattrs(tar: Uint8Array): string[] {
  const found: string[] = [];
  const text = (bytes: Uint8Array) => new TextDecoder().decode(bytes).replace(/\0.*$/s, "");
  let pending: string[] = [];
  for (let pos = 0; pos + 512 <= tar.length; ) {
    const header = tar.subarray(pos, pos + 512);
    if (header.every((byte) => byte === 0)) {
      break;
    }
    const size = parseInt(text(header.subarray(124, 136)).trim() || "0", 8);
    const type = text(header.subarray(156, 157)) || "0";
    const body = tar.subarray(pos + 512, pos + 512 + size);
    if (type === "x") {
      pending = new TextDecoder()
        .decode(body)
        .split("\n")
        .filter((record) => record.includes("xattr"));
    } else if (type !== "g") {
      for (const record of pending) {
        found.push(`${text(header.subarray(0, 100))}: ${record}`);
      }
      pending = [];
    }
    pos += 512 + Math.ceil(size / 512) * 512;
  }
  return found;
}

function summarize(archive: string): Summary {
  const dir = mkdtempSync(join(tmpdir(), "analyze-384-"));
  try {
    execFileSync("tar", ["-xf", archive, "-C", dir]);
    const blob = (digest: string) => readFileSync(join(dir, "blobs", ...digest.split(":")));
    const image = JSON.parse(readFileSync(join(dir, "index.json"), "utf8")).manifests[0].digest as string;
    const manifestBytes = blob(image);
    if (sha(manifestBytes) !== image) {
      throw new Error(`${archive}: the manifest does not hash to ${image}`);
    }
    const manifest = JSON.parse(manifestBytes.toString("utf8"));
    const config = JSON.parse(blob(manifest.config.digest).toString("utf8"));
    const history = (config.history ?? []) as { created_by?: string; comment?: string }[];
    const last = manifest.layers.at(-1).digest as string;
    return {
      label: basename(archive).replace(/\.oci\.tar$/, ""),
      image,
      config: manifest.config.digest,
      configJson: config,
      layers: manifest.layers.map((layer: { digest: string }) => layer.digest),
      copyDiffId: config.rootfs.diff_ids.at(-1),
      copyStep: history.map((entry) => /COPY [^ ]*file:[0-9a-f]+/.exec(entry.created_by ?? "")?.[0]).find((step) => step !== undefined) ?? "(none)",
      fromComments: history.map((entry) => entry.comment ?? "").filter((comment) => /^FROM [0-9a-f]+$/.test(comment)),
      buildahVersion: config.config?.Labels?.["io.buildah.version"] ?? "(none)",
      xattrs: paxXattrs(new Uint8Array(gunzipSync(blob(last)))),
    };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** The paths at which two JSON values differ, with both values. */
function differences(a: Json, b: Json, path = ""): string[] {
  if (JSON.stringify(a) === JSON.stringify(b)) {
    return [];
  }
  if (a !== null && b !== null && typeof a === "object" && typeof b === "object" && Array.isArray(a) === Array.isArray(b)) {
    const keys = Array.isArray(a)
      ? [...Array(Math.max(a.length, (b as Json[]).length)).keys()].map(String)
      : [...new Set([...Object.keys(a), ...Object.keys(b as object)])].sort();
    return keys.flatMap((key) =>
      differences((a as Record<string, Json>)[key] ?? null, (b as Record<string, Json>)[key] ?? null, `${path}/${key}`),
    );
  }
  return [`${path}: ${JSON.stringify(a)} | ${JSON.stringify(b)}`];
}

const args = Bun.argv.slice(2);
const pairs: [string, string][] = [];
for (let index = args.indexOf("--diff"); index >= 0; index = args.indexOf("--diff")) {
  pairs.push([args[index + 1] ?? "", args[index + 2] ?? ""]);
  args.splice(index, 3);
}
const summaries = archives(args).map(summarize);
for (const s of summaries) {
  console.log(`${s.label}`);
  console.log(`  image ${s.image}`);
  console.log(`  config ${s.config}, COPY diff_id ${s.copyDiffId}`);
  console.log(`  ${s.copyStep}; FROM comments: ${s.fromComments.join(", ") || "(none)"}; io.buildah.version ${s.buildahVersion}`);
  console.log(`  xattrs in the COPY layer: ${s.xattrs.length === 0 ? "(none)" : s.xattrs.join("; ")}`);
}
const byLabel = new Map(summaries.map((s) => [s.label, s]));
for (const s of summaries) {
  const match = /^(.*?)root-(.*)$/.exec(s.label);
  if (match !== null && !s.label.includes("rootless-")) {
    pairs.push([s.label, `${match[1]}rootless-${match[2]}`]);
  }
}
for (const [a, b] of pairs) {
  const s = byLabel.get(a);
  const other = byLabel.get(b);
  if (s === undefined || other === undefined) {
    console.log(`MISSING  ${a} / ${b}`);
    process.exitCode = 1;
    continue;
  }
  const same = s.image === other.image;
  console.log(`${same ? "SAME" : "DIFF"}  ${s.label} / ${other.label}: ${s.image} / ${other.image}`);
  if (!same) {
    s.layers.forEach((layer, index) => {
      if (layer !== other.layers[index]) {
        console.log(`  layer ${index}: ${layer} | ${other.layers[index]}`);
      }
    });
    for (const line of differences(s.configJson, other.configJson)) {
      console.log(`  config ${line}`);
    }
  }
}
