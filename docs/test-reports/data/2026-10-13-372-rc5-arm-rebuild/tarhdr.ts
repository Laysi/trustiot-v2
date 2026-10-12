// Laysi/trustiot-v2#372: prints every header of a gzip-compressed tar layer (ustar fields and
// PAX records), so two layers that hold the same file can be compared field by field.
//   bun tarhdr.ts <layer.tar.gz>
//   bun tarhdr.ts <image directory>   the last layer of an OCI layout (index.json) or of
//                                     skopeo's dir: format (manifest.json)
import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";
import { createHash } from "node:crypto";

const text = (b: Uint8Array) => new TextDecoder().decode(b).replace(/\0.*$/s, "");
const field = (h: Uint8Array, off: number, len: number) => text(h.subarray(off, off + len));
const octal = (h: Uint8Array, off: number, len: number) => field(h, off, len).trim();
const sha = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");
const json = (p: string) => JSON.parse(readFileSync(p, "utf8"));

function layerPath(arg: string): string {
  if (!statSync(arg).isDirectory()) return arg;
  if (existsSync(join(arg, "index.json"))) {
    const blob = (digest: string) => join(arg, "blobs", ...digest.split(":"));
    const image = json(join(arg, "index.json")).manifests[0].digest as string;
    const layer = json(blob(image)).layers.at(-1).digest as string;
    console.log(`image ${image}, COPY layer ${layer}`);
    return blob(layer);
  }
  const layer = json(join(arg, "manifest.json")).layers.at(-1).digest as string;
  console.log(`image sha256:${sha(readFileSync(join(arg, "manifest.json")))}, COPY layer ${layer}`);
  return join(arg, layer.split(":")[1]!);
}

const gz = new Uint8Array(readFileSync(layerPath(process.argv[2]!)));
const tar = new Uint8Array(gunzipSync(gz));
console.log(`compressed ${gz.length} bytes, sha256 ${sha(gz)}`);
console.log(`uncompressed ${tar.length} bytes, sha256 ${sha(tar)} (the layer's diff_id)`);
console.log(`gzip header: ${[...gz.subarray(0, 10)].map((x) => x.toString(16).padStart(2, "0")).join(" ")}`);
let pos = 0;
while (pos + 512 <= tar.length) {
  const h = tar.subarray(pos, pos + 512);
  if (h.every((x) => x === 0)) {
    console.log(`@${pos}: zero block; ${tar.length - pos} bytes from here to the end`);
    break;
  }
  const size = parseInt(octal(h, 124, 12) || "0", 8);
  const type = field(h, 156, 1) || "0";
  console.log(
    `@${pos}: type ${JSON.stringify(type)} name ${JSON.stringify(field(h, 0, 100))} prefix ${JSON.stringify(field(h, 345, 155))}` +
      ` mode ${octal(h, 100, 8)} uid ${octal(h, 108, 8)} gid ${octal(h, 116, 8)} size ${size} mtime ${octal(h, 136, 12)}` +
      ` link ${JSON.stringify(field(h, 157, 100))} magic ${JSON.stringify(field(h, 257, 6))} version ${JSON.stringify(new TextDecoder().decode(h.subarray(263, 265)))}` +
      ` uname ${JSON.stringify(field(h, 265, 32))} gname ${JSON.stringify(field(h, 297, 32))} dev ${octal(h, 329, 8)},${octal(h, 337, 8)}` +
      ` header-sha256 ${sha(h).slice(0, 16)}`,
  );
  const body = tar.subarray(pos + 512, pos + 512 + size);
  if (type === "x" || type === "g") {
    for (const rec of new TextDecoder().decode(body).split("\n").filter((r) => r !== "")) {
      console.log(`      pax ${JSON.stringify(rec)}`);
    }
  } else if (size > 0) {
    console.log(`      content sha256 ${sha(body)}`);
  }
  pos += 512 + Math.ceil(size / 512) * 512;
}
