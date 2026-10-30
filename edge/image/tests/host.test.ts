// The host side: argument rules, paths that must stay outside the repository, the container
// command line, the pins, and the README listing what is pinned.
import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { REPO, UsageError, containerArgs, isInside, outsideRepo, parseArgs, rootlessProblem } from "../src/host.ts";
import { DEBIAN_SNAPSHOT, IMAGE_PACKAGES, KNOWN_FINDINGS, NTP_SERVERS, SOURCE, TOOL_IMAGE, TOOL_PACKAGES } from "../src/pins.ts";
import { LAN_PATTERNS, PRIVATE_KEY_PATTERN, textProblems } from "../src/tree.ts";

const dir = mkdtempSync(join(tmpdir(), "image-host-"));
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("arguments", () => {
  test("named values, required ones, no repeats or unknown names", () => {
    expect(parseArgs(["--a", "1", "--b", "2"], ["a", "b"], ["a"])).toEqual(new Map([["a", "1"], ["b", "2"]]));
    expect(() => parseArgs(["--b", "2"], ["a", "b"], ["a"])).toThrow("--a is required");
    expect(() => parseArgs(["--c", "1"], ["a"], [])).toThrow("unknown argument --c");
    expect(() => parseArgs(["--a", "1", "--a", "2"], ["a"], [])).toThrow("given twice");
    expect(() => parseArgs(["--a"], ["a"], [])).toThrow("needs a value");
    expect(() => parseArgs(["--a", "--b"], ["a", "b"], [])).toThrow("needs a value");
  });
});

describe("paths outside the repository", () => {
  test("a key or output inside the checkout is refused", () => {
    expect(() => outsideRepo(join(REPO, "edge/image/admin.pub"), "--ssh-key")).toThrow(UsageError);
    expect(() => outsideRepo(join(REPO, "target/out"), "--out")).toThrow("outside the repository");
    expect(outsideRepo(join(dir, "admin.pub"), "--ssh-key")).toBe(join(dir, "admin.pub"));
  });

  test("a symbolic link into the checkout counts as inside", () => {
    const link = join(dir, "repo-link");
    symlinkSync(REPO, link);
    expect(isInside(join(link, "edge/image/key.pub"), REPO)).toBe(true);
    mkdirSync(join(dir, "real"));
    expect(isInside(join(dir, "real/not-yet/key.pub"), REPO)).toBe(false);
    expect(isInside(`${REPO}-sibling/key.pub`, REPO)).toBe(false);
  });
});

describe("rootless only", () => {
  test("refuses root and a rootful podman", () => {
    expect(rootlessProblem(1000, "true\n")).toBeUndefined();
    expect(rootlessProblem(0, "true")).toContain("refusing to run as root");
    expect(rootlessProblem(1000, "false\n")).toContain("not rootless");
    expect(rootlessProblem(1000, "")).toContain("not rootless");
  });
});

describe("tool container", () => {
  const args = containerArgs(
    [
      { host: "/cache/work", container: "/work", readOnly: false },
      { host: "/keys/admin.pub", container: "/in/ssh-key.pub", readOnly: true },
    ],
    "/src/image/src/inside/build.ts",
    { TRUSTIOT_IMAGE_MANIFEST: "1" },
  );

  test("is rootless podman with only the capabilities the mounts need", () => {
    expect(args.slice(0, 2)).toEqual(["podman", "run"]);
    expect(args).toContain("--cap-add=SYS_ADMIN");
    expect(args).toContain("--device=/dev/fuse");
    expect(args).toContain("--security-opt=no-new-privileges");
    expect(args.some((a) => a.startsWith("--privileged"))).toBe(false);
    expect(args.some((a) => /^--(network|net)=host/.test(a))).toBe(false);
    expect(args).toContain("--pull=never");
  });

  test("mounts inputs read-only and runs the pinned image", () => {
    expect(args).toContain("--volume=/keys/admin.pub:/in/ssh-key.pub:ro");
    expect(args).toContain("--volume=/cache/work:/work:rw");
    expect(args).toContain("--env=TRUSTIOT_IMAGE_MANIFEST=1");
    expect(args.slice(-3)).toEqual([TOOL_IMAGE, "/opt/bun/bun", "/src/image/src/inside/build.ts"]);
  });

  test("refuses a path podman would split", () => {
    expect(() => containerArgs([{ host: "/a:b", container: "/x", readOnly: true }], "/s.ts")).toThrow("cannot be mounted");
  });
});

describe("pins", () => {
  test("source image from the official site over HTTPS, with a SHA-256", () => {
    expect(SOURCE.url).toStartWith("https://downloads.raspberrypi.com/raspios_lite_arm64/images/");
    expect(SOURCE.url).toEndWith(`/${SOURCE.file}`);
    expect(SOURCE.sha256).toMatch(/^[0-9a-f]{64}$/);
  });

  test("tool image by digest, packages at exact versions from a snapshot", () => {
    expect(TOOL_IMAGE).toMatch(/^docker\.io\/library\/debian@sha256:[0-9a-f]{64}$/);
    expect(DEBIAN_SNAPSHOT).toMatch(/^\d{8}T\d{6}Z$/);
    for (const version of [...Object.values(IMAGE_PACKAGES), ...Object.values(TOOL_PACKAGES)]) expect(version).toMatch(/^[0-9][\w.+~:-]*$/);
  });

  test("README lists every pin", async () => {
    const readme = await Bun.file(join(REPO, "edge/image/README.md")).text();
    const digest = TOOL_IMAGE.split("@")[1] ?? "";
    for (const value of [SOURCE.url, SOURCE.sha256, digest, DEBIAN_SNAPSHOT, ...NTP_SERVERS]) expect(readme).toContain(value);
    for (const [name, version] of [...Object.entries(IMAGE_PACKAGES), ...Object.entries(TOOL_PACKAGES)]) expect(readme).toContain(`${name} ${version}`);
    // Every gitleaks finding the check accepts is named, with its package, where the checks are described.
    for (const entry of KNOWN_FINDINGS) for (const value of [entry.path, entry.package]) expect(readme).toContain(value);
  });
});

describe("content scan", () => {
  test("finds private key blocks and LAN names, not public addresses", () => {
    expect(PRIVATE_KEY_PATTERN.test("-----BEGIN OPENSSH PRIVATE KEY-----")).toBe(true);
    // The finding names the line and the pattern, never the address or name itself.
    expect(textProblems("/etc/x", "# servers\nserver 10.1.2.3", true)).toEqual(["/etc/x names a LAN address or host (line 2, LAN_PATTERNS[0])"]);
    expect(textProblems("/etc/x", "host gw.lan", true)).toEqual(["/etc/x names a LAN address or host (line 1, LAN_PATTERNS[6])"]);
    expect(textProblems("/etc/x", "server 10.1.2.3", true).join(" ")).not.toContain("10.1.2.3");
    expect(textProblems("/etc/x", "NTP=time.cloudflare.com 162.159.200.1 2606:4700:f1::1", true)).toEqual([]);
    expect(textProblems("/usr/share/doc/x", "example 192.168.1.1", false)).toEqual([]);
    for (const address of ["192.168.0.1", "172.16.0.1", "172.31.255.1", "169.254.1.1", "100.64.0.1", "fd12:3456::1"]) {
      expect(LAN_PATTERNS.some((p) => p.test(address))).toBe(true);
    }
    for (const address of ["172.15.0.1", "172.32.0.1", "100.63.0.1", "8.8.8.8"]) {
      expect(LAN_PATTERNS.some((p) => p.test(address))).toBe(false);
    }
  });
});
