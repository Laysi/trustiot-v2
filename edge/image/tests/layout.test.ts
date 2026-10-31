// The partition table: numbers, structure, the sfdisk script, and what first boot does to it.
import { afterAll, describe, expect, test } from "bun:test";
import { closeSync, mkdtempSync, openSync, readSync, rmSync, truncateSync, writeFileSync, writeSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fstabWithData, parseFstab } from "../src/files.ts";
import {
  ALIGN_SECTORS,
  BOOT_CODE_BYTES,
  compareTables,
  imageSectors,
  keepsDiskIdentifier,
  layoutProblems,
  parseSfdiskJson,
  partuuid,
  renderSfdiskScript,
  sourceTable,
  targetTable,
} from "../src/layout.ts";
import { DATA, SECTOR } from "../src/pins.ts";

// GitHub sets CI=true: there a missing tool fails the test instead of skipping it.
const IN_CI = process.env.CI === "true";

function available(tool: string): boolean {
  const found = Bun.which(tool) !== null;
  if (!found && IN_CI) throw new Error(`${tool} is required in CI`);
  return found;
}

const dir = mkdtempSync(join(tmpdir(), "image-layout-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("target table", () => {
  const table = targetTable();
  const part = (n: number) => table.partitions.find((p) => p.number === n);

  test("keeps boot, puts data after it and moves root to the end", () => {
    expect(part(1)).toEqual({ number: 1, start: 16384, size: 1048576, type: "c" });
    expect(part(3)).toEqual({ number: 3, start: 1064960, size: DATA.sizeSectors, type: "83" });
    expect(part(2)).toEqual({ number: 2, start: 1064960 + DATA.sizeSectors, size: 4947968, type: "83" });
    expect(table.labelId).toBe(sourceTable().labelId);
    expect(imageSectors(table)).toBe(1064960 + DATA.sizeSectors + 4947968);
  });

  test("follows the structural rules", () => {
    expect(layoutProblems(table)).toEqual([]);
    for (const p of table.partitions) expect(p.start % ALIGN_SECTORS).toBe(0);
  });

  test("structural rules catch root not last, overlap and misalignment", () => {
    const rootFirst = {
      labelId: table.labelId,
      partitions: [
        { number: 1, start: 16384, size: 1048576, type: "c" },
        { number: 2, start: 1064960, size: 4947968, type: "83" },
        { number: 3, start: 6012928, size: 4194304, type: "83" },
      ],
    };
    expect(layoutProblems(rootFirst)).toContain("root must be entry 2 and last on the disk, so first boot can grow it");
    const overlapping = { labelId: table.labelId, partitions: [...table.partitions.map((p) => (p.number === 3 ? { ...p, size: p.size + 8192 } : p))] };
    expect(layoutProblems(overlapping).some((p) => p.includes("overlap"))).toBe(true);
    const misaligned = { labelId: table.labelId, partitions: table.partitions.map((p) => (p.number === 3 ? { ...p, start: p.start + 1 } : p)) };
    expect(layoutProblems(misaligned).some((p) => p.includes("4 MiB"))).toBe(true);
  });

  test("PARTUUIDs follow the disk identifier", () => {
    expect(partuuid("0x27b94958", 3)).toBe("27b94958-03");
    expect(partuuid("0x00ab", 2)).toBe("000000ab-02");
  });

  test("sfdisk script lists the entries in table order with explicit positions", () => {
    expect(renderSfdiskScript(table)).toBe(
      "label: dos\nlabel-id: 0x27b94958\nunit: sectors\nsector-size: 512\n\n" +
        "start=16384, size=1048576, type=c\n" +
        `start=${1064960 + DATA.sizeSectors}, size=4947968, type=83\n` +
        `start=1064960, size=${DATA.sizeSectors}, type=83\n`,
    );
  });

  test("compareTables names each difference", () => {
    const other = { labelId: "0x12345678", partitions: table.partitions.filter((p) => p.number !== 3) };
    expect(compareTables(other, table)).toEqual(["disk identifier 0x12345678, expected 0x27b94958", "partition 3 is missing"]);
    expect(compareTables(table, table)).toEqual([]);
  });
});

describe("sfdisk", () => {
  test("parses sfdisk --json", () => {
    const json = JSON.stringify({
      partitiontable: {
        label: "dos",
        id: "0x27b94958",
        device: "/w/x.img",
        unit: "sectors",
        sectorsize: 512,
        partitions: [
          { node: "/w/x.img1", start: 16384, size: 1048576, type: "c" },
          { node: "/w/x.img2", start: 5259264, size: 4947968, type: "83" },
        ],
      },
    });
    expect(parseSfdiskJson(json)).toEqual({
      labelId: "0x27b94958",
      partitions: [
        { number: 1, start: 16384, size: 1048576, type: "c" },
        { number: 2, start: 5259264, size: 4947968, type: "83" },
      ],
    });
    expect(() => parseSfdiskJson(JSON.stringify({ partitiontable: { label: "gpt" } }))).toThrow("not an MBR");
  });

  test.skipIf(!available("sfdisk"))("writes the table to an image file and reads it back unchanged", async () => {
    const table = targetTable();
    const image = join(dir, "table.img");
    writeFileSync(image, "");
    truncateSync(image, imageSectors(table) * SECTOR);
    const write = Bun.spawnSync(["sfdisk", "--no-reread", "--no-tell-kernel", "--wipe", "never", "--quiet", image], {
      stdin: new Blob([renderSfdiskScript(table)]),
    });
    expect(write.exitCode).toBe(0);
    const read = Bun.spawnSync(["sfdisk", "--json", image]);
    expect(compareTables(parseSfdiskJson(read.stdout.toString()), table)).toEqual([]);
  });

  // The source image's initramfs runs `parted -m <disk> u s resizepart 2 <last sector>` on first
  // boot. parted is not in every CI image, and the image check runs the image's own parted.
  const cardSectors = (16 * 1024 * 1024 * 1024) / SECTOR;
  function firstBoot(name: string, bootCode: Uint8Array | null) {
    const table = targetTable();
    const card = join(dir, name);
    writeFileSync(card, "");
    truncateSync(card, cardSectors * SECTOR);
    expect(Bun.spawnSync(["sfdisk", "--no-reread", "--no-tell-kernel", "--quiet", card], { stdin: new Blob([renderSfdiskScript(table)]) }).exitCode).toBe(0);
    if (bootCode) {
      const fd = openSync(card, "r+");
      writeSync(fd, bootCode, 0, bootCode.length, 0);
      closeSync(fd);
    }
    const mbr = new Uint8Array(SECTOR);
    const read = openSync(card, "r");
    readSync(read, mbr, 0, SECTOR, 0);
    closeSync(read);
    expect(Bun.spawnSync(["parted", "-m", card, "u", "s", "resizepart", "2", String(cardSectors - 1)]).exitCode).toBe(0);
    const grown = parseSfdiskJson(Bun.spawnSync(["sfdisk", "--json", card]).stdout.toString());
    const expected = { labelId: table.labelId, partitions: table.partitions.map((p) => (p.number === 2 ? { ...p, size: cardSectors - p.start } : p)) };
    return { mbr, problems: compareTables(grown, expected) };
  }
  // With CI=true a missing parted or sfdisk fails here instead of skipping both tests below.
  const haveParted = available("parted") && available("sfdisk");

  // The second test below records what the SOURCE IMAGE's parted does without boot code: it
  // replaces the disk identifier (libparted's msdos_write, parted 3.6; see keepsDiskIdentifier).
  // The test runs the host's parted, so it only documents that behaviour when the host's parted is
  // the same series as the image's. parted 3.7.13 keeps the identifier in this case, so on a newer
  // host the test would fail without anything being wrong with the build, which always copies the
  // boot code, and the image check runs the image's own parted. Decision: run it only when the
  // host's major.minor equals SOURCE_PARTED_SERIES (the granularity at which the behaviour is
  // known to differ) and otherwise skip it with a message naming both versions. Running the
  // image's parted under arm64 emulation would need the source image and a chroot: too heavy for
  // a unit test. The first test (boot code present, identifier kept) holds on 3.6 and 3.7.13, so
  // it runs on every host with parted. CI's runner has parted 3.6.x (Ubuntu 24.04), so both run
  // there; with CI=true a parted that is not the 3.6 series fails the test instead of skipping.
  const SOURCE_PARTED_SERIES = "3.6";
  const hostParted = (() => {
    if (!haveParted) return null;
    const out = Bun.spawnSync(["parted", "--version"]).stdout.toString();
    return /\(GNU parted\)\s+(\d+\.\d+(?:\.\d+)?)/.exec(out)?.[1] ?? null;
  })();
  const partedSeries = (version: string | null) => (version === null ? null : version.split(".").slice(0, 2).join("."));
  const sameSeries = partedSeries(hostParted) === SOURCE_PARTED_SERIES;
  if (haveParted && !sameSeries) {
    const message = `host parted ${hostParted ?? "(version not recognised)"}, the source image's is ${SOURCE_PARTED_SERIES}.x`;
    if (IN_CI) throw new Error(`CI must run the boot-code test with parted ${SOURCE_PARTED_SERIES}.x: ${message}`);
    console.warn(`skipping the no-boot-code parted test: ${message}`);
  }

  test.skipIf(!haveParted)("first boot's resizepart grows root and keeps boot, data and the disk identifier", () => {
    // Any non-zero first byte stands in for the source image's boot code (copied by the build).
    const { mbr, problems } = firstBoot("card.img", new Uint8Array(BOOT_CODE_BYTES).fill(0xfa, 0, 1));
    expect(keepsDiskIdentifier(mbr)).toBe(true);
    expect(problems).toEqual([]);
  });

  test.skipIf(!haveParted || !sameSeries)("without boot code parted replaces the disk identifier (why the build copies it)", () => {
    const { mbr, problems } = firstBoot("bare.img", null);
    expect(keepsDiskIdentifier(mbr)).toBe(false);
    expect(problems).toHaveLength(1);
    expect(problems[0]).toStartWith("disk identifier ");
  });
});

describe("first boot's new disk identifier", () => {
  // set_partuuid in the source image's initramfs: sed -i "s/$OLD_DISKID/$DISKID/g" /etc/fstab
  test("rewrites boot and root; the data partition's line names the unlocked volume", () => {
    const sourceFstab =
      "proc            /proc           proc    defaults          0       0\n" +
      "PARTUUID=27b94958-01  /boot/firmware  vfat    defaults          0       2\n" +
      "PARTUUID=27b94958-02  /               ext4    defaults,noatime  0       1\n";
    const built = fstabWithData(sourceFstab);
    const afterFirstBoot = built.replaceAll("27b94958", "5c0ffee1");
    const entries = parseFstab(afterFirstBoot);
    // /etc/crypttab finds the volume by its LUKS UUID, which the disk identifier does not change.
    expect(entries.find((e) => e.file === DATA.mountPoint)?.spec).toBe("/dev/mapper/trustiot_data");
    expect(entries.find((e) => e.file === "/")?.spec).toBe("PARTUUID=5c0ffee1-02");
    expect(afterFirstBoot).not.toContain("27b94958");
  });
});
