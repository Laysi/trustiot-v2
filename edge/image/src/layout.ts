// The image's partition table: the source's boot partition, then the data partition, then the
// source's root partition moved to the end of the image.
//
// Root stays partition 2 and the last one on the disk because the source image grows it on first
// boot: its initramfs script resize_early runs `parted resizepart 2 <end of card>` only when root
// is partition 2, and set_partuuid then gives the card a random disk identifier and rewrites
// every PARTUUID in /etc/fstab and cmdline.txt. The data partition is entry 3 but sits between
// boot and root, so neither step moves it (README.md, section 「分割區」).
import { DATA, SECTOR, SOURCE_TABLE } from "./pins.ts";

export interface Partition {
  /** Entry in the MBR (1 to 4); the kernel names it <disk>p<number>. */
  number: number;
  start: number;
  size: number;
  /** MBR type as sfdisk prints it: "c" (FAT32 LBA) or "83" (Linux). */
  type: string;
}

export interface Table {
  /** MBR disk identifier as sfdisk prints it, for example 0x27b94958. */
  labelId: string;
  partitions: Partition[];
}

/** Partition starts are multiples of 4 MiB, like the source image's. */
export const ALIGN_SECTORS = (4 * 1024 * 1024) / SECTOR;

export function alignUp(sector: number): number {
  return Math.ceil(sector / ALIGN_SECTORS) * ALIGN_SECTORS;
}

export function sourceTable(): Table {
  return {
    labelId: SOURCE_TABLE.labelId,
    partitions: SOURCE_TABLE.partitions.map((p) => ({ ...p })),
  };
}

function entry(table: Table, number: number): Partition {
  const found = table.partitions.find((p) => p.number === number);
  if (!found) throw new Error(`the table has no partition ${number}`);
  return found;
}

/** The table of the built image. */
export function targetTable(): Table {
  const source = sourceTable();
  const boot = entry(source, 1);
  const root = entry(source, 2);
  const dataStart = alignUp(boot.start + boot.size);
  const rootStart = alignUp(dataStart + DATA.sizeSectors);
  return {
    labelId: source.labelId,
    partitions: [
      { ...boot },
      { number: 2, start: rootStart, size: root.size, type: root.type },
      { number: DATA.number, start: dataStart, size: DATA.sizeSectors, type: "83" },
    ],
  };
}

/** Size of the image file: up to the end of the last partition on the disk. */
export function imageSectors(table: Table): number {
  return Math.max(...table.partitions.map((p) => p.start + p.size));
}

/** Input for `sfdisk <image>`: entries in table order, each with an explicit position. */
export function renderSfdiskScript(table: Table): string {
  const lines = [`label: dos`, `label-id: ${table.labelId}`, `unit: sectors`, `sector-size: ${SECTOR}`, ``];
  const ordered = [...table.partitions].sort((a, b) => a.number - b.number);
  ordered.forEach((p, index) => {
    if (p.number !== index + 1) throw new Error(`partition numbers must be 1..n, found ${p.number}`);
    lines.push(`start=${p.start}, size=${p.size}, type=${p.type}`);
  });
  return `${lines.join("\n")}\n`;
}

/** Reads `sfdisk --json <file>`. */
export function parseSfdiskJson(text: string): Table {
  const parsed = JSON.parse(text) as {
    partitiontable?: {
      label?: string;
      id?: string;
      device?: string;
      sectorsize?: number;
      partitions?: { node?: string; start?: number; size?: number; type?: string }[];
    };
  };
  const table = parsed.partitiontable;
  if (!table || table.label !== "dos") throw new Error("not an MBR (dos) partition table");
  if (table.sectorsize !== undefined && table.sectorsize !== SECTOR) {
    throw new Error(`sector size ${table.sectorsize}, expected ${SECTOR}`);
  }
  const device = table.device ?? "";
  const partitions = (table.partitions ?? []).map((p) => {
    const suffix = (p.node ?? "").startsWith(device) ? (p.node ?? "").slice(device.length) : (p.node ?? "");
    const match = /^p?(\d+)$/.exec(suffix);
    if (!match || p.start === undefined || p.size === undefined || p.type === undefined) {
      throw new Error(`unexpected partition entry ${JSON.stringify(p)}`);
    }
    return { number: Number(match[1]), start: p.start, size: p.size, type: p.type };
  });
  return { labelId: table.id ?? "", partitions };
}

/** Differences between two tables, empty when they are equal. */
export function compareTables(actual: Table, expected: Table): string[] {
  const problems: string[] = [];
  if (actual.labelId.toLowerCase() !== expected.labelId.toLowerCase()) {
    problems.push(`disk identifier ${actual.labelId}, expected ${expected.labelId}`);
  }
  const numbers = new Set([...actual.partitions, ...expected.partitions].map((p) => p.number));
  for (const number of [...numbers].sort()) {
    const a = actual.partitions.find((p) => p.number === number);
    const e = expected.partitions.find((p) => p.number === number);
    if (!a) problems.push(`partition ${number} is missing`);
    else if (!e) problems.push(`unexpected partition ${number}`);
    else if (a.start !== e.start || a.size !== e.size || a.type !== e.type) {
      problems.push(
        `partition ${number} is start=${a.start} size=${a.size} type=${a.type}, expected start=${e.start} size=${e.size} type=${e.type}`,
      );
    }
  }
  return problems;
}

/** Structural rules the target table must follow, independent of the exact numbers. */
export function layoutProblems(table: Table): string[] {
  const problems: string[] = [];
  const byStart = [...table.partitions].sort((a, b) => a.start - b.start);
  for (const p of table.partitions) {
    if (p.start % ALIGN_SECTORS !== 0) problems.push(`partition ${p.number} does not start on a 4 MiB boundary`);
    if (p.size <= 0) problems.push(`partition ${p.number} is empty`);
  }
  for (let i = 1; i < byStart.length; i++) {
    const before = byStart[i - 1];
    const current = byStart[i];
    if (before && current && before.start + before.size > current.start) {
      problems.push(`partitions ${before.number} and ${current.number} overlap`);
    }
  }
  if (byStart[0]?.number !== 1 || byStart[0]?.type !== "c") problems.push("the boot partition must be entry 1 and first on the disk");
  if (byStart.at(-1)?.number !== 2) problems.push("root must be entry 2 and last on the disk, so first boot can grow it");
  return problems;
}

/** Bytes 0 to 439 of the MBR: the boot code area, before the disk identifier. */
export const BOOT_CODE_BYTES = 440;

/**
 * Whether first boot keeps the disk identifier. parted (libparted's msdos_write, 3.6) clears the
 * whole MBR and picks a random identifier when the boot code area starts with a zero byte, and
 * sfdisk leaves that area zero on a new image. If resize_early's parted changed the identifier,
 * set_partuuid would no longer find the old one in /etc/fstab and cmdline.txt, and the next boot
 * would not find root. The build therefore copies the source image's boot code area.
 */
export function keepsDiskIdentifier(mbr: Uint8Array): boolean {
  return mbr.length >= BOOT_CODE_BYTES && mbr[0] !== 0;
}

/** PARTUUID of an MBR partition: the disk identifier without 0x, a dash, two-digit number. */
export function partuuid(labelId: string, number: number): string {
  const id = labelId.toLowerCase().replace(/^0x/, "").padStart(8, "0");
  return `${id}-${String(number).padStart(2, "0")}`;
}
