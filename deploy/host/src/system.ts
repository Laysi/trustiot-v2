// The installer's only door to the host. Steps read and change the host through this interface,
// so tests can run every step against a simulated host (tests/fake-host.ts).
//
// Commands are always an argument array (no shell). Secrets never travel as arguments: the
// tunnel token is read from stdin or a file and written to a root-only file.
import { randomBytes } from "node:crypto";
import { chmod, chown, lstat, mkdir, open, readFile, readdir, rename, rm } from "node:fs/promises";
import { arch } from "node:os";
import { basename, dirname, join } from "node:path";

export interface RunResult {
  code: number;
  stdout: string;
  stderr: string;
}

export interface RunOptions {
  /** Extra environment variables, added to the installer's own environment. */
  env?: Record<string, string>;
  /** Show the command's output on the console as it runs instead of capturing it. */
  inheritOutput?: boolean;
  /** Kill the command after this many milliseconds. */
  timeoutMs?: number;
}

export interface FileInfo {
  /** Permission bits only (for example 0o600). */
  mode: number;
  uid: number;
  gid: number;
  /** The path itself, not what a symbolic link points to. */
  type: "file" | "directory" | "symlink" | "other";
}

export interface System {
  run(cmd: string[], options?: RunOptions): Promise<RunResult>;
  /** File contents, or null when the file does not exist. */
  readFile(path: string): Promise<Uint8Array | null>;
  /** Names of the entries in a directory, or none when it does not exist. */
  listDirectory(path: string): Promise<string[]>;
  /**
   * Replaces a file atomically with root ownership and the given mode: the data goes to a
   * temporary file in the same directory, which is renamed over the target.
   */
  writeFile(path: string, data: Uint8Array | string, mode: number): Promise<void>;
  makeDirectory(path: string, mode: number): Promise<void>;
  /** Makes root:root the owner and sets the mode. */
  restrict(path: string, mode: number): Promise<void>;
  remove(path: string): Promise<void>;
  /** Like lstat(2): a symbolic link is reported as such, not followed. */
  stat(path: string): Promise<FileInfo | null>;
  /** Downloads a URL over HTTPS and returns the body; throws on any non-200 answer. */
  download(url: string): Promise<Uint8Array>;
  /** A new directory only root can enter, for downloads. */
  makeTempDirectory(): Promise<string>;
  uid(): number;
  /** CPU architecture as Node names it: x64 or arm64. */
  arch(): string;
  sleep(ms: number): Promise<void>;
  /** Current time, for backup names. */
  now(): Date;
}

export class HostSystem implements System {
  async run(cmd: string[], options: RunOptions = {}): Promise<RunResult> {
    let child: ReturnType<typeof Bun.spawn>;
    try {
      child = Bun.spawn(cmd, {
        env: { ...process.env, ...options.env },
        stdin: "ignore",
        stdout: options.inheritOutput ? "inherit" : "pipe",
        stderr: options.inheritOutput ? "inherit" : "pipe",
        timeout: options.timeoutMs,
      });
    } catch (error) {
      // A program that is not installed (for example firewall-cmd before firewalld) answers
      // like a shell would: exit 127.
      if (isNotFound(error)) return { code: 127, stdout: "", stderr: `${cmd[0]}: command not found` };
      throw error;
    }
    const [stdout, stderr, code] = await Promise.all([
      child.stdout instanceof ReadableStream ? new Response(child.stdout).text() : "",
      child.stderr instanceof ReadableStream ? new Response(child.stderr).text() : "",
      child.exited,
    ]);
    return { code, stdout, stderr };
  }

  async readFile(path: string): Promise<Uint8Array | null> {
    try {
      return new Uint8Array(await readFile(path));
    } catch (error) {
      if (isNotFound(error)) return null;
      throw error;
    }
  }

  async listDirectory(path: string): Promise<string[]> {
    try {
      return (await readdir(path)).sort();
    } catch (error) {
      if (isNotFound(error)) return [];
      throw error;
    }
  }

  async writeFile(path: string, data: Uint8Array | string, mode: number): Promise<void> {
    const temp = join(dirname(path), `.${basename(path)}.${randomBytes(6).toString("hex")}.tmp`);
    // O_EXCL with the final mode: the file is never readable by anyone else, not even briefly.
    const handle = await open(temp, "wx", mode);
    try {
      await handle.writeFile(data);
      await handle.sync();
    } finally {
      await handle.close();
    }
    try {
      await chown(temp, 0, 0);
      await chmod(temp, mode); // open() applies the umask
      await rename(temp, path);
    } catch (error) {
      await rm(temp, { force: true });
      throw error;
    }
  }

  async makeDirectory(path: string, mode: number): Promise<void> {
    await mkdir(path, { mode, recursive: true });
    await chmod(path, mode);
  }

  async restrict(path: string, mode: number): Promise<void> {
    await chown(path, 0, 0);
    await chmod(path, mode);
  }

  async remove(path: string): Promise<void> {
    await rm(path, { force: true, recursive: true });
  }

  async stat(path: string): Promise<FileInfo | null> {
    try {
      const info = await lstat(path);
      const type = info.isSymbolicLink() ? "symlink" : info.isDirectory() ? "directory" : info.isFile() ? "file" : "other";
      return { mode: info.mode & 0o7777, uid: info.uid, gid: info.gid, type };
    } catch (error) {
      if (isNotFound(error)) return null;
      throw error;
    }
  }

  async download(url: string): Promise<Uint8Array> {
    if (!url.startsWith("https://")) throw new Error(`refusing to download over plain HTTP: ${url}`);
    const response = await fetch(url, { redirect: "follow", signal: AbortSignal.timeout(120_000) });
    if (response.status !== 200) throw new Error(`download of ${url} failed: HTTP ${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  }

  async makeTempDirectory(): Promise<string> {
    const path = join("/var/tmp", `trustiot-host-install.${randomBytes(6).toString("hex")}`);
    await mkdir(path, { mode: 0o700 });
    return path;
  }

  uid(): number {
    return process.getuid?.() ?? -1;
  }

  arch(): string {
    return arch();
  }

  sleep(ms: number): Promise<void> {
    return Bun.sleep(ms);
  }

  now(): Date {
    return new Date();
  }
}

function isNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}
