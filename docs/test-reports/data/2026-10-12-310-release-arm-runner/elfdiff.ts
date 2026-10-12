// Section-level comparison of two 64-bit little-endian ELF files, plus printable strings that
// only one of them holds.   bun elfdiff.ts <a> <b>
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";

const [pa, pb] = process.argv.slice(2) as [string, string];
const sha = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex").slice(0, 16);

function sections(bytes: Uint8Array) {
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
  const out = new Map<string, { off: number; size: number; data: Uint8Array }>();
  for (let i = 1; i < shnum; i++) {
    const h = hdr(i);
    out.set(name(h.name), { off: h.off, size: h.size, data: h.type === 8 ? new Uint8Array() : bytes.subarray(h.off, h.off + h.size) });
  }
  return out;
}

function strings(bytes: Uint8Array, min = 8): Map<string, number> {
  const out = new Map<string, number>();
  let start = -1;
  for (let i = 0; i <= bytes.length; i++) {
    const c = bytes[i] ?? 0;
    const printable = c >= 0x20 && c < 0x7f;
    if (printable && start < 0) start = i;
    if (!printable && start >= 0) {
      if (i - start >= min) {
        const s = new TextDecoder().decode(bytes.subarray(start, i));
        out.set(s, (out.get(s) ?? 0) + 1);
      }
      start = -1;
    }
  }
  return out;
}

const a = new Uint8Array(readFileSync(pa));
const b = new Uint8Array(readFileSync(pb));
console.log(`sizes ${a.length} / ${b.length}`);
const sa = sections(a);
const sb = sections(b);
for (const [n, x] of sa) {
  const y = sb.get(n);
  const same = y !== undefined && sha(x.data) === sha(y.data);
  console.log(`${same ? "same" : "DIFF"} ${n.padEnd(24)} ${x.size} / ${y?.size ?? "-"}`);
}
for (const n of sb.keys()) if (!sa.has(n)) console.log(`only b: ${n}`);
const ta = strings(a);
const tb = strings(b);
const onlyA = [...ta.keys()].filter((s) => !tb.has(s));
const onlyB = [...tb.keys()].filter((s) => !ta.has(s));
console.log(`strings only in a: ${onlyA.length}, only in b: ${onlyB.length}`);
for (const s of onlyA.slice(0, 40)) console.log(`  a: ${s.slice(0, 160)}`);
for (const s of onlyB.slice(0, 40)) console.log(`  b: ${s.slice(0, 160)}`);
for (const needle of ["x86_64", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu", "/root/", "/work/"]) {
  console.log(`count "${needle}": ${[...ta].filter(([s]) => s.includes(needle)).length} / ${[...tb].filter(([s]) => s.includes(needle)).length}`);
}
