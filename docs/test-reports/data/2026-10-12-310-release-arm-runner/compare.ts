// Compares the outputs of the x64 and arm64 release runs of arm-eval.sh.
//   bun compare.ts <x64 out> <arm64 out>
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";

const [a, b] = process.argv.slice(2) as [string, string];
const sha = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
const read = (dir: string, file: string) => new Uint8Array(readFileSync(join(dir, file)));
const json = (dir: string, file: string) => JSON.parse(readFileSync(join(dir, file), "utf8"));
let same = true;
const verdict = (what: string, equal: boolean, detail = "") => {
  if (!equal) same = false;
  console.log(`${equal ? "SAME" : "DIFF"}  ${what}${detail ? `  ${detail}` : ""}`);
};

/** Section name → [offset, size, sha256] of a 64-bit little-endian ELF file. */
function sections(bytes: Uint8Array): Map<string, [number, number, string]> {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const shoff = Number(v.getBigUint64(0x28, true));
  const shentsize = v.getUint16(0x3a, true);
  const shnum = v.getUint16(0x3c, true);
  const shstrndx = v.getUint16(0x3e, true);
  const hdr = (i: number) => {
    const o = shoff + i * shentsize;
    return { name: v.getUint32(o, true), type: v.getUint32(o + 4, true), off: Number(v.getBigUint64(o + 0x18, true)), size: Number(v.getBigUint64(o + 0x20, true)) };
  };
  const strtab = hdr(shstrndx);
  const name = (off: number) => {
    let end = strtab.off + off;
    while (bytes[end] !== 0) end++;
    return new TextDecoder().decode(bytes.subarray(strtab.off + off, end));
  };
  const out = new Map<string, [number, number, string]>();
  for (let i = 1; i < shnum; i++) {
    const h = hdr(i);
    const data = h.type === 8 ? new Uint8Array() : bytes.subarray(h.off, h.off + h.size); // SHT_NOBITS
    out.set(name(h.name), [h.off, h.size, sha(data)]);
  }
  return out;
}

console.log("rustc host:", ...[a, b].map((d) => readFileSync(join(d, "rustc-vV.txt"), "utf8").match(/host: (\S+)/)?.[1]));
for (const file of ["plan.json"]) {
  verdict(file, sha(read(a, file)) === sha(read(b, file)));
}

for (const dir of ["bin", "cobj"]) {
  for (const file of readdirSync(join(a, dir)).sort()) {
    const x = read(a, join(dir, file));
    const y = existsSync(join(b, dir, file)) ? read(b, join(dir, file)) : new Uint8Array();
    const equal = sha(x) === sha(y);
    verdict(`${dir}/${file}`, equal, `${x.length} / ${y.length} bytes, sha256 ${sha(x).slice(0, 16)} / ${sha(y).slice(0, 16)}`);
    if (!equal && dir === "bin") {
      let diff = 0;
      for (let i = 0; i < Math.min(x.length, y.length); i++) if (x[i] !== y[i]) diff++;
      console.log(`      differing bytes at equal offsets: ${diff}`);
      const sx = sections(x);
      const sy = sections(y);
      for (const [name, [off, size, h]] of sx) {
        const other = sy.get(name);
        if (other === undefined) console.log(`      section ${name}: only in x64`);
        else if (other[2] !== h || other[1] !== size) console.log(`      section ${name}: size ${size} / ${other[1]}, content differs`);
      }
      for (const name of sy.keys()) if (!sx.has(name)) console.log(`      section ${name}: only in arm64`);
    }
  }
}

const builds = [json(a, "release/build.json"), json(b, "release/build.json")];
for (const image of builds[0].images) {
  const other = builds[1].images.find((i: { name: string }) => i.name === image.name);
  if (other === undefined) {
    verdict(`image ${image.name}`, false, "not built in the second run");
    continue;
  }
  verdict(`image ${image.name} digest`, image.digest === other?.digest, `${image.digest} / ${other?.digest}`);
  verdict(`image ${image.name} archive size`, image.size === other?.size, `${image.size} / ${other?.size}`);
  const sboms = [a, b].map((d) => json(d, `release/images/${image.name}.spdx.json`));
  verdict(`sbom ${image.name} file bytes`, sha(read(a, `release/images/${image.name}.spdx.json`)) === sha(read(b, `release/images/${image.name}.spdx.json`)));
  const normalized = sboms.map((s) => {
    const { documentNamespace, creationInfo, ...rest } = s;
    return JSON.stringify({ ...rest, creationInfo: { ...creationInfo, created: "<created>" } });
  });
  verdict(`sbom ${image.name} without documentNamespace and created`, normalized[0] === normalized[1]);
  const pkgs = sboms.map((s) => new Set<string>((s.packages ?? []).map((p: { name: string; versionInfo?: string }) => `${p.name}@${p.versionInfo ?? ""}`)));
  const onlyA = [...pkgs[0]].filter((p) => !pkgs[1].has(p));
  const onlyB = [...pkgs[1]].filter((p) => !pkgs[0].has(p));
  verdict(`sbom ${image.name} package list (${pkgs[0].size} / ${pkgs[1].size})`, onlyA.length === 0 && onlyB.length === 0, `${onlyA.length ? `only x64: ${onlyA.join(", ")}` : ""} ${onlyB.length ? `only arm64: ${onlyB.join(", ")}` : ""}`);
  console.log(`      namespaces: ${sboms[0].documentNamespace} / ${sboms[1].documentNamespace}`);
  console.log(`      created: ${sboms[0].creationInfo?.created} / ${sboms[1].creationInfo?.created}`);
}

const published = [json(a, "published.json"), json(b, "published.json")];
verdict("published chart digest", published[0].chart?.digest === published[1].chart?.digest, `${published[0].chart?.digest} / ${published[1].chart?.digest}`);
for (const image of published[0].images ?? []) {
  const other = (published[1].images ?? []).find((i: { name: string }) => i.name === image.name);
  verdict(`published ${image.name} digest`, image.digest === other?.digest);
}
for (const d of [a, b]) {
  const p1 = readFileSync(join(d, "published.json"), "utf8");
  const p2 = readFileSync(join(d, "published2.json"), "utf8");
  verdict(`${d.includes("arm64") ? "arm64" : "x64"}: publish re-run gives the same published.json`, p1 === p2);
}
console.log(same ? "RESULT: identical" : "RESULT: differences above");
