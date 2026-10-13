// Applies the `=` pins Renovate plans (pins.txt: "<crate> <current> <=new>") to the
// [workspace.dependencies] table of a Cargo.toml, the way the pin PR would.
// Usage: bun apply-pins.ts <pins.txt> <Cargo.toml>
import { readFileSync, writeFileSync } from "node:fs";

const [pinsPath, cargoPath] = Bun.argv.slice(2);
const pins = readFileSync(pinsPath!, "utf8").trim().split("\n").map((l) => l.split(" ") as [string, string, string]);
const text = readFileSync(cargoPath!, "utf8");
const start = text.indexOf("[workspace.dependencies]");
const end = text.indexOf("\n[", start + 1);
let section = text.slice(start, end);
for (const [name, current, pinned] of pins) {
  const escaped = name.replace(/[-]/g, "\\-");
  const plain = new RegExp(`^(${escaped} = )"${current.replace(/\./g, "\\.")}"$`, "m");
  const table = new RegExp(`^(${escaped} = \\{ version = )"${current.replace(/\./g, "\\.")}"`, "m");
  const before = section;
  section = section.replace(plain, `$1"${pinned}"`).replace(table, `$1"${pinned}"`);
  if (section === before) throw new Error(`no requirement for ${name} ${current}`);
}
writeFileSync(cargoPath!, text.slice(0, start) + section + text.slice(end));
console.log(`pinned ${pins.length} crates`);
