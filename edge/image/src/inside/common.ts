// Helpers for the steps that run inside the tool container (src/host.ts starts it). The
// container is rootless podman: "root" here is the invoking user mapped into a user namespace,
// so mounts, chroot and fuse2fs work without any privilege on the build host, and every mount
// disappears with the container.
import { createReadStream } from "node:fs";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { DEBIAN_SNAPSHOT, SECTOR, TOOL_PACKAGES } from "../pins.ts";

/** Fixed paths of the container (host.ts mounts them). */
export const PATHS = {
  gitleaks: "/opt/gitleaks/gitleaks",
  source: "/in/source.img.xz",
  agent: "/in/trustiot-agent",
  sshKey: "/in/ssh-key.pub",
  image: "/in/image.img",
  src: "/src/image",
  agentSystemd: "/src/agent-systemd",
  work: "/work",
} as const;

export function step(name: string): void {
  console.log(`\n== ${name}`);
}

export interface RunOptions {
  env?: Record<string, string>;
  input?: string | Uint8Array;
  /** Exit codes that count as success (default: 0 only). */
  okCodes?: number[];
  /** Show the output as it comes instead of returning it. */
  inherit?: boolean;
}

export interface RunResult {
  code: number;
  stdout: string;
  stderr: string;
}

const BASE_ENV = { PATH: "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", LANG: "C.UTF-8", TZ: "UTC" };

/** Runs a command (argument array, no shell) and fails on an unexpected exit code. */
export async function run(cmd: string[], options: RunOptions = {}): Promise<RunResult> {
  const child = Bun.spawn(cmd, {
    env: { ...BASE_ENV, ...options.env },
    stdin: options.input === undefined ? "ignore" : new Blob([options.input]),
    stdout: options.inherit ? "inherit" : "pipe",
    stderr: options.inherit ? "inherit" : "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    options.inherit ? "" : new Response(child.stdout as ReadableStream).text(),
    options.inherit ? "" : new Response(child.stderr as ReadableStream).text(),
    child.exited,
  ]);
  if (!(options.okCodes ?? [0]).includes(code)) {
    throw new Error(`${cmd.join(" ")} exited with ${code}${stderr.trim() ? `: ${stderr.trim().slice(-2000)}` : ""}`);
  }
  return { code, stdout, stderr };
}

export async function sha256File(path: string): Promise<string> {
  const hasher = new Bun.CryptoHasher("sha256");
  for await (const chunk of createReadStream(path, { highWaterMark: 4 * 1024 * 1024 })) hasher.update(chunk as Buffer);
  return hasher.digest("hex");
}

export function sha256Bytes(bytes: Uint8Array | string): string {
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

/**
 * Points the container's apt at the pinned snapshot and installs the tools at their pinned
 * versions. The Debian container image has no CA certificates, so ca-certificates comes first over
 * plain HTTP (apt still checks the archive signature and every file's hash, and the exact version
 * rules out an older index); everything else then comes over HTTPS.
 */
export async function installTools(): Promise<void> {
  step(`tool packages (snapshot.debian.org ${DEBIAN_SNAPSHOT})`);
  const env = { DEBIAN_FRONTEND: "noninteractive" };
  const apt = ["apt-get", "-o", "Acquire::Retries=5", "-q"];
  const pinned = (names: string[]) => names.map((name) => `${name}=${TOOL_PACKAGES[name as keyof typeof TOOL_PACKAGES]}`);
  const names = Object.keys(TOOL_PACKAGES);
  await rm("/etc/apt/sources.list", { force: true });
  await writeFile("/etc/apt/sources.list.d/debian.sources", snapshotSources("http"));
  await run([...apt, "update"], { env });
  await run([...apt, "install", "-y", "--no-install-recommends", ...pinned(["ca-certificates"])], { env });
  await writeFile("/etc/apt/sources.list.d/debian.sources", snapshotSources("https"));
  await run([...apt, "update"], { env });
  await run([...apt, "install", "-y", "--no-install-recommends", ...pinned(names.filter((n) => n !== "ca-certificates"))], { env });
  const versions = await run(["dpkg-query", "-W", ...names]);
  console.log(versions.stdout.trim());
}

/** deb822 sources for the pinned snapshot (apt verifies the archive signatures as usual). */
export function snapshotSources(scheme: "http" | "https" = "https"): string {
  const keyring = "/usr/share/keyrings/debian-archive-keyring.pgp";
  return (
    `Types: deb\nURIs: ${scheme}://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}\n` +
    `Suites: trixie trixie-updates\nComponents: main\nSigned-By: ${keyring}\nCheck-Valid-Until: no\n\n` +
    `Types: deb\nURIs: ${scheme}://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}\n` +
    `Suites: trixie-security\nComponents: main\nSigned-By: ${keyring}\nCheck-Valid-Until: no\n`
  );
}
/** Copies one partition out of a disk image into its own (sparse) file. */
export async function extractPartition(image: string, start: number, size: number, out: string): Promise<void> {
  await rm(out, { force: true });
  await run([
    "dd",
    `if=${image}`,
    `of=${out}`,
    "bs=4M",
    "iflag=skip_bytes,count_bytes",
    `skip=${start * SECTOR}`,
    `count=${size * SECTOR}`,
    "conv=sparse",
    "status=none",
  ]);
  const written = Bun.file(out).size;
  if (written !== size * SECTOR) throw new Error(`${out} has ${written} bytes, expected ${size * SECTOR}`);
}

/** Writes a partition file into a disk image at its start sector, leaving zero blocks sparse. */
export async function placePartition(image: string, file: string, start: number): Promise<void> {
  await run([
    "dd",
    `if=${file}`,
    `of=${image}`,
    "bs=4M",
    "oflag=seek_bytes",
    `seek=${start * SECTOR}`,
    "conv=notrunc,sparse",
    "status=none",
  ]);
}

async function mounted(dir: string): Promise<boolean> {
  const info = await readFile("/proc/self/mountinfo", "utf8");
  return info.split("\n").some((line) => line.split(" ")[4] === dir);
}

async function waitFor(what: string, condition: () => Promise<boolean>, limitMs: number): Promise<void> {
  const deadline = Date.now() + limitMs;
  while (!(await condition())) {
    if (Date.now() > deadline) throw new Error(`timed out after ${limitMs / 1000} s waiting for ${what}`);
    await Bun.sleep(200);
  }
}

/**
 * An ext4 image file mounted with fuse2fs (no loop device, no host privilege). unmount() waits
 * for fuse2fs to exit, which is when every change has reached the file.
 */
export class Ext4Mount {
  private constructor(
    readonly dir: string,
    private readonly child: ReturnType<typeof Bun.spawn>,
  ) {}

  static async open(file: string, dir: string, readOnly: boolean): Promise<Ext4Mount> {
    await mkdir(dir, { recursive: true });
    const options = readOnly ? "ro,fakeroot" : "fakeroot";
    const child = Bun.spawn(["fuse2fs", "-f", "-o", options, file, dir], {
      env: BASE_ENV,
      stdin: "ignore",
      stdout: "inherit",
      stderr: "inherit",
    });
    const mount = new Ext4Mount(dir, child);
    // fuse2fs on a loaded host can take a while to answer; it exits at once on a bad file.
    await waitFor(`fuse2fs to mount ${file}`, async () => {
      if (child.exitCode !== null) throw new Error(`fuse2fs exited with ${child.exitCode} before mounting ${file}`);
      return mounted(dir);
    }, 120_000);
    return mount;
  }

  async unmount(): Promise<void> {
    if (await mounted(this.dir)) {
      const plain = await run(["umount", this.dir], { okCodes: [0, 1, 32] });
      if (plain.code !== 0) await run(["umount", "-l", this.dir]);
    }
    // fuse2fs writes everything back before it exits; an exit code other than 0 means it failed.
    const code = await Promise.race([this.child.exited, Bun.sleep(600_000).then(() => "timeout" as const)]);
    if (code === "timeout") throw new Error(`fuse2fs did not exit within 600 s after unmounting ${this.dir}`);
    if (code !== 0) throw new Error(`fuse2fs exited with ${code} after unmounting ${this.dir}`);
  }
}

/** The mounts a chroot into the image needs, made and removed together. */
export class Chroot {
  private readonly mounts: string[] = [];

  constructor(readonly root: string) {}

  async enter(options: { network: boolean }): Promise<void> {
    const r = this.root;
    await this.mount(["mount", "--rbind", "/proc", join(r, "proc")], join(r, "proc"));
    await this.mount(["mount", "--rbind", "/dev", join(r, "dev")], join(r, "dev"));
    await this.mount(["mount", "-t", "tmpfs", "-o", "mode=1777", "tmpfs", join(r, "tmp")], join(r, "tmp"));
    await this.mount(["mount", "-t", "tmpfs", "-o", "mode=0755", "tmpfs", join(r, "run")], join(r, "run"));
    if (options.network) {
      // The image's own resolv.conf stays untouched: the container's is mounted over it.
      await this.mount(["mount", "--bind", "/etc/resolv.conf", join(r, "etc/resolv.conf")], join(r, "etc/resolv.conf"));
    }
  }

  private async mount(cmd: string[], target: string): Promise<void> {
    await run(cmd);
    this.mounts.push(target);
  }

  async leave(): Promise<void> {
    for (const target of this.mounts.reverse()) await run(["umount", "-l", target]);
    this.mounts.length = 0;
  }

  /** Runs a command inside the image with a clean environment. */
  run(cmd: string[], options: RunOptions = {}): Promise<RunResult> {
    return run(["chroot", this.root, ...cmd], {
      ...options,
      env: { DEBIAN_FRONTEND: "noninteractive", HOME: "/root", ...options.env },
    });
  }
}

/** apt-get inside the image against the snapshot, keeping lists and downloads in /tmp. */
export async function chrootApt(chroot: Chroot, args: string[], options: RunOptions = {}): Promise<RunResult> {
  const tmp = join(chroot.root, "tmp/apt");
  await mkdir(join(tmp, "parts"), { recursive: true });
  await mkdir(join(tmp, "lists/partial"), { recursive: true });
  await mkdir(join(tmp, "cache/archives/partial"), { recursive: true });
  await writeFile(join(tmp, "parts/snapshot.sources"), snapshotSources());
  return chroot.run(
    [
      "apt-get",
      "-q",
      "-o", "Dir::Etc::SourceList=/dev/null",
      "-o", "Dir::Etc::SourceParts=/tmp/apt/parts",
      "-o", "Dir::State::Lists=/tmp/apt/lists",
      "-o", "Dir::Cache=/tmp/apt/cache",
      "-o", "Acquire::Retries=5",
      // fuse2fs lets only the mounting user in, so apt's unprivileged download user cannot write.
      "-o", "APT::Sandbox::User=root",
      "-o", "Dpkg::Options::=--force-unsafe-io",
      ...args,
    ],
    options,
  );
}

export async function readText(path: string): Promise<string> {
  return readFile(path, "utf8");
}
