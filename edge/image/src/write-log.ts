// What the data partition can hold after a power cut inside the setup (README.md, section
// 「開機測試」; Laysi/trustiot-v2#918). The boot test records a setup with QEMU's blklogwrites
// driver, which logs every write the virtual machine makes to the gateway's disk and every flush,
// in the format of Linux's dm-log-writes (QEMU's block/blklogwrites.c); boot-test/power-cut.sh
// marks where each step of the setup begins and ends with a write to a sector outside every
// partition. From the log this module rebuilds the partition after a cut at any point of the
// setup (CrashState), under four models:
//
//   cut       every write up to a point arrived, in the order of the log, and none after it: a cut
//             inside a step on a card that keeps writes in order.
//   torn-cut  a cut whose last write arrived only in part, its first sectors: at every sector
//             boundary inside the LUKS2 metadata (the two copies, METADATA_SECTORS), and at the
//             middle of a write elsewhere.
//   lost      a card cache that loses or reorders what was not flushed: everything before the last
//             flush arrived, and of the writes since (an epoch), each part of the partition holds
//             what a subset of the epoch's writes to it left there (a later write kept and an
//             earlier one lost, too), or a mix of its sectors from before and from the epoch.
//             The parts: for each metadata copy its first sector (where blkid reads the magic,
//             label and UUID) and the rest of the copy (cryptsetup reads a copy only when every
//             byte of it is from the same write: its checksum), the keyslot area and the data area.
//             Every subset when the epoch writes a part at most SUBSET_WRITES times; otherwise
//             every prefix, every write alone and every write left out. Only the first sectors
//             are read byte by byte, and they are one sector each, so their every subset is one of
//             the epoch's writes to them. The other parts are read only as a whole: the rest of a
//             copy through the checksum, the keyslot area through a keyslot's digest (when the TPM
//             unlocks the volume), the data area as all zeros or not. What sets their states apart
//             is whether they hold a version the setup wrote, which is a prefix, or a mix.
//   torn      the first sector of one metadata copy written only up to a byte, or only from a
//             byte, between what it held before the epoch and each version the epoch writes to
//             it: at the boundaries of the LUKS2 binary header's fields and after each byte in
//             which the label, the magic or the UUID differ; every other part (as in lost) as
//             before the epoch or as the epoch left it.
//
// What the setup reads of a state is the header area (the first 16 MiB) and whether the whole
// partition is zeros (PartitionView); the rest of the data area is the ciphertext of the setup's
// own file system. Pure functions over the log's bytes, so the unit tests cover them without QEMU.
import { createHash } from "node:crypto";
import { DATA, LUKS, SECTOR } from "./pins.ts";

/** dm-log-writes' entry flags (drivers/md/dm-log-writes.c; QEMU writes the same). */
export const LOG_FLAGS = { flush: 1, fua: 2, discard: 4, mark: 8, metadata: 16 } as const;
const LOG_MAGIC = 0x6a736677736872n;
const LOG_VERSION = 1n;
/** Bytes of an entry's fixed fields; the rest of its log sector is padding. */
const ENTRY_BYTES = 32;

/** The two metadata copies at the start of the header, METADATA_SECTORS / 2 sectors each. */
export const METADATA_SECTORS = (2 * LUKS.metadataBytes) / SECTOR;
const COPY_SECTORS = LUKS.metadataBytes / SECTOR;
/** The header area: the metadata, then the keyslot area. */
export const HEADER_SECTORS = LUKS.headerBytes / SECTOR;
/**
 * The disk sector power-cut.sh writes its step marks to (trustiot_test.step-marks=): in the gap
 * between the MBR (sector 0) and the first partition, which nothing else writes. The image's first
 * partition is the source's boot partition, at sector 16384 (SOURCE_TABLE, layout.ts's
 * targetTable), so 8192 is 4 MiB clear of both; tests/write-log.test.ts and the boot test's
 * prepareDisk (against the image's own table) check that no partition holds it.
 */
export const STEP_MARK_SECTOR = 8192;

export interface LogEntry {
  /** Position in the log, from 0 (the first entry after the super block). */
  index: number;
  /** First sector written, in 512-byte sectors of the disk. */
  sector: number;
  sectors: number;
  flags: number;
  /** The bytes written; null for a flush or a discard. */
  data: Uint8Array | null;
}

/** A dm-log-writes log as QEMU's blklogwrites writes it (log-sector-size 512). */
export function parseWriteLog(log: Uint8Array): LogEntry[] {
  const view = new DataView(log.buffer, log.byteOffset, log.byteLength);
  if (log.byteLength < SECTOR) throw new Error(`the write log has ${log.byteLength} bytes, less than its super block`);
  const magic = view.getBigUint64(0, true);
  const version = view.getBigUint64(8, true);
  const count = view.getBigUint64(16, true);
  const sectorSize = view.getUint32(24, true);
  if (magic !== LOG_MAGIC) throw new Error(`the write log's magic is 0x${magic.toString(16)}, not dm-log-writes'`);
  if (version !== LOG_VERSION) throw new Error(`the write log is version ${version}, expected ${LOG_VERSION}`);
  if (sectorSize !== SECTOR) throw new Error(`the write log's sectors are ${sectorSize} bytes, expected ${SECTOR}`);
  const entries: LogEntry[] = [];
  let at = SECTOR;
  for (let index = 0; BigInt(index) < count; index++) {
    if (at + SECTOR > log.byteLength) throw new Error(`the write log ends inside entry ${index} of ${count}`);
    const sector = Number(view.getBigUint64(at, true));
    const sectors = Number(view.getBigUint64(at + 8, true));
    const flags = Number(view.getBigUint64(at + 16, true));
    const dataLength = Number(view.getBigUint64(at + 24, true));
    if ((flags & ~(LOG_FLAGS.flush | LOG_FLAGS.fua | LOG_FLAGS.discard)) !== 0) throw new Error(`entry ${index} has flags 0x${flags.toString(16)}, which the log of a disk never has`);
    if (dataLength !== 0 && dataLength !== sectors * SECTOR) throw new Error(`entry ${index} says it has ${dataLength} bytes of data for ${sectors} sectors`);
    for (let i = ENTRY_BYTES; i < SECTOR; i++) if (log[at + i] !== 0) throw new Error(`entry ${index} is not followed by zeros in its sector`);
    at += SECTOR;
    let data: Uint8Array | null = null;
    if ((flags & LOG_FLAGS.discard) === 0 && sectors > 0) {
      if (at + sectors * SECTOR > log.byteLength) throw new Error(`the write log ends inside the data of entry ${index}`);
      data = log.subarray(at, at + sectors * SECTOR);
      at += sectors * SECTOR;
    }
    if ((flags & LOG_FLAGS.flush) !== 0 && sectors > 0) throw new Error(`entry ${index} is a flush with data, which QEMU never logs`);
    entries.push({ index, sector, sectors, flags, data });
  }
  return entries;
}

/** What power-cut.sh writes to the mark sector: "trustiot-step <begin|end> <step>[ <exit code>]". */
export function parseStepMark(data: Uint8Array): { phase: "begin" | "end"; step: string; exit: number | null } | null {
  const text = new TextDecoder().decode(data).replace(/\0+$/, "");
  const m = /^trustiot-step (begin|end) ([a-z-]+)(?: (\d+))?\n$/.exec(text);
  if (!m) return null;
  return { phase: m[1] as "begin" | "end", step: m[2] ?? "", exit: m[3] === undefined ? null : Number(m[3]) };
}

export interface PartitionWrite {
  /** The log entry. */
  entry: number;
  /** First sector, from the start of the data partition. */
  sector: number;
  data: Uint8Array;
  /** The step of the setup it belongs to (a mark's), or where it falls between them. */
  step: string;
}

export interface SetupRecord {
  /** The writes to the data partition until the end of the setup's last step, in the order of the log. */
  writes: PartitionWrite[];
  /** Each flush as the number of writes before it: everything in writes[0, flush) is on the disk. */
  flushes: number[];
  /** The steps the marks show, in order, with their writes [first, end) and the exit code. */
  steps: { step: string; first: number; end: number; exit: number }[];
  /** Entries left out: writes elsewhere on the disk (the root partition, the marks), and every entry after the last step. */
  elsewhere: number;
  after: number;
}

/**
 * The writes of the setup to the data partition (`partition`: start and size in disk sectors) and
 * the flushes, from the log of the boot that ran it, until the end mark of `lastStep`. Fails on
 * anything the crash models do not cover: a discard of the partition, a write across its edge, a
 * FUA write to it, a step mark that is not where it should be, and a log without the end of
 * `lastStep`.
 *
 * A FUA write is on the disk when it completes, without a flush, so the lost and torn models would
 * lose it where no card can. The log never holds one: QEMU's virtio-blk offers the guest a flush
 * but no FUA, so the guest's kernel sends a FUA write as a write and a flush (Laysi/trustiot-v2#918
 * found none in the logs). Should a FUA write ever appear, the record fails rather than make
 * states no power cut can leave.
 */
export function setupRecord(entries: readonly LogEntry[], partition: { start: number; sectors: number }, markSector: number, lastStep: string): SetupRecord {
  const writes: PartitionWrite[] = [];
  const flushes: number[] = [];
  const steps: SetupRecord["steps"] = [];
  let open: { step: string; first: number } | null = null;
  let elsewhere = 0;
  let where = "before the first step";
  for (const e of entries) {
    if ((e.flags & LOG_FLAGS.flush) !== 0) {
      flushes.push(writes.length);
      continue;
    }
    const end = e.sector + e.sectors;
    const overlaps = (start: number, sectors: number) => e.sector < start + sectors && end > start;
    if (overlaps(partition.start, partition.sectors)) {
      if ((e.flags & LOG_FLAGS.discard) !== 0) throw new Error(`entry ${e.index} discards sectors of the data partition`);
      if (e.sector < partition.start || end > partition.start + partition.sectors) throw new Error(`entry ${e.index} writes across the edge of the data partition`);
      if ((e.flags & LOG_FLAGS.fua) !== 0) throw new Error(`entry ${e.index} is a FUA write to the data partition, which the crash models do not cover`);
      writes.push({ entry: e.index, sector: e.sector - partition.start, data: e.data as Uint8Array, step: open?.step ?? where });
      continue;
    }
    if (overlaps(markSector, 1)) {
      const mark = e.sector === markSector && e.sectors === 1 && e.data ? parseStepMark(e.data) : null;
      if (!mark) throw new Error(`entry ${e.index} writes the mark sector ${markSector} with something that is not a step mark`);
      if (mark.phase === "begin") {
        if (open) throw new Error(`step ${mark.step} begins inside step ${open.step} (entry ${e.index})`);
        open = { step: mark.step, first: writes.length };
      } else {
        if (open?.step !== mark.step) throw new Error(`step ${mark.step} ends outside its own (entry ${e.index})`);
        steps.push({ step: mark.step, first: open.first, end: writes.length, exit: mark.exit ?? -1 });
        open = null;
        where = `after ${mark.step}`;
        if (mark.step === lastStep) return { writes, flushes, steps, elsewhere, after: entries.length - e.index - 1 };
      }
      continue;
    }
    elsewhere++;
  }
  throw new Error(`the write log has no end of the step ${lastStep}`);
}

/** Sectors [from, to) of one partition write, counted from the write's first sector. */
export interface Piece {
  write: number;
  from: number;
  to: number;
}

/** The first sector of a metadata copy, written only in part: bytes [0, at) from `head`, the rest from the other. */
export interface Tear {
  /** The write whose version of the sector is the new one; the old one is what the state holds without it. */
  write: number;
  sector: number;
  at: number;
  head: "new" | "old";
}

export interface CrashState {
  model: "cut" | "torn-cut" | "lost" | "torn";
  /** The steps of the writes the cut falls among. */
  step: string;
  /** writes[0, durable) arrived in full. */
  durable: number;
  /** Then these, in order. */
  pieces: Piece[];
  tear?: Tear;
  what: string;
}

/**
 * The parts of the partition the lost and torn models put together (see the top of this file).
 * Of the data area the setup reads only whether the partition is all zeros (when blkid finds no
 * format), so its versions are the one before the epoch, the one the epoch leaves, and its first
 * write that is not all zeros alone (zerosOnly): no subset makes it zeros unless it was before.
 * The setup writes the data area only through dm-crypt, where a sector of zeros is one written
 * back with what it held (mke2fs writes whole pages: the kernel reads the rest of a page through
 * dm-crypt and writes it back unchanged), never zeros over what was not.
 */
type Unit = { name: string; sectors: (s: number) => boolean; first?: number; zerosOnly?: boolean };
const UNITS: Unit[] = [
  ...[0, 1].flatMap((c): Unit[] => [
    { name: `copy ${c + 1} sector 0`, sectors: (s) => s === c * COPY_SECTORS, first: c * COPY_SECTORS },
    { name: `copy ${c + 1} rest`, sectors: (s) => s > c * COPY_SECTORS && s < (c + 1) * COPY_SECTORS },
  ]),
  { name: "keyslot area", sectors: (s) => s >= METADATA_SECTORS && s < HEADER_SECTORS },
  { name: "data area", sectors: (s) => s >= HEADER_SECTORS, zerosOnly: true },
];

/** The fields of a LUKS2 binary header's first 512 bytes (cryptsetup's struct luks2_hdr_disk). */
export const LUKS2_FIELDS = [
  ["magic", 0, 6],
  ["version", 6, 8],
  ["hdr_size", 8, 16],
  ["seqid", 16, 24],
  ["label", 24, 72],
  ["checksum_alg", 72, 104],
  ["salt", 104, 168],
  ["uuid", 168, 208],
  ["subsystem", 208, 256],
  ["hdr_offset", 256, 264],
  ["padding", 264, 448],
  ["csum", 448, 512],
] as const;

/**
 * Where a sector that holds `older` and is being overwritten with `newer` can tear: the field
 * boundaries of the binary header, and after each differing byte of the label (a label cut short
 * can read as another one) and the first differing byte of the magic and the UUID. Only offsets
 * whose torn sector is neither of the two.
 */
export function tearOffsets(older: Uint8Array, newer: Uint8Array): number[] {
  const at = new Set<number>();
  for (const [name, start, end] of LUKS2_FIELDS) {
    const differ: number[] = [];
    for (let i = start; i < end; i++) if (older[i] !== newer[i]) differ.push(i);
    if (differ.length === 0) continue;
    at.add(start);
    at.add(end);
    if (name === "label") for (const i of differ) at.add(i + 1);
    if (name === "magic" || name === "uuid") at.add((differ[0] as number) + 1);
  }
  const torn = (cut: number, head: "new" | "old") => tornSector(older, newer, cut, head);
  const same = (a: Uint8Array, b: Uint8Array) => a.length === b.length && a.every((x, i) => x === b[i]);
  return [...at]
    .filter((cut) => cut > 0 && cut < SECTOR)
    .filter((cut) => (["new", "old"] as const).some((head) => !same(torn(cut, head), older) && !same(torn(cut, head), newer)))
    .sort((a, b) => a - b);
}

export function tornSector(older: Uint8Array, newer: Uint8Array, at: number, head: "new" | "old"): Uint8Array {
  const out = new Uint8Array(SECTOR);
  const [first, second] = head === "new" ? [newer, older] : [older, newer];
  out.set(first.subarray(0, at), 0);
  out.set(second.subarray(at, SECTOR), at);
  return out;
}

/** The most writes of an epoch to one part whose every subset the lost model takes (2^8 - 1 subsets). */
export const SUBSET_WRITES = 8;

/** The most states the lost and torn models make of one epoch; more fails rather than run for hours. */
export const MAX_EPOCH_STATES = 4096;

/**
 * Every state the models make of the setup in `record`, epoch by epoch, with `durable` never
 * decreasing (PartitionView applies them one after the other). `baseMetadata` is what the
 * partition held in its first METADATA_SECTORS before the setup (for the torn sectors).
 */
export function* crashStates(record: SetupRecord, baseMetadata: Uint8Array): Generator<CrashState> {
  const { writes } = record;
  const n = writes.length;
  const bounds = [...new Set([0, ...record.flushes.filter((f) => f <= n), n])].sort((a, b) => a - b);
  const stepsOf = (from: number, to: number) => [...new Set(writes.slice(from, to).map((w) => w.step))].join(", ") || "no write";
  const sectorAt = (sector: number, before: number): Uint8Array => {
    for (let i = before - 1; i >= 0; i--) {
      const w = writes[i] as PartitionWrite;
      const k = sector - w.sector;
      if (k >= 0 && k * SECTOR < w.data.length) return w.data.subarray(k * SECTOR, (k + 1) * SECTOR);
    }
    return baseMetadata.subarray(sector * SECTOR, (sector + 1) * SECTOR);
  };

  for (let b = 0; b + 1 < bounds.length || (b === 0 && n === 0); b++) {
    const start = bounds[b] as number;
    const end = bounds[b + 1] ?? n;
    if (end > start) yield* epochStates(writes, start, end, stepsOf(start, end), sectorAt);
    const last = b + 2 >= bounds.length;
    for (let k = start; k < end || (last && k === end); k++) {
      yield { model: "cut", step: k < n ? (writes[k] as PartitionWrite).step : "after the last write", durable: k, pieces: [], what: k < n ? `cut before write ${k + 1} of ${n}` : `all ${n} writes` };
      if (k >= n) break;
      const w = writes[k] as PartitionWrite;
      const length = w.data.length / SECTOR;
      const cuts = new Set<number>();
      for (let j = 1; j < length; j++) if (w.sector + j <= METADATA_SECTORS) cuts.add(j);
      if (length > 1) cuts.add(Math.floor(length / 2));
      for (const j of [...cuts].sort((x, y) => x - y)) {
        yield { model: "torn-cut", step: w.step, durable: k, pieces: [{ write: k, from: 0, to: j }], what: `write ${k + 1} of ${n} cut after ${j} of its ${length} sectors` };
      }
    }
  }
}

/** The lost and torn states of the epoch writes[start, end). */
function* epochStates(
  writes: readonly PartitionWrite[],
  start: number,
  end: number,
  step: string,
  sectorAt: (sector: number, before: number) => Uint8Array,
): Generator<CrashState> {
  // For each unit: the sectors the epoch writes, and the last write (so far) of each.
  type Choice = { name: string; pieces: Piece[] };
  const options: { unit: Unit; choices: Choice[]; final: Choice }[] = [];
  for (const unit of UNITS) {
    // The epoch's writes to the unit, in the order of the log.
    const inUnit = (w: PartitionWrite) => Array.from({ length: w.data.length / SECTOR }, (_, k) => w.sector + k).filter((s) => unit.sectors(s));
    const touching: number[] = [];
    for (let i = start; i < end; i++) if (inUnit(writes[i] as PartitionWrite).length > 0) touching.push(i);
    if (touching.length === 0) continue;
    // The unit as a subset of those writes left it: of each sector, the last write of the subset.
    const latestOf = (subset: readonly number[]) => {
      const latest = new Map<number, number>();
      for (const i of subset) for (const s of inUnit(writes[i] as PartitionWrite)) latest.set(s, i);
      return latest;
    };
    const choices: Choice[] = [{ name: "as before the epoch", pieces: [] }];
    const byContent = new Map<string, Choice>();
    const add = (name: string, pieces: Piece[]): Choice => {
      const key = contentKey(writes, pieces);
      const known = byContent.get(key);
      if (known) return known;
      const choice = { name, pieces };
      byContent.set(key, choice);
      choices.push(choice);
      return choice;
    };
    const subsetName = (subset: readonly number[]) => (subset.length === 1 ? `only write ${(subset[0] as number) + 1} of the epoch` : `only writes ${subset.map((i) => i + 1).join(", ")} of the epoch`);
    // Every prefix (a card that keeps the writes in order); the last is what the epoch leaves.
    let final: Choice | null = null;
    for (let n = unit.zerosOnly ? touching.length : 1; n <= touching.length; n++) {
      final = add(`as write ${(touching[n - 1] as number) + 1} left it`, piecesOf(writes, latestOf(touching.slice(0, n))));
    }
    if (unit.zerosOnly) {
      const nonZero = touching.find((i) => {
        const w = writes[i] as PartitionWrite;
        return inUnit(w).some((s) => w.data.subarray((s - w.sector) * SECTOR, (s - w.sector + 1) * SECTOR).some((b) => b !== 0));
      });
      if (nonZero !== undefined) add(subsetName([nonZero]), piecesOf(writes, latestOf([nonZero])));
    } else if (touching.length <= SUBSET_WRITES) {
      for (let mask = 1; mask < 1 << touching.length; mask++) {
        const subset = touching.filter((_, b) => (mask & (1 << b)) !== 0);
        add(subsetName(subset), piecesOf(writes, latestOf(subset)));
      }
    } else {
      for (const i of touching) add(subsetName([i]), piecesOf(writes, latestOf([i])));
      for (const i of touching) {
        const rest = touching.filter((j) => j !== i);
        add(`all but write ${i + 1} of the epoch`, piecesOf(writes, latestOf(rest)));
      }
    }
    // A mix of sectors from before the epoch and from it (the metadata copies' rest, and the
    // keyslot area): the first sector the epoch writes only, and all but that one.
    const latest = latestOf(touching);
    const sectors = [...latest.keys()].sort((a, b) => a - b);
    if (unit.first === undefined && !unit.zerosOnly && sectors.length > 1) {
      const first = sectors[0] as number;
      add(`only its sector ${first} from the epoch`, piecesOf(writes, new Map([[first, latest.get(first) as number]])));
      add(`all but its sector ${first} from the epoch`, piecesOf(writes, new Map([...latest].filter(([s]) => s !== first))));
    }
    options.push({ unit, choices, final: final as Choice });
  }

  const total = options.reduce((p, o) => p * o.choices.length, 1);
  if (total > MAX_EPOCH_STATES) throw new Error(`writes ${start + 1} to ${end} make ${total} lost states, more than ${MAX_EPOCH_STATES}`);
  // Every combination but all before the epoch and all from its last write (cut states).
  const picks = new Array<number>(options.length).fill(0);
  for (let count = 0; count < total; count++) {
    let rest = count;
    for (let u = 0; u < options.length; u++) {
      const size = (options[u] as (typeof options)[number]).choices.length;
      picks[u] = rest % size;
      rest = Math.floor(rest / size);
    }
    const chosen = options.map((o, u) => o.choices[picks[u] as number] as Choice);
    if (chosen.every((c) => c.pieces.length === 0)) continue;
    if (chosen.every((c, u) => c === (options[u] as (typeof options)[number]).final)) continue;
    yield {
      model: "lost",
      step,
      durable: start,
      pieces: chosen.flatMap((c) => c.pieces),
      what: `writes ${start + 1} to ${end} since the last flush: ${options.map((o, u) => `${o.unit.name} ${(chosen[u] as { name: string }).name}`).join("; ")}`,
    };
  }

  // A torn first sector of a metadata copy: between what it held before the epoch and each
  // version the epoch writes; every other part (the rest of that copy, the other copy, the
  // keyslot area, the data area) as before the epoch or as the epoch left it.
  for (const o of options) {
    if (o.unit.first === undefined) continue;
    const sector = o.unit.first;
    const older = sectorAt(sector, start);
    for (const version of o.choices.slice(1)) {
      const piece = version.pieces[0] as Piece;
      const w = writes[piece.write] as PartitionWrite;
      const newer = w.data.subarray((sector - w.sector) * SECTOR, (sector - w.sector + 1) * SECTOR);
      const others = options.filter((x) => x !== o);
      const combos = others.reduce<Choice[][]>((acc, x) => acc.flatMap((c) => [...new Set([x.choices[0] as Choice, x.final])].map((v) => [...c, v])), [[]]);
      const offsets = tearOffsets(older, newer);
      if (offsets.length * 2 * combos.length > MAX_EPOCH_STATES) throw new Error(`writes ${start + 1} to ${end} make ${offsets.length * 2 * combos.length} torn states of ${o.unit.name}, more than ${MAX_EPOCH_STATES}`);
      for (const at of offsets) {
        for (const head of ["new", "old"] as const) {
          for (const combo of combos) {
            yield {
              model: "torn",
              step,
              durable: start,
              pieces: combo.flatMap((c) => c.pieces),
              tear: { write: piece.write, sector, at, head },
              what: `writes ${start + 1} to ${end} since the last flush: ${o.unit.name} torn at byte ${at} (bytes before it ${head === "new" ? `from write ${piece.write + 1}` : "as before"}, after it ${head === "new" ? "as before" : `from write ${piece.write + 1}`}); ${others.map((x, i) => `${x.unit.name} ${(combo[i] as { name: string }).name}`).join("; ")}`,
            };
          }
        }
      }
    }
  }
}

/** The sectors of `latest` (sector -> write) as pieces, in sector order, runs of one write merged. */
function piecesOf(writes: readonly PartitionWrite[], latest: ReadonlyMap<number, number>): Piece[] {
  const pieces: Piece[] = [];
  for (const sector of [...latest.keys()].sort((a, b) => a - b)) {
    const write = latest.get(sector) as number;
    const k = sector - (writes[write] as PartitionWrite).sector;
    const last = pieces.at(-1);
    if (last && last.write === write && last.to === k) last.to = k + 1;
    else pieces.push({ write, from: k, to: k + 1 });
  }
  return pieces;
}

function contentKey(writes: readonly PartitionWrite[], pieces: Piece[]): string {
  const hash = createHash("sha256");
  for (const p of pieces) {
    const w = writes[p.write] as PartitionWrite;
    hash.update(`${w.sector + p.from}:`);
    hash.update(w.data.subarray(p.from * SECTOR, p.to * SECTOR));
  }
  return hash.digest("hex");
}

/**
 * The data partition as the setup reads it: the header area byte for byte, and which sectors of
 * the data area are not zeros. A copy shares both with the view it was made from until either
 * writes to them (copy on write), so the many states of one epoch cost little.
 */
export class PartitionView {
  private headerHash: string | null = null;

  private constructor(
    private headerBytes: Uint8Array,
    private headerShared: boolean,
    private nonZero: Uint8Array,
    private nonZeroCount: number,
    private nonZeroShared: boolean,
  ) {}

  /** The header area (read only: write with put()). */
  get header(): Uint8Array {
    return this.headerBytes;
  }

  /** The partition before the setup: its header area and the data-area sectors that are not zeros. */
  static of(header: Uint8Array, nonZeroDataSectors: Iterable<number>): PartitionView {
    if (header.length !== HEADER_SECTORS * SECTOR) throw new Error(`the header area has ${header.length} bytes, expected ${HEADER_SECTORS * SECTOR}`);
    const nonZero = new Uint8Array(DATA.sizeSectors - HEADER_SECTORS);
    let count = 0;
    for (const s of nonZeroDataSectors) {
      if (s < HEADER_SECTORS || s >= DATA.sizeSectors) throw new Error(`sector ${s} is not in the data area`);
      if (nonZero[s - HEADER_SECTORS] === 0) count++;
      nonZero[s - HEADER_SECTORS] = 1;
    }
    return new PartitionView(new Uint8Array(header), false, nonZero, count, false);
  }

  clone(): PartitionView {
    this.headerShared = true;
    this.nonZeroShared = true;
    const copy = new PartitionView(this.headerBytes, true, this.nonZero, this.nonZeroCount, true);
    copy.headerHash = this.headerHash;
    return copy;
  }

  put(sector: number, bytes: Uint8Array): void {
    if (bytes.length % SECTOR !== 0 || sector < 0 || sector + bytes.length / SECTOR > DATA.sizeSectors) throw new Error(`cannot write ${bytes.length} bytes at sector ${sector}`);
    for (let k = 0; k < bytes.length / SECTOR; k++) {
      const s = sector + k;
      const block = bytes.subarray(k * SECTOR, (k + 1) * SECTOR);
      if (s < HEADER_SECTORS) {
        if (this.headerShared) {
          this.headerBytes = new Uint8Array(this.headerBytes);
          this.headerShared = false;
        }
        this.headerBytes.set(block, s * SECTOR);
        this.headerHash = null;
        continue;
      }
      const zero = block.every((b) => b === 0);
      const was = this.nonZero[s - HEADER_SECTORS] === 1;
      if (zero === !was) continue;
      if (this.nonZeroShared) {
        this.nonZero = new Uint8Array(this.nonZero);
        this.nonZeroShared = false;
      }
      this.nonZero[s - HEADER_SECTORS] = zero ? 0 : 1;
      this.nonZeroCount += zero ? -1 : 1;
    }
  }

  /** One sector of the header area. */
  get(sector: number): Uint8Array {
    if (sector < 0 || sector >= HEADER_SECTORS) throw new Error(`sector ${sector} is not in the header area`);
    return this.headerBytes.slice(sector * SECTOR, (sector + 1) * SECTOR);
  }

  get dataIsZero(): boolean {
    return this.nonZeroCount === 0;
  }

  /** The SHA-256 of the header area (hex). */
  headerSha256(): string {
    this.headerHash ??= createHash("sha256").update(this.headerBytes).digest("hex");
    return this.headerHash;
  }

  /** Two views with the same key look the same to the setup. */
  key(): string {
    return `${this.headerSha256()}${this.dataIsZero ? "" : "+data"}`;
  }
}

/**
 * Applies a state's writes on top of what the partition held before them: writes[from, durable)
 * in full (the caller has applied writes[0, from)), then its pieces and its torn sector. `put`
 * writes partition sectors; `get` reads one sector of the header area as it is so far.
 */
export function applyState(
  writes: readonly PartitionWrite[],
  state: CrashState,
  target: { put: (sector: number, bytes: Uint8Array) => void; get: (sector: number) => Uint8Array },
  from = 0,
): void {
  for (let i = from; i < state.durable; i++) {
    const w = writes[i] as PartitionWrite;
    target.put(w.sector, w.data);
  }
  for (const p of state.pieces) {
    const w = writes[p.write] as PartitionWrite;
    target.put(w.sector + p.from, w.data.subarray(p.from * SECTOR, p.to * SECTOR));
  }
  if (state.tear) {
    const { write, sector, at, head } = state.tear;
    const w = writes[write] as PartitionWrite;
    const newer = w.data.subarray((sector - w.sector) * SECTOR, (sector - w.sector + 1) * SECTOR);
    target.put(sector, tornSector(target.get(sector), newer, at, head));
  }
}
