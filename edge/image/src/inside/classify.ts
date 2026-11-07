// The boot test's look at what a power cut inside the setup can leave (README.md, section
// 「開機測試」; Laysi/trustiot-v2#918): runs the image's own trustiot-data-setup on each state
// src/write-log.ts rebuilds, in a chroot of the power-cut copy's root file system, through
// boot-test/classify.sh, with the image's own bash, blkid, cryptsetup and systemd-cryptenroll.
// Each worker has its own copy of the setup (FACTORY_FLAG pointed at a file of its own) and its
// own partition file in the chroot's /tmp (a tmpfs): a sparse file of the partition's size that
// holds the state's header area, and a sector that is not zeros behind it when the state's data
// area is not all zeros, which is all the setup reads of the data area.
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { closeSync, openSync, readSync, writeSync } from "node:fs";
import { join } from "node:path";
import { MAX_RUNS, type SetupRunOn, parseSetupRun } from "../boot-results.ts";
import { DATA, DATA_CHECK, SECTOR } from "../pins.ts";
import { HEADER_SECTORS, type PartitionView } from "../write-log.ts";
import { Chroot, Ext4Mount, PATHS, run } from "./common.ts";

const SETUP = "usr/libexec/trustiot/trustiot-data-setup";
/** Runs of the setup at once. */
export const CLASSIFY_WORKERS = 4;
/** Bound for one run of the setup on a file: it reads the header with blkid and cryptsetup, and talks to no TPM. */
const RUN_LIMIT_S = 300;

export class Classifier {
  private readonly idle: number[] = [];
  private readonly waiting: ((worker: number) => void)[] = [];
  /** Runs of the setup so far. */
  runs = 0;

  private constructor(
    private readonly root: string,
    private readonly mount: Ext4Mount,
    private readonly chroot: Chroot,
  ) {
    for (let w = 0; w < CLASSIFY_WORKERS; w++) this.idle.push(w);
  }

  /** Mounts `rootFile` (the power-cut copy's root file system) read-only at `dir` and prepares the workers. */
  static async open(rootFile: string, dir: string): Promise<Classifier> {
    const mount = await Ext4Mount.open(rootFile, dir, true);
    const chroot = new Chroot(dir);
    try {
      await chroot.enter({ network: false, sys: true });
      const setup = await readFile(join(dir, SETUP), "utf8");
      const line = `readonly FACTORY_FLAG=${DATA_CHECK.factoryFlag}`;
      if (setup.split("\n").filter((l) => l === line).length !== 1) throw new Error(`/${SETUP} does not have the line ${line} exactly once`);
      await writeFile(join(dir, "tmp/classify.sh"), await readFile(join(PATHS.src, "boot-test/classify.sh")), { mode: 0o755 });
      for (let w = 0; w < CLASSIFY_WORKERS; w++) {
        const wd = join(dir, `tmp/classify-${w}`);
        await mkdir(wd, { recursive: true });
        await writeFile(join(wd, "setup"), setup.replace(line, `readonly FACTORY_FLAG=/tmp/classify-${w}/flag`), { mode: 0o755 });
        await run(["truncate", "-s", String(DATA.sizeSectors * SECTOR), join(wd, "partition")]);
      }
    } catch (error) {
      await chroot.leave();
      await mount.unmount();
      throw error;
    }
    return new Classifier(dir, mount, chroot);
  }

  async close(): Promise<void> {
    await this.chroot.leave();
    await this.mount.unmount();
  }

  /**
   * Runs the setup on the state `view`, and again on the header area each run leaves, as the next
   * boot would: until a run leaves the header area as it was or formats the partition (a real boot
   * writes a new header then), at most MAX_RUNS times. Waits for a free worker.
   */
  async classify(view: PartitionView): Promise<SetupRunOn[]> {
    const worker = await this.take();
    try {
      return await this.classifyOn(worker, view);
    } finally {
      this.give(worker);
    }
  }

  private take(): Promise<number> {
    const w = this.idle.pop();
    if (w !== undefined) return Promise.resolve(w);
    return new Promise((resolve) => this.waiting.push(resolve));
  }

  private give(worker: number): void {
    const next = this.waiting.shift();
    if (next) next(worker);
    else this.idle.push(worker);
  }

  private async classifyOn(worker: number, view: PartitionView): Promise<SetupRunOn[]> {
    const inside = `/tmp/classify-${worker}`;
    const partition = join(this.root, inside.slice(1), "partition");
    const fd = openSync(partition, "r+");
    let header: Uint8Array;
    try {
      writeAll(fd, view.header, 0);
      writeAll(fd, new Uint8Array(SECTOR).fill(view.dataIsZero ? 0 : 0xa5), HEADER_SECTORS * SECTOR);
      header = new Uint8Array(view.header);
    } finally {
      closeSync(fd);
    }
    const runs: SetupRunOn[] = [];
    for (let i = 0; i < MAX_RUNS; i++) {
      // The PATH without /usr/local: the power-cut copy's stand-ins there would only run the real
      // programs, which this runs directly.
      const r = await run(["chroot", this.root, "timeout", String(RUN_LIMIT_S), "/bin/bash", "/tmp/classify.sh", `${inside}/setup`, `${inside}/partition`, `${inside}/flag`], {
        env: { HOME: "/root", PATH: "/usr/sbin:/usr/bin:/sbin:/bin" },
      });
      this.runs++;
      const after = readHeader(partition);
      const changed = !Buffer.from(after).equals(Buffer.from(header));
      const setupRun = { ...parseSetupRun(r.stdout, r.stderr, `${inside}/partition`), changed };
      // classify.sh names what wrote the header area; it must agree with the comparison here.
      if (changed !== (setupRun.changedBy !== "nothing")) {
        throw new Error(`classify.sh says ${setupRun.changedBy} wrote the header area, but it ${changed ? "changed" : "is unchanged"}`);
      }
      runs.push(setupRun);
      if (!changed || setupRun.decision === "first-boot" || setupRun.decision === "start-again") break;
      header = after;
    }
    return runs;
  }
}

function writeAll(fd: number, bytes: Uint8Array, position: number): void {
  let done = 0;
  while (done < bytes.length) done += writeSync(fd, bytes, done, bytes.length - done, position + done);
}

/** The header area of a partition file. */
export function readHeader(file: string): Uint8Array {
  const out = new Uint8Array(HEADER_SECTORS * SECTOR);
  const fd = openSync(file, "r");
  try {
    let done = 0;
    while (done < out.length) {
      const n = readSync(fd, out, done, out.length - done, done);
      if (n === 0) throw new Error(`${file} ends inside its header area`);
      done += n;
    }
  } finally {
    closeSync(fd);
  }
  return out;
}

/** The sectors of a partition file's data area that are not all zeros (read in 4 MiB pieces). */
export function nonZeroDataSectors(file: string): number[] {
  const found: number[] = [];
  const chunk = new Uint8Array(4 * 1024 * 1024);
  const words = new Uint32Array(chunk.buffer);
  const perSector = SECTOR / 4;
  const fd = openSync(file, "r");
  try {
    for (let at = HEADER_SECTORS * SECTOR; at < DATA.sizeSectors * SECTOR; at += chunk.length) {
      const want = Math.min(chunk.length, DATA.sizeSectors * SECTOR - at);
      let done = 0;
      while (done < want) {
        const n = readSync(fd, chunk, done, want - done, at + done);
        if (n === 0) throw new Error(`${file} ends at byte ${at + done}, before the end of the partition`);
        done += n;
      }
      for (let s = 0; s < want / SECTOR; s++) {
        for (let k = s * perSector; k < (s + 1) * perSector; k++) {
          if (words[k] !== 0) {
            found.push(at / SECTOR + s);
            break;
          }
        }
      }
    }
  } finally {
    closeSync(fd);
  }
  return found;
}
