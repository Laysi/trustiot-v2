// The boot test's write log (src/write-log.ts; Laysi/trustiot-v2#918): QEMU's blklogwrites log of
// a recorded setup, the setup's writes and steps in it, and the states a power cut can leave.
import { describe, expect, test } from "bun:test";
import { targetTable } from "../src/layout.ts";
import { DATA, SECTOR } from "../src/pins.ts";
import {
  type CrashState,
  HEADER_SECTORS,
  LOG_FLAGS,
  LUKS2_FIELDS,
  MAX_EPOCH_STATES,
  METADATA_SECTORS,
  PartitionView,
  type PartitionWrite,
  STEP_MARK_SECTOR,
  SUBSET_WRITES,
  type SetupRecord,
  applyState,
  crashStates,
  parseStepMark,
  parseWriteLog,
  setupRecord,
  tearOffsets,
  tornSector,
} from "../src/write-log.ts";

const COPY = METADATA_SECTORS / 2;

interface Entry {
  sector: number;
  sectors?: number;
  flags?: number;
  data?: Uint8Array;
}

/** A log as QEMU's blklogwrites writes it: super block, then per entry its sector and its data. */
function logOf(entries: Entry[], options: { magic?: bigint; version?: bigint; sectorSize?: number; count?: number } = {}): Uint8Array {
  const parts: Uint8Array[] = [];
  const superBlock = new Uint8Array(SECTOR);
  const sv = new DataView(superBlock.buffer);
  sv.setBigUint64(0, options.magic ?? 0x6a736677736872n, true);
  sv.setBigUint64(8, options.version ?? 1n, true);
  sv.setBigUint64(16, BigInt(options.count ?? entries.length), true);
  sv.setUint32(24, options.sectorSize ?? SECTOR, true);
  parts.push(superBlock);
  for (const e of entries) {
    const head = new Uint8Array(SECTOR);
    const hv = new DataView(head.buffer);
    const sectors = e.sectors ?? (e.data ? e.data.length / SECTOR : 0);
    hv.setBigUint64(0, BigInt(e.sector), true);
    hv.setBigUint64(8, BigInt(sectors), true);
    hv.setBigUint64(16, BigInt(e.flags ?? 0), true);
    // QEMU leaves data_len at 0 and puts the data in the sectors that follow.
    parts.push(head);
    if (e.data) parts.push(e.data);
  }
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

const filled = (sectors: number, byte: number) => new Uint8Array(sectors * SECTOR).fill(byte);
const markData = (text: string) => {
  const d = new Uint8Array(SECTOR);
  d.set(new TextEncoder().encode(text));
  return d;
};
const flush: Entry = { sector: 0, flags: LOG_FLAGS.flush };
const PART = { start: 1000, sectors: DATA.sizeSectors };
const MARK = 100;
const begin = (step: string): Entry => ({ sector: MARK, data: markData(`trustiot-step begin ${step}\n`) });
const end = (step: string, exit = 0): Entry => ({ sector: MARK, data: markData(`trustiot-step end ${step} ${exit}\n`) });

describe("the write log", () => {
  test("its entries: writes with their data, flushes and discards without", () => {
    const entries = parseWriteLog(
      logOf([
        { sector: 7, data: filled(2, 0xaa) },
        flush,
        { sector: 9, sectors: 4, flags: LOG_FLAGS.discard },
        { sector: 3, data: filled(1, 0), flags: LOG_FLAGS.fua },
      ]),
    );
    expect(entries.map((e) => ({ index: e.index, sector: e.sector, sectors: e.sectors, flags: e.flags, bytes: e.data?.length ?? null }))).toEqual([
      { index: 0, sector: 7, sectors: 2, flags: 0, bytes: 2 * SECTOR },
      { index: 1, sector: 0, sectors: 0, flags: LOG_FLAGS.flush, bytes: null },
      { index: 2, sector: 9, sectors: 4, flags: LOG_FLAGS.discard, bytes: null },
      { index: 3, sector: 3, sectors: 1, flags: LOG_FLAGS.fua, bytes: SECTOR },
    ]);
    expect(entries[0]?.data?.every((b) => b === 0xaa)).toBe(true);
  });

  test("anything but a dm-log-writes log of 512-byte sectors is refused", () => {
    const write = [{ sector: 7, data: filled(1, 1) }];
    expect(() => parseWriteLog(new Uint8Array(100))).toThrow("less than its super block");
    expect(() => parseWriteLog(logOf(write, { magic: 1n }))).toThrow("magic");
    expect(() => parseWriteLog(logOf(write, { version: 2n }))).toThrow("version");
    expect(() => parseWriteLog(logOf(write, { sectorSize: 4096 }))).toThrow("sectors are 4096 bytes");
  });

  test("a log that ends early, or holds what QEMU never writes, is refused", () => {
    const write = [{ sector: 7, data: filled(2, 1) }];
    expect(() => parseWriteLog(logOf(write, { count: 2 }))).toThrow("ends inside entry 1");
    expect(() => parseWriteLog(logOf(write).subarray(0, 2 * SECTOR + 10))).toThrow("ends inside the data of entry 0");
    expect(() => parseWriteLog(logOf([{ sector: 7, data: filled(1, 1), flags: LOG_FLAGS.mark }]))).toThrow("flags 0x8");
    expect(() => parseWriteLog(logOf([{ sector: 0, data: filled(1, 1), flags: LOG_FLAGS.flush }]))).toThrow("a flush with data");
    const padded = logOf(write);
    padded[SECTOR + 40] = 1;
    expect(() => parseWriteLog(padded)).toThrow("not followed by zeros");
    const length = logOf(write);
    new DataView(length.buffer).setBigUint64(SECTOR + 24, 3n * BigInt(SECTOR), true);
    expect(() => parseWriteLog(length)).toThrow("bytes of data for 2 sectors");
  });

  test("step marks as power-cut.sh writes them", () => {
    expect(parseStepMark(markData("trustiot-step begin write-header\n"))).toEqual({ phase: "begin", step: "write-header", exit: null });
    expect(parseStepMark(markData("trustiot-step end relabel 0\n"))).toEqual({ phase: "end", step: "relabel", exit: 0 });
    expect(parseStepMark(markData("trustiot-step end mkfs 1\n"))).toEqual({ phase: "end", step: "mkfs", exit: 1 });
    for (const text of ["", "trustiot-step begin\n", "trustiot-step middle mkfs\n", "trustiot-step begin mkfs", "power-cut mkfs\n"]) expect(parseStepMark(markData(text))).toBeNull();
  });

  test("the step marks' sector lies between the MBR and the image's first partition", () => {
    const starts = targetTable().partitions.map((p) => p.start);
    expect(Math.min(...starts)).toBe(16384);
    expect(STEP_MARK_SECTOR).toBeGreaterThan(0);
    expect(STEP_MARK_SECTOR).toBeLessThan(Math.min(...starts));
  });
});

describe("the setup's writes in the log", () => {
  test("writes to the data partition by step, the flushes, and what is left out", () => {
    const entries = parseWriteLog(
      logOf([
        { sector: 50, data: filled(1, 9) }, // the root file system, before the setup
        begin("write-header"),
        { sector: PART.start, data: filled(64, 1) },
        flush,
        end("write-header"),
        { sector: 50, data: filled(1, 9) },
        { sector: PART.start + 64, data: filled(8, 2) }, // between two steps
        begin("relabel"),
        { sector: PART.start + 8, data: filled(8, 3) },
        flush,
        end("relabel"),
        { sector: PART.start + HEADER_SECTORS, data: filled(8, 4) }, // the mount, after the setup
        flush,
      ]),
    );
    const rec = setupRecord(entries, PART, MARK, "relabel");
    expect(rec.writes.map((w) => ({ entry: w.entry, sector: w.sector, sectors: w.data.length / SECTOR, step: w.step }))).toEqual([
      { entry: 2, sector: 0, sectors: 64, step: "write-header" },
      { entry: 6, sector: 64, sectors: 8, step: "after write-header" },
      { entry: 8, sector: 8, sectors: 8, step: "relabel" },
    ]);
    expect(rec.flushes).toEqual([1, 3]);
    expect(rec.steps).toEqual([
      { step: "write-header", first: 0, end: 1, exit: 0 },
      { step: "relabel", first: 2, end: 3, exit: 0 },
    ]);
    expect({ elsewhere: rec.elsewhere, after: rec.after }).toEqual({ elsewhere: 2, after: 2 });
  });

  test("a step that failed keeps its exit code", () => {
    const rec = setupRecord(parseWriteLog(logOf([begin("mkfs"), end("mkfs", 1)])), PART, MARK, "mkfs");
    expect(rec.steps).toEqual([{ step: "mkfs", first: 0, end: 0, exit: 1 }]);
  });

  test("whatever the crash models do not cover is refused", () => {
    const record = (entries: Entry[]) => () => setupRecord(parseWriteLog(logOf(entries)), PART, MARK, "relabel");
    expect(record([{ sector: PART.start + 8, sectors: 8, flags: LOG_FLAGS.discard }])).toThrow("discards sectors of the data partition");
    expect(record([{ sector: PART.start - 1, data: filled(2, 1) }])).toThrow("across the edge");
    expect(record([{ sector: PART.start + 8, data: filled(1, 1), flags: LOG_FLAGS.fua }])).toThrow("a FUA write to the data partition");
    expect(record([{ sector: PART.start + PART.sectors - 1, data: filled(2, 1) }])).toThrow("across the edge");
    expect(record([{ sector: MARK, data: filled(1, 7) }])).toThrow("not a step mark");
    expect(record([{ sector: MARK - 1, data: filled(2, 7) }])).toThrow("not a step mark");
    expect(record([begin("mkfs"), begin("relabel")])).toThrow("begins inside step mkfs");
    expect(record([begin("mkfs"), end("relabel")])).toThrow("ends outside its own");
    expect(record([begin("mkfs"), end("mkfs")])).toThrow("no end of the step relabel");
  });
});

/** A record of writes (partition sectors), with flushes after the given numbers of writes. */
function recordOf(writes: { sector: number; data: Uint8Array; step?: string }[], flushes: number[]): SetupRecord {
  return {
    writes: writes.map((w, i): PartitionWrite => ({ entry: i, sector: w.sector, data: w.data, step: w.step ?? "s" })),
    flushes,
    steps: [],
    elsewhere: 0,
    after: 0,
  };
}

/** A LUKS2 binary header's first sector with this label, seqid and magic. */
function binaryHeader(label: string, seqid: number, magic = "LUKS\xba\xbe"): Uint8Array {
  const s = new Uint8Array(SECTOR);
  s.set(
    [...magic].map((c) => c.charCodeAt(0)),
    0,
  );
  s[7] = 2;
  s[23] = seqid;
  s.set(new TextEncoder().encode(label), 24);
  s.set(new TextEncoder().encode("sha256"), 72);
  s.set(new TextEncoder().encode("9da7f680-5912-4d5c-bca6-d46c83e064f8"), 168);
  s.fill(seqid, 448, 480);
  return s;
}

function viewOf(writes: readonly PartitionWrite[], state: CrashState, base = PartitionView.of(new Uint8Array(HEADER_SECTORS * SECTOR), [])): PartitionView {
  const view = base.clone();
  applyState(writes, state, view);
  return view;
}

describe("torn sectors of a metadata copy", () => {
  test("the offsets are the boundaries of the fields that differ, and inside the label, the magic and the UUID", () => {
    const older = binaryHeader("trustiot-data", 5);
    const newer = binaryHeader("trustiot-data-new", 3);
    const offsets = tearOffsets(older, newer);
    // seqid, label and csum differ: their boundaries, and after each label byte that differs (37
    // to 40). Not 16: the bytes before the seqid are the same in both, so a cut there tears nothing.
    expect(offsets).toEqual([24, 38, 39, 40, 41, 72, 448]);
    // The magic and the UUID are the same in both: no cut inside them.
    for (const [name, start] of LUKS2_FIELDS) if (name === "magic" || name === "uuid") expect(offsets).not.toContain(start + 1);
  });

  test("against zeros: every field the header fills, one cut inside the magic and the UUID", () => {
    const offsets = tearOffsets(new Uint8Array(SECTOR), binaryHeader("trustiot-data-new", 1));
    expect(offsets).toContain(1); // after the first byte of the magic
    expect(offsets).toContain(169); // after the first byte of the UUID
    expect(offsets).toContain(24 + "trustiot-data-new".length);
    // Every offset gives a sector that is neither the old one nor the new one.
    for (const at of offsets) {
      const torn = [tornSector(new Uint8Array(SECTOR), binaryHeader("trustiot-data-new", 1), at, "new"), tornSector(new Uint8Array(SECTOR), binaryHeader("trustiot-data-new", 1), at, "old")];
      expect(torn.some((t) => !t.every((b) => b === 0) && !t.every((b, i) => b === binaryHeader("trustiot-data-new", 1)[i]))).toBe(true);
    }
  });

  test("a torn sector takes its first bytes from one version and the rest from the other", () => {
    const older = filled(1, 1);
    const newer = filled(1, 2);
    expect([...tornSector(older, newer, 3, "new").subarray(0, 5)]).toEqual([2, 2, 2, 1, 1]);
    expect([...tornSector(older, newer, 3, "old").subarray(0, 5)]).toEqual([1, 1, 1, 2, 2]);
  });
});

describe("the states a power cut can leave", () => {
  // Two epochs: the header's two metadata copies in one write, then a flush; then the keyslot
  // area in two writes and a data-area write, then a flush.
  const header = new Uint8Array(METADATA_SECTORS * SECTOR);
  header.set(binaryHeader("trustiot-data-new", 1), 0);
  header.fill(7, SECTOR, COPY * SECTOR);
  header.set(binaryHeader("trustiot-data-new", 1), COPY * SECTOR);
  header.fill(8, (COPY + 1) * SECTOR);
  const rec = recordOf(
    [
      { sector: 0, data: header, step: "write-header" },
      { sector: METADATA_SECTORS, data: filled(4, 5), step: "write-keyslots" },
      { sector: METADATA_SECTORS + 4, data: filled(4, 6), step: "write-keyslots" },
      { sector: HEADER_SECTORS + 10, data: filled(2, 9), step: "mkfs" },
    ],
    [1, 4],
  );
  const zeros = new Uint8Array(METADATA_SECTORS * SECTOR);
  const states = [...crashStates(rec, zeros)];
  const by = (model: CrashState["model"]) => states.filter((s) => s.model === model);

  test("a cut before every write and after the last, in the order of the log", () => {
    expect(by("cut").map((s) => s.durable)).toEqual([0, 1, 2, 3, 4]);
    expect(by("cut").at(-1)?.what).toBe("all 4 writes");
  });

  test("a write cut short: at every sector boundary inside the metadata, and in the middle elsewhere", () => {
    const first = by("torn-cut").filter((s) => s.pieces[0]?.write === 0);
    expect(first.map((s) => s.pieces[0]?.to)).toEqual(Array.from({ length: METADATA_SECTORS - 1 }, (_, j) => j + 1));
    expect(by("torn-cut").filter((s) => s.pieces[0]?.write === 1).map((s) => s.pieces[0]?.to)).toEqual([2]);
  });

  test("states come epoch by epoch, never taking back a write that was flushed", () => {
    let durable = 0;
    for (const s of states) {
      expect(s.durable).toBeGreaterThanOrEqual(durable);
      durable = s.durable;
      const flushedBefore = Math.max(0, ...rec.flushes.filter((f) => f <= s.durable));
      for (const p of s.pieces) expect(p.write).toBeGreaterThanOrEqual(flushedBefore);
    }
  });

  test("lost writes: each part of the partition as before the epoch, as one of its writes left it, or mixed", () => {
    const lost = by("lost");
    // The first epoch: four parts (each copy's first sector and rest), the rest of a copy also
    // mixed; all as before or all as the write left them are the cut states, not lost ones.
    expect(lost.filter((s) => s.durable === 0)).toHaveLength(2 * 4 * 2 * 4 - 2);
    // A lost state in the first epoch: copy 1's first sector arrived, nothing else.
    const onlyFirst = lost.find((s) => s.durable === 0 && s.pieces.length === 1 && s.pieces[0]?.from === 0 && s.pieces[0]?.to === 1);
    expect(onlyFirst).toBeDefined();
    const view = viewOf(rec.writes, onlyFirst as CrashState);
    expect([...view.get(0).subarray(0, 6)]).toEqual([...binaryHeader("x", 1).subarray(0, 6)]);
    expect(view.get(1).every((b) => b === 0)).toBe(true);
    // The second epoch: the keyslot area before, after either write in order, with only the
    // second write (the first lost), or mixed; the data area before or after.
    expect(lost.filter((s) => s.durable === 1)).toHaveLength((1 + 2 + 1 + 2) * 2 - 2);
    const laterKept = lost.find((s) => s.durable === 1 && s.what.includes("keyslot area only write 3 of the epoch;"));
    expect(laterKept?.pieces).toEqual([{ write: 2, from: 0, to: 4 }]);
  });

  test("a part written more often than every subset allows: every prefix, every write alone, every write left out", () => {
    const writes = Array.from({ length: SUBSET_WRITES + 2 }, (_, i) => ({ sector: METADATA_SECTORS + 4 * i, data: filled(4, i + 1) }));
    const lost = [...crashStates(recordOf(writes, [writes.length]), zeros)].filter((s) => s.model === "lost");
    const n = writes.length;
    // Before, n prefixes, n - 1 more writes alone (the first alone is the first prefix), n - 1
    // writes left out (all but the last is a prefix), two sector mixes; less before and the last prefix.
    expect(lost).toHaveLength(1 + n + (n - 1) + (n - 1) + 2 - 2);
    expect(lost.some((s) => s.what.endsWith("keyslot area all but write 1 of the epoch"))).toBe(true);
    expect(lost.some((s) => s.what.endsWith("keyslot area only write 5 of the epoch"))).toBe(true);
  });

  test("the data area: before, as the epoch left it, or its first write that is not zeros alone", () => {
    const writes = [
      { sector: HEADER_SECTORS, data: filled(1, 0) },
      { sector: HEADER_SECTORS + 1, data: filled(1, 3) },
      { sector: HEADER_SECTORS + 2, data: filled(1, 4) },
    ];
    const lost = [...crashStates(recordOf(writes, [3]), zeros)].filter((s) => s.model === "lost");
    expect(lost.map((s) => s.pieces)).toEqual([[{ write: 1, from: 0, to: 1 }]]);
  });

  test("torn first sectors of a metadata copy, with the rest of each copy as before or as written", () => {
    const torn = by("torn");
    expect(torn.length).toBeGreaterThan(0);
    for (const s of torn) {
      expect(s.durable).toBe(0);
      expect([0, COPY]).toContain(s.tear?.sector as number);
    }
    // Copy 1's magic cut after its first byte: blkid finds no LUKS2 magic there.
    const magic = torn.find((s) => s.tear?.sector === 0 && s.tear.at === 1 && s.tear.head === "new" && s.pieces.length === 0);
    expect(magic).toBeDefined();
    const view = viewOf(rec.writes, magic as CrashState);
    expect([...view.get(0).subarray(0, 3)]).toEqual([0x4c, 0, 0]);
  });

  test("a torn first sector with the keyslot area of the same epoch as before or as written", () => {
    const both = recordOf(
      [
        { sector: 0, data: header },
        { sector: METADATA_SECTORS, data: filled(4, 5) },
      ],
      [2],
    );
    const torn = [...crashStates(both, zeros)].filter((s) => s.model === "torn" && s.tear?.sector === 0 && s.tear.at === 1 && s.tear.head === "new");
    const keyslots = torn.map((s) => /keyslot area ([^;]*)/.exec(s.what)?.[1]);
    expect(new Set(keyslots)).toEqual(new Set(["as before the epoch", "as write 2 left it"]));
  });

  test("states without writes: only the partition as it was", () => {
    expect([...crashStates(recordOf([], []), zeros)]).toEqual([{ model: "cut", step: "after the last write", durable: 0, pieces: [], what: "all 0 writes" }]);
  });

  test("an epoch that would make too many states fails instead of running for hours", () => {
    // Three parts of the header, 20 versions of each, none flushed: 21 * 21 * 21 combinations.
    const many = Array.from({ length: 60 }, (_, i) => ({ sector: [1, COPY + 1, METADATA_SECTORS][i % 3] as number, data: filled(1, i + 1) }));
    expect(() => [...crashStates(recordOf(many, []), zeros)]).toThrow(`more than ${MAX_EPOCH_STATES}`);
  });
});

describe("the partition as the setup reads it", () => {
  test("a copy shares the header and the data area until either is written", () => {
    const a = PartitionView.of(new Uint8Array(HEADER_SECTORS * SECTOR), []);
    const b = a.clone();
    b.put(3, filled(1, 4));
    expect(a.get(3).every((x) => x === 0)).toBe(true);
    expect(b.get(3).every((x) => x === 4)).toBe(true);
    expect(a.key()).not.toBe(b.key());
    expect(a.clone().key()).toBe(a.key());
  });

  test("the data area counts only as zeros or not", () => {
    const a = PartitionView.of(new Uint8Array(HEADER_SECTORS * SECTOR), []);
    const b = a.clone();
    b.put(HEADER_SECTORS + 5, filled(2, 1));
    expect([a.dataIsZero, b.dataIsZero]).toEqual([true, false]);
    expect(b.key()).toBe(`${a.headerSha256()}+data`);
    b.put(HEADER_SECTORS + 5, filled(1, 0));
    expect(b.dataIsZero).toBe(false);
    b.put(HEADER_SECTORS + 6, filled(1, 0));
    expect(b.key()).toBe(a.key());
    expect(a.dataIsZero).toBe(true);
    const c = PartitionView.of(new Uint8Array(HEADER_SECTORS * SECTOR), [HEADER_SECTORS, DATA.sizeSectors - 1]);
    expect(c.dataIsZero).toBe(false);
  });

  test("writes outside the partition or not whole sectors are refused", () => {
    const a = PartitionView.of(new Uint8Array(HEADER_SECTORS * SECTOR), []);
    expect(() => a.put(DATA.sizeSectors - 1, filled(2, 1))).toThrow();
    expect(() => a.put(0, new Uint8Array(100))).toThrow();
    expect(() => a.get(HEADER_SECTORS)).toThrow();
    expect(() => PartitionView.of(new Uint8Array(SECTOR), [])).toThrow();
    expect(() => PartitionView.of(new Uint8Array(HEADER_SECTORS * SECTOR), [3])).toThrow("not in the data area");
  });

  test("a state is its durable writes, then its pieces, then its torn sector", () => {
    const writes = recordOf(
      [
        { sector: 0, data: filled(2, 1) },
        { sector: 1, data: filled(2, 2) },
        { sector: COPY, data: filled(1, 3) },
      ],
      [],
    ).writes;
    const view = viewOf(writes, { model: "torn", step: "s", durable: 1, pieces: [{ write: 1, from: 1, to: 2 }], tear: { write: 2, sector: COPY, at: 10, head: "new" }, what: "" });
    expect([view.get(0)[0], view.get(1)[0], view.get(2)[0]]).toEqual([1, 1, 2]);
    expect([view.get(COPY)[9], view.get(COPY)[10]]).toEqual([3, 0]);
  });
});
