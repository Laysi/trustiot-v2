// What the boot test adds to its copies of the image, and the image check that the image itself
// carries none of it (src/boot-test-files.ts; Laysi/trustiot-v2#918).
import { describe, expect, test } from "bun:test";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { powerCutPrograms } from "../src/boot-results.ts";
import {
  BOOT_TEST_CONTAINER_FILES,
  BOOT_TEST_FILES,
  BOOT_TEST_MODULES,
  BOOT_TEST_PREFIX,
  bootTestArtifactProblems,
  powerCutLink,
  wantsLink,
} from "../src/boot-test-files.ts";
import { REPO } from "../src/host.ts";

const DIR = join(REPO, "edge/image/boot-test");
const PROGRAMS = [...powerCutPrograms(readFileSync(join(DIR, "power-cut.sh"), "utf8")).keys()];

/** A root file system like the image's: system programs, units, the image's own files. */
const CLEAN = [
  "/usr/bin/dd",
  "/usr/sbin/cryptsetup",
  "/usr/sbin/mke2fs",
  "/usr/bin/systemd-cryptenroll",
  "/usr/local/sbin",
  "/usr/local/bin",
  "/usr/local/bin/some-local-tool",
  "/usr/lib/modules/6.12.47+rpt-rpi-v8/modules.dep",
  "/usr/libexec/trustiot/trustiot-data-setup",
  "/usr/lib/systemd/system/trustiot-data-setup.service",
  "/etc/systemd/system/multi-user.target.wants/trustiot-agent.service",
  "/etc/systemd/system/multi-user.target.wants/ssh.service",
];

describe("the boot test's files", () => {
  test("every file in boot-test/ is installed in the copies or runs only in the tool container, and every one listed exists", () => {
    const listed = [...BOOT_TEST_FILES.map((f) => f.repo), ...BOOT_TEST_CONTAINER_FILES].sort();
    expect(readdirSync(DIR).sort()).toEqual(listed);
  });

  test("every name the copies add starts with the boot test's prefix", () => {
    for (const file of BOOT_TEST_FILES) {
      expect(file.image.slice(file.image.lastIndexOf("/") + 1).startsWith(BOOT_TEST_PREFIX)).toBe(true);
    }
  });

  test("a unit enabled in the copies is wanted by the target its [Install] section names", () => {
    for (const file of BOOT_TEST_FILES.filter((f) => f.repo.endsWith(".service"))) {
      const text = readFileSync(join(DIR, file.repo), "utf8");
      expect({ unit: file.repo, wantedBy: /^WantedBy=(\S+)$/m.exec(text)?.[1] }).toEqual({ unit: file.repo, wantedBy: file.wantedBy });
    }
  });

  test("power-cut.sh stands in for the programs whose steps it cuts", () => {
    expect(PROGRAMS.sort()).toEqual(["cryptsetup", "dd", "mke2fs", "systemd-cryptenroll"]);
  });
});

describe("the image check: no boot test shim, probe or unit in the image", () => {
  test("an image without them passes", () => {
    expect(bootTestArtifactProblems(CLEAN, PROGRAMS)).toEqual([]);
  });

  test("each file the copies add is found, and the link that enables it", () => {
    for (const file of BOOT_TEST_FILES) {
      expect(bootTestArtifactProblems([...CLEAN, file.image], PROGRAMS).some((p) => p.startsWith(`${file.image} is boot-test/${file.repo}`))).toBe(true);
      const link = wantsLink(file);
      if (link) expect(bootTestArtifactProblems([...CLEAN, link], PROGRAMS).some((p) => p.startsWith(`${link} is the link that enables`))).toBe(true);
    }
  });

  test("power-cut.sh's links are found, also without the shim they point at", () => {
    for (const program of PROGRAMS) {
      expect(bootTestArtifactProblems([...CLEAN, powerCutLink(program)], PROGRAMS)).toContain(`${powerCutLink(program)} is power-cut.sh's link for ${program}`);
    }
  });

  test("the boot test kernel's modules are found", () => {
    expect(bootTestArtifactProblems([...CLEAN, BOOT_TEST_MODULES, `${BOOT_TEST_MODULES}/updates/tpm_tis.ko`], PROGRAMS)).toEqual([
      `${BOOT_TEST_MODULES} holds the boot test kernel's modules`,
    ]);
  });

  test("a boot test file the list does not know is found by its name", () => {
    for (const path of ["/usr/local/sbin/trustiot-boot-test-new-probe", "/etc/systemd/system/trustiot-boot-test-new.service", "/usr/lib/systemd/system/trustiot-boot-test-other.timer"]) {
      expect(bootTestArtifactProblems([...CLEAN, path], PROGRAMS)).toEqual([`${path} has a boot test name`]);
    }
  });

  test("a program in /usr/local that runs in place of a system program is found, whatever its name", () => {
    expect(bootTestArtifactProblems([...CLEAN, "/usr/local/bin/dd"], [])).toEqual(["/usr/local/bin/dd runs in place of /usr/bin/dd"]);
    expect(bootTestArtifactProblems([...CLEAN, "/usr/local/sbin/mke2fs"], [])).toEqual(["/usr/local/sbin/mke2fs runs in place of /usr/sbin/mke2fs"]);
    // Not a stand-in: no system program has its name, or it is not in a program directory.
    expect(bootTestArtifactProblems([...CLEAN, "/usr/local/sbin/trustiot-tool", "/usr/local/share/dd"], [])).toEqual([]);
  });
});
