// What the boot test adds to its copies of the image (README.md, section 「開機測試」), in one list:
// the boot test installs from it (src/inside/boot-test.ts), and the image check (src/inside/check.ts)
// refuses an image that carries any of it (Laysi/trustiot-v2#918). Every file in boot-test/ is
// either listed here as installed in the copies or as one that only runs in the tool container,
// so a new one fails tests/boot-test-files.test.ts until it is one or the other.
import { BOOT_TEST } from "./pins.ts";

export interface BootTestFile {
  /** The file in edge/image/boot-test/. */
  repo: string;
  /** Where the test's copies have it. */
  image: string;
  mode: number;
  /** The target whose .wants/ directory links the unit: the copies enable it. */
  wantedBy?: string;
  /** Only the power-cut copy has it (with a link in /usr/local/sbin for each program it stands in for). */
  powerCutOnly?: boolean;
}

/** Every name the test's copies add starts with this, so the image check also finds one this list forgets. */
export const BOOT_TEST_PREFIX = "trustiot-boot-test";

export const BOOT_TEST_FILES: readonly BootTestFile[] = [
  { repo: "probe.sh", image: "/usr/local/sbin/trustiot-boot-test-probe", mode: 0o755 },
  { repo: "trustiot-boot-test.service", image: "/etc/systemd/system/trustiot-boot-test.service", mode: 0o644, wantedBy: "multi-user.target" },
  { repo: "tpm-late.sh", image: "/usr/local/sbin/trustiot-boot-test-tpm-late", mode: 0o755 },
  {
    repo: "trustiot-boot-test-tpm-late.service",
    image: "/etc/systemd/system/trustiot-boot-test-tpm-late.service",
    mode: 0o644,
    wantedBy: "sysinit.target",
  },
  { repo: "power-cut.sh", image: "/usr/local/sbin/trustiot-boot-test-power-cut", mode: 0o755, powerCutOnly: true },
];

/** Files of boot-test/ that no copy carries: they run in the tool container, against a copy's files. */
export const BOOT_TEST_CONTAINER_FILES: readonly string[] = ["classify.sh"];

/** The modules of the boot test's kernel (Debian's, with tpm_tis), which the copies add to the root file system. */
export const BOOT_TEST_MODULES = `/usr/lib/modules/${BOOT_TEST.kernel}`;

/** The .wants/ link that enables a listed unit. */
export function wantsLink(file: BootTestFile): string | null {
  if (!file.wantedBy) return null;
  return `/etc/systemd/system/${file.wantedBy}.wants/${file.image.slice(file.image.lastIndexOf("/") + 1)}`;
}

/** Where the power-cut copy links power-cut.sh as `program` (it comes first in the PATH systemd gives a service). */
export function powerCutLink(program: string): string {
  return `/usr/local/sbin/${program}`;
}

const SYSTEM_PROGRAM_DIRS = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];
const LOCAL_PROGRAM_DIRS = ["/usr/local/sbin", "/usr/local/bin"];

/**
 * Why an image whose root file system holds `paths` (every path in it, as "/etc/fstab") carries
 * something of the boot test's, if it does: a file of BOOT_TEST_FILES or the link that enables it,
 * a link of the programs power-cut.sh stands in for (`powerCutPrograms`), the boot test kernel's
 * modules, any name that starts with BOOT_TEST_PREFIX, or a program in /usr/local/sbin or
 * /usr/local/bin with the name of one in /usr/sbin, /usr/bin, /sbin or /bin, which would run in
 * its place, as power-cut.sh's links do.
 */
export function bootTestArtifactProblems(paths: Iterable<string>, powerCutPrograms: Iterable<string>): string[] {
  const all = new Set(paths);
  const listed = new Map<string, string>();
  for (const file of BOOT_TEST_FILES) {
    listed.set(file.image, `boot-test/${file.repo}`);
    const link = wantsLink(file);
    if (link) listed.set(link, `the link that enables boot-test/${file.repo}`);
  }
  for (const program of powerCutPrograms) listed.set(powerCutLink(program), `power-cut.sh's link for ${program}`);
  const problems = new Set<string>();
  for (const path of all) {
    const what = listed.get(path);
    if (what) problems.add(`${path} is ${what}`);
    if (path === BOOT_TEST_MODULES || path.startsWith(`${BOOT_TEST_MODULES}/`)) problems.add(`${BOOT_TEST_MODULES} holds the boot test kernel's modules`);
    const slash = path.lastIndexOf("/");
    const [dir, name] = [path.slice(0, slash), path.slice(slash + 1)];
    if (name.startsWith(BOOT_TEST_PREFIX)) problems.add(`${path} has a boot test name`);
    if (LOCAL_PROGRAM_DIRS.includes(dir)) {
      const shadowed = SYSTEM_PROGRAM_DIRS.map((d) => `${d}/${name}`).filter((p) => all.has(p));
      if (shadowed.length > 0) problems.add(`${path} runs in place of ${shadowed.join(" and ")}`);
    }
  }
  return [...problems].sort();
}
