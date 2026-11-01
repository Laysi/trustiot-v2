// Laysi/trustiot-v2#944: builds the web image with the release flow on an arm64 Linux host and
// checks it, in one self-contained run (nothing is kept between runs).
//
//   bun docs/test-reports/data/2026-11-01-944-web-image-arm64/web-image.ts repro
//     builds the same commit twice (the second time from a fresh clone, with its own Cargo target
//     directory and umask 027), compares the SPA's dist/ and the web image digests, lists the
//     owner and mode of /srv/www in the image, starts it and requests deep links and every file
//   bun docs/test-reports/data/2026-11-01-944-web-image-arm64/web-image.ts measure [--runs 5]
//     builds once, then starts the image `runs` times and measures the startup time and the memory
//     at rest and after a burst of requests; also the sizes of the build output and the image
//
// Run it from the root of a checkout whose HEAD is the commit to build (the release scripts
// refuse anything else), with no uncommitted changes, on an arm64 Linux host with rootless
// podman and the tools that `proto install` installs. skopeo is taken from PATH; when it is
// missing and dnf exists, its package is downloaded and unpacked into the work directory, so
// nothing is installed on the host. Containers run from a store of this run's own (vfs, in the
// work directory), so the user's podman store is not touched. The output names no local path:
// the checkout is written <checkout>, the work directory <work> and the home directory ~.

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..", "..", "..", "..");
const mode = Bun.argv[2];
if (mode !== "repro" && mode !== "measure") {
  console.error("usage: web-image.ts repro | measure [--runs N]");
  process.exit(2);
}
const runsArg = Bun.argv.indexOf("--runs");
const RUNS = runsArg > 0 ? Number(Bun.argv[runsArg + 1]) : 5;
if (!Number.isSafeInteger(RUNS) || RUNS < 1) {
  console.error("--runs must be a positive integer");
  process.exit(2);
}
const TAG = "v0.0.0-rc.944";
const REGISTRY = "127.0.0.1:5068/laysi/trustiot-v2";
/** How long to wait for the container's readiness endpoint before giving up. */
const READY_BOUND_MS = 60_000;

mkdirSync(join(homedir(), ".cache"), { recursive: true });
const WORK = mkdtempSync(join(homedir(), ".cache", "trustiot-944-"));
const STORE = join(WORK, "store");
/** Containers started and not yet removed. */
const CONTAINERS = new Set<string>();

function scrub(text: string): string {
  let out = text.split(WORK).join("<work>").split(ROOT).join("<checkout>").split(homedir()).join("~");
  const host = hostname();
  if (host.length > 2) {
    out = out.split(host).join("<host>");
  }
  return out;
}

function section(name: string, body: string): void {
  console.log(`\n===== ${name} =====\n${scrub(body).trimEnd()}`);
}

interface Result {
  code: number;
  stdout: string;
  stderr: string;
}

function sh(cmd: string[], opts: { cwd?: string; env?: Record<string, string>; allowFail?: boolean } = {}): Result {
  const proc = Bun.spawnSync(cmd, {
    cwd: opts.cwd ?? ROOT,
    env: { ...process.env, ...(opts.env ?? {}) },
    stdout: "pipe",
    stderr: "pipe",
  });
  const result = { code: proc.exitCode ?? -1, stdout: proc.stdout.toString(), stderr: proc.stderr.toString() };
  if (result.code !== 0 && !opts.allowFail) {
    throw new Error(scrub(`${cmd.join(" ")} exited ${result.code}\n${result.stderr.slice(-4000)}\n${result.stdout.slice(-4000)}`));
  }
  return result;
}

function loadavg(): string {
  return readFileSync("/proc/loadavg", "utf8").split(" ").slice(0, 3).join(" ");
}

function sha256(data: Uint8Array | string): string {
  return createHash("sha256").update(data).digest("hex");
}

function hostInfo(): string {
  const mem = readFileSync("/proc/meminfo", "utf8").split("\n").find((line) => line.startsWith("MemTotal")) ?? "";
  const os = readFileSync("/etc/os-release", "utf8").split("\n").find((line) => line.startsWith("PRETTY_NAME")) ?? "";
  const cpus = sh(["nproc"]).stdout.trim();
  return [
    `arch: ${sh(["uname", "-m"]).stdout.trim()}`,
    `kernel: ${sh(["uname", "-r"]).stdout.trim()}`,
    os,
    `cpus: ${cpus}`,
    mem.replace(/\s+/g, " "),
    `loadavg (1/5/15 min): ${loadavg()}`,
    `podman: ${sh(["podman", "--version"]).stdout.trim()}`,
    `bun: ${Bun.version}`,
  ].join("\n");
}

/** skopeo from PATH, or unpacked from the distribution's package into the work directory. */
function ensureSkopeo(): string {
  if (Bun.which("skopeo") !== null) {
    return "skopeo from PATH";
  }
  if (Bun.which("dnf") === null) {
    throw new Error("skopeo is missing and there is no dnf to fetch it");
  }
  const dir = join(WORK, "skopeo");
  mkdirSync(dir);
  sh(["dnf", "download", "--destdir", dir, "skopeo"]);
  const rpm = readdirSync(dir).find((name) => name.endsWith(".rpm")) ?? "";
  sh(["sh", "-c", `rpm2cpio "$1" | cpio -idm --quiet`, "sh", rpm], { cwd: dir });
  process.env.PATH = `${join(dir, "usr", "bin")}:${process.env.PATH}`;
  return `${rpm} unpacked into the work directory (not installed)`;
}

function storageEnv(): Record<string, string> {
  if (!existsSync(STORE)) {
    mkdirSync(STORE);
    const graph = join(STORE, "graph");
    writeFileSync(
      join(STORE, "storage.conf"),
      ["[storage]", 'driver = "vfs"', `graphroot = "${graph}"`, `rootless_storage_path = "${graph}"`, `runroot = "${join(STORE, "run")}"`, ""].join("\n"),
    );
  }
  return { CONTAINERS_STORAGE_CONF: join(STORE, "storage.conf") };
}

function podman(args: string[], allowFail = false): Result {
  return sh(["podman", ...args], { env: storageEnv(), allowFail });
}

function plan(): string {
  const full = join(WORK, "plan-full.json");
  sh(["bun", "scripts/release/plan.ts", "--tag", TAG, "--dry-run", "--registry", REGISTRY, "--output", full]);
  const doc = JSON.parse(readFileSync(full, "utf8"));
  doc.images = doc.images.filter((image: { name: string }) => image.name === "web");
  if (doc.images.length !== 1) {
    throw new Error("the plan has no web image");
  }
  const path = join(WORK, "plan.json");
  writeFileSync(path, `${JSON.stringify(doc, null, 2)}\n`);
  return path;
}

interface Build {
  label: string;
  root: string;
  out: string;
  targetDir: string;
  seconds: number;
  loadBefore: string;
  loadAfter: string;
}

function build(label: string, root: string, planPath: string, env: Record<string, string>, umask?: string): Build {
  const out = join(WORK, `out-${label}`);
  const logFile = join(WORK, `build-${label}.log`);
  const loadBefore = loadavg();
  const started = performance.now();
  const cmd = ["bun", "scripts/release/build.ts", "--plan", planPath, "--out", out];
  const wrapped = umask === undefined ? cmd : ["sh", "-c", `umask ${umask} && exec "$@"`, "sh", ...cmd];
  const result = sh(wrapped, { cwd: root, env, allowFail: true });
  writeFileSync(logFile, result.stdout + result.stderr);
  const seconds = (performance.now() - started) / 1000;
  if (result.code !== 0) {
    section(`build ${label} FAILED (exit ${result.code}), last lines`, (result.stdout + result.stderr).split("\n").slice(-80).join("\n"));
    throw new Error(`build ${label} failed`);
  }
  const releaseLines = (result.stdout + result.stderr).split("\n").filter((line) => line.startsWith("[release:"));
  section(`build ${label}: release log lines`, releaseLines.join("\n"));
  const targetDir = env.CARGO_TARGET_DIR ?? process.env.CARGO_TARGET_DIR ?? join(root, "target");
  return { label, root, out, targetDir, seconds, loadBefore, loadAfter: loadavg() };
}

/** `<sha256>  <bytes>  <path>` for every file under dir, sorted by path. */
function listing(dir: string): { lines: string[]; files: { path: string; bytes: number; data: Uint8Array<ArrayBuffer> }[] } {
  const files: { path: string; bytes: number; data: Uint8Array<ArrayBuffer> }[] = [];
  const walk = (relative: string) => {
    for (const name of readdirSync(join(dir, relative)).sort()) {
      const path = relative === "" ? name : `${relative}/${name}`;
      const stats = statSync(join(dir, path));
      if (stats.isDirectory()) {
        walk(path);
      } else {
        const data = new Uint8Array(readFileSync(join(dir, path)));
        files.push({ path, bytes: stats.size, data });
      }
    }
  };
  walk("");
  return { lines: files.map((file) => `${sha256(file.data)}  ${file.bytes}  ${file.path}`), files };
}

interface Oci {
  dir: string;
  manifestDigest: string;
  manifest: { config: { digest: string }; layers: { digest: string; size: number; mediaType: string }[] };
  config: { config: { User?: string; Entrypoint?: string[]; Labels?: Record<string, string> }; rootfs: { diff_ids: string[] }; created?: string; history?: unknown[] };
}

function blob(dir: string, digest: string): string {
  return join(dir, "blobs", "sha256", digest.replace("sha256:", ""));
}

function openArchive(archive: string, name: string): Oci {
  const dir = join(WORK, `oci-${name}`);
  mkdirSync(dir);
  sh(["tar", "-xf", archive, "-C", dir]);
  const index = JSON.parse(readFileSync(join(dir, "index.json"), "utf8"));
  const manifestDigest: string = index.manifests[0].digest;
  const manifest = JSON.parse(readFileSync(blob(dir, manifestDigest), "utf8"));
  const config = JSON.parse(readFileSync(blob(dir, manifest.config.digest), "utf8"));
  return { dir, manifestDigest, manifest, config };
}

/** `tar -tv` of one layer with numeric owners and full times. */
function layerEntries(oci: Oci, digest: string): string[] {
  return sh(["tar", "--numeric-owner", "--full-time", "-tvzf", blob(oci.dir, digest)]).stdout.trim().split("\n");
}

function inspectImage(oci: Oci): { ok: boolean; text: string } {
  const lines: string[] = [];
  let ok = true;
  const check = (condition: boolean, what: string) => {
    lines.push(`${condition ? "PASS" : "FAIL"}  ${what}`);
    ok &&= condition;
  };
  lines.push(`manifest digest: ${oci.manifestDigest}`);
  lines.push(`config: User=${oci.config.config.User} Entrypoint=${JSON.stringify(oci.config.config.Entrypoint)} created=${oci.config.created}`);
  lines.push(`labels: ${JSON.stringify(oci.config.config.Labels)}`);
  lines.push("layers (compressed bytes, uncompressed bytes, entries):");
  const www: string[] = [];
  const bin: string[] = [];
  for (const layer of oci.manifest.layers) {
    const raw = Bun.gunzipSync(new Uint8Array(readFileSync(blob(oci.dir, layer.digest))));
    const entries = layerEntries(oci, layer.digest);
    lines.push(`  ${layer.digest}  ${layer.size}  ${raw.length}  ${entries.length}`);
    for (const entry of entries) {
      if (/ srv\/?(www\/?.*)?$/.test(entry)) {
        www.push(entry);
      }
      if (/ usr\/local\/bin\/web$/.test(entry)) {
        bin.push(entry);
      }
    }
  }
  lines.push("", "/srv/www in the image (tar -tv --numeric-owner --full-time):", ...www);
  lines.push("", "the binary:", ...bin);
  lines.push("");
  const parsed = www.map((entry) => {
    const [perm, owner, , , , ...rest] = entry.split(/\s+/);
    return { perm: perm ?? "", owner: owner ?? "", path: rest.join(" ") };
  });
  const files = parsed.filter((entry) => entry.perm.startsWith("-"));
  const dirs = parsed.filter((entry) => entry.perm.startsWith("d") && entry.path.startsWith("srv/www"));
  check(files.length > 0, `/srv/www holds ${files.length} files and ${dirs.length} directories`);
  check(files.every((entry) => entry.perm === "-rw-r--r--"), "every file under /srv/www is 0644");
  check(dirs.every((entry) => entry.perm === "drwxr-xr-x"), "every directory under /srv/www (itself included) is 0755");
  check(parsed.every((entry) => entry.owner === "0/0"), "every entry under /srv/www is owned by 0:0 (root)");
  check(parsed.every((entry) => !entry.perm.startsWith("l")), "no symbolic link under /srv/www");
  check(oci.config.config.User === "65532:65532", "the image runs as 65532:65532");
  check(bin.length === 1 && bin[0]?.startsWith("-r-xr-xr-x 0/0") === true, "the binary is 0555, owned by root");
  return { ok, text: lines.join("\n") };
}

function freePort(): number {
  const server = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const port = server.port;
  server.stop(true);
  return port;
}

interface Container {
  name: string;
  port: number;
  health: number;
  startedAtClient: number;
}

function startContainer(image: string, name: string): Container {
  const port = freePort();
  const health = freePort();
  const startedAtClient = Date.now();
  // Registered first, so cleanup() removes it even when podman run fails half way.
  CONTAINERS.add(name);
  podman([
    "run",
    "--detach",
    "--pull=never",
    "--name",
    name,
    "--read-only",
    "--cap-drop=all",
    "--security-opt=no-new-privileges",
    "--publish",
    `127.0.0.1:${port}:8080`,
    "--publish",
    `127.0.0.1:${health}:8081`,
    image,
  ]);
  return { name, port, health, startedAtClient };
}

async function waitReady(container: Container): Promise<number> {
  const deadline = Date.now() + READY_BOUND_MS;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${container.health}/readyz`);
      if (response.status === 200) {
        return Date.now();
      }
    } catch {
      // not listening yet
    }
    await Bun.sleep(5);
  }
  const logs = podman(["logs", container.name], true);
  throw new Error(`the web container's /readyz did not answer 200 within ${READY_BOUND_MS} ms\n${logs.stdout}${logs.stderr}`);
}

function stopContainer(container: Container): string {
  podman(["stop", "--time", "30", container.name], true);
  const exit = podman(["inspect", "--format", "{{.State.ExitCode}}", container.name], true).stdout.trim();
  const logs = podman(["logs", container.name], true);
  podman(["rm", "--force", "--volumes", container.name], true);
  CONTAINERS.delete(container.name);
  return `exit code ${exit}\n${logs.stdout}${logs.stderr}`;
}

const SECURITY_HEADERS = [
  "content-security-policy",
  "content-security-policy-report-only",
  "x-content-type-options",
  "referrer-policy",
  "permissions-policy",
  "cross-origin-opener-policy",
  "cross-origin-resource-policy",
  "x-frame-options",
  "strict-transport-security",
];

async function requests(container: Container, dist: ReturnType<typeof listing>): Promise<{ ok: boolean; text: string }> {
  const base = `http://127.0.0.1:${container.port}`;
  const lines: string[] = [];
  let ok = true;
  const index = dist.files.find((file) => file.path === "index.html");
  if (index === undefined) {
    throw new Error("dist has no index.html");
  }
  const indexSha = sha256(index.data);
  const row = async (method: string, path: string, expectStatus: number, expectSha?: string, expectType?: string) => {
    const response = await fetch(`${base}${path}`, { method, ...(method === "POST" ? { body: "{}", headers: { "content-type": "application/json" } } : {}) });
    const body = new Uint8Array(await response.arrayBuffer());
    const type = response.headers.get("content-type") ?? "";
    const security = SECURITY_HEADERS.every((name) => response.headers.has(name));
    const shaOk = expectSha === undefined || sha256(body) === expectSha;
    const typeOk = expectType === undefined || type === expectType;
    const pass = response.status === expectStatus && shaOk && typeOk && security;
    ok &&= pass;
    lines.push(
      `${pass ? "PASS" : "FAIL"}  ${method} ${path} -> ${response.status} ${type} cache-control=${response.headers.get("cache-control")} ` +
        `bytes=${body.length}${expectSha === undefined ? "" : shaOk ? " body=dist file" : " body DIFFERS from dist"} security-headers=${security ? 9 : "missing"}`,
    );
    return response;
  };
  const deep = await row("GET", "/devices/42?tab=data", 200, indexSha, "text/html; charset=utf-8");
  for (const path of ["/", "/settings/users", "/a/b/c/d"]) {
    await row("GET", path, 200, indexSha, "text/html; charset=utf-8");
  }
  for (const file of dist.files) {
    await row("GET", `/${file.path}`, 200, sha256(file.data));
  }
  await row("HEAD", "/devices/42", 200);
  await row("GET", "/static/missing.js", 404);
  await row("GET", "/static/..%2Findex.html", 404);
  await row("POST", "/", 405);
  lines.push("", "headers of the deep link GET /devices/42?tab=data:");
  for (const [name, value] of deep.headers) {
    if (name !== "date") {
      lines.push(`  ${name}: ${value}`);
    }
  }
  const referenced = [...new TextDecoder().decode(index.data).matchAll(/(?:src|href)="(\/[^"]+)"/g)].map((match) => match[1]);
  lines.push("", `index.html references: ${referenced.join(", ")}`);
  const missing = referenced.filter((path) => !dist.files.some((file) => `/${file.path}` === path));
  lines.push(missing.length === 0 ? "PASS  every path index.html references is a dist file (each requested above)" : `FAIL  index.html references missing files: ${missing.join(", ")}`);
  ok &&= missing.length === 0;
  return { ok, text: lines.join("\n") };
}

/** Milliseconds since the epoch of an RFC 3339 UTC time with any number of fraction digits. */
function isoMs(text: string): number {
  const match = /^(.*T\d\d:\d\d:\d\d)(?:\.(\d+))?Z$/.exec(text);
  if (match === null) {
    return Number.NaN;
  }
  return Date.parse(`${match[1]}Z`) + Number(`0.${match[2] ?? "0"}`) * 1000;
}

function procStatus(pid: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of readFileSync(`/proc/${pid}/status`, "utf8").split("\n")) {
    const [key, value] = line.split(":\t");
    if (key !== undefined && value !== undefined) {
      out[key] = value.trim().replace(/\s+/g, " ");
    }
  }
  return out;
}

function cgroupMemory(pid: string): string {
  const line = readFileSync(`/proc/${pid}/cgroup`, "utf8").trim().split("\n").find((entry) => entry.startsWith("0::")) ?? "";
  const dir = join("/sys/fs/cgroup", line.slice(3));
  const read = (name: string) => {
    try {
      return readFileSync(join(dir, name), "utf8").trim();
    } catch {
      return "n/a";
    }
  };
  const stat = read("memory.stat").split("\n").filter((entry) => /^(anon|file) /.test(entry)).join(", ");
  return `cgroup memory.current=${read("memory.current")} memory.peak=${read("memory.peak")} (${stat})`;
}

function memory(name: string): string {
  const pid = podman(["inspect", "--format", "{{.State.Pid}}", name]).stdout.trim();
  const status = procStatus(pid);
  return `VmRSS=${status.VmRSS} VmHWM=${status.VmHWM} RssAnon=${status.RssAnon} RssFile=${status.RssFile} Threads=${status.Threads}; ${cgroupMemory(pid)}`;
}

/** `total` requests from `concurrency` workers over deep links and every dist file. */
async function burst(container: Container, dist: ReturnType<typeof listing>, total: number, concurrency: number): Promise<{ failed: number; text: string }> {
  const paths = ["/devices/42", "/settings/users", ...dist.files.map((file) => `/${file.path}`)];
  let next = 0;
  let failed = 0;
  let bytes = 0;
  const started = performance.now();
  await Promise.all(
    Array.from({ length: concurrency }, async () => {
      while (next < total) {
        const path = paths[next++ % paths.length];
        const response = await fetch(`http://127.0.0.1:${container.port}${path}`);
        bytes += (await response.arrayBuffer()).byteLength;
        if (response.status !== 200) {
          failed++;
        }
      }
    }),
  );
  const seconds = (performance.now() - started) / 1000;
  return { failed, text: `${total} requests, ${concurrency} at a time, ${failed} not 200, ${bytes} bytes, ${seconds.toFixed(2)} s (client on the same host)` };
}

function sizes(dist: ReturnType<typeof listing>, oci: Oci, archive: string, binary: string): string {
  const lines = ["build output (frontend/dist): bytes, gzip -9 bytes, path"];
  let total = 0;
  let gz = 0;
  for (const file of dist.files) {
    const zipped = Bun.gzipSync(file.data, { level: 9 }).length;
    total += file.bytes;
    gz += zipped;
    lines.push(`  ${file.bytes}  ${zipped}  ${file.path}`);
  }
  lines.push(`  total: ${dist.files.length} files, ${total} bytes, ${gz} bytes with gzip -9`);
  lines.push(`web binary (aarch64 musl, static): ${statSync(binary).size} bytes`);
  lines.push(`image OCI archive: ${statSync(archive).size} bytes`);
  const compressed = oci.manifest.layers.reduce((sum, layer) => sum + layer.size, 0);
  lines.push(`image layers: ${oci.manifest.layers.length}, ${compressed} bytes compressed`);
  return lines.join("\n");
}

function loadImage(archive: string): string {
  const out = podman(["pull", "--quiet", `oci-archive:${archive}`]).stdout.trim().split("\n");
  return out[out.length - 1] ?? "";
}

function cleanup(): void {
  // Containers a failed check left running, before their store goes.
  for (const name of CONTAINERS) {
    podman(["rm", "--force", "--volumes", "--time", "5", name], true);
  }
  if (existsSync(STORE)) {
    podman(["unshare", "rm", "-rf", "--", STORE], true);
  }
  rmSync(WORK, { recursive: true, force: true });
}

async function repro(): Promise<boolean> {
  section("host", hostInfo());
  section("skopeo", `${ensureSkopeo()}\n${sh(["skopeo", "--version"]).stdout}`);
  const head = sh(["git", "rev-parse", "HEAD"]).stdout.trim();
  const tree = sh(["git", "rev-parse", "HEAD^{tree}"]).stdout.trim();
  const planPath = plan();
  const planDoc = JSON.parse(readFileSync(planPath, "utf8"));
  section("plan", `commit ${head}, tree ${tree}, sourceDateEpoch ${planDoc.sourceDateEpoch}, image ${planDoc.images[0].ref}`);

  const first = build("1", ROOT, planPath, {});
  const dist1 = listing(join(ROOT, "frontend", "dist"));
  const clone = join(WORK, "clone");
  sh(["git", "clone", "--quiet", "--no-hardlinks", "--no-checkout", ROOT, clone]);
  // The clone's default branch need not be HEAD: build exactly the commit of the first build.
  sh(["git", "-C", clone, "checkout", "--quiet", "--detach", head]);
  const second = build("2", clone, planPath, { CARGO_TARGET_DIR: join(WORK, "target-2") }, "027");
  const dist2 = listing(join(clone, "frontend", "dist"));
  section(
    "builds",
    [first, second]
      .map((b) => `build ${b.label}: ${b.root === ROOT ? "the checkout" : "a fresh clone in another directory, own Cargo target directory, umask 027"}, ${b.seconds.toFixed(0)} s, loadavg before ${b.loadBefore}, after ${b.loadAfter}`)
      .join("\n"),
  );

  const lines: string[] = [];
  let same = true;
  // `informational`: compared and reported, but not part of what must be the same (see below).
  const compare = (what: string, a: string, b: string, informational = false) => {
    lines.push(`${a === b ? "SAME" : "DIFFERENT"}${informational ? " (informational)" : ""}  ${what}\n  1: ${a}\n  2: ${b}`);
    if (!informational) {
      same &&= a === b;
    }
  };
  section("dist build 1 (sha256, bytes, path)", dist1.lines.join("\n"));
  compare("frontend/dist listing (sha256 of the listing)", sha256(dist1.lines.join("\n")), sha256(dist2.lines.join("\n")));
  const binary = (b: Build) => join(b.targetDir, "aarch64-unknown-linux-musl", "release", "web");
  compare("web binary sha256", sha256(new Uint8Array(readFileSync(binary(first)))), sha256(new Uint8Array(readFileSync(binary(second)))));
  const archive = (b: Build) => join(b.out, "images", "web.oci.tar");
  const built = (b: Build) => JSON.parse(readFileSync(join(b.out, "build.json"), "utf8"));
  compare("image digest (build.json)", built(first).images[0].digest, built(second).images[0].digest);
  const oci1 = openArchive(archive(first), "1");
  const oci2 = openArchive(archive(second), "2");
  compare("layer digests", oci1.manifest.layers.map((l) => l.digest).join(","), oci2.manifest.layers.map((l) => l.digest).join(","));
  compare("config digest", oci1.manifest.config.digest, oci2.manifest.config.digest);
  // What publish pushes (skopeo copy --preserve-digests): index.json, oci-layout and every blob.
  const contents1 = listing(oci1.dir).lines;
  const contents2 = listing(oci2.dir).lines;
  compare("OCI archive contents: index.json, oci-layout and blobs (sha256 of their listing)", sha256(contents1.join("\n")), sha256(contents2.join("\n")));
  // The archive file itself is a tar that podman writes around those files; publish never pushes
  // it as it is, so its own headers are reported only.
  compare("OCI archive file sha256", sha256(new Uint8Array(readFileSync(archive(first)))), sha256(new Uint8Array(readFileSync(archive(second)))), true);
  const outer = (b: Build) => sh(["tar", "--numeric-owner", "--full-time", "-tvf", archive(b)]).stdout.trim().split("\n");
  const outer1 = outer(first);
  const outer2 = outer(second);
  lines.push(
    "  entries of the archive file (tar -tv) that differ:",
    ...outer1.filter((entry) => !outer2.includes(entry)).map((entry) => `    1: ${entry}`),
    ...outer2.filter((entry) => !outer1.includes(entry)).map((entry) => `    2: ${entry}`),
  );
  // The SBOM is attested next to the image, not part of its digest.
  const sbom = (b: Build) => readFileSync(join(b.out, "images", "web.spdx.json"), "utf8");
  compare("SBOM (web.spdx.json) sha256", sha256(sbom(first)), sha256(sbom(second)), true);
  const sbomFields = (b: Build) => {
    const doc = JSON.parse(sbom(b));
    return `documentNamespace=${doc.documentNamespace} created=${doc.creationInfo?.created} packages=${doc.packages?.length}`;
  };
  lines.push(`  1: ${sbomFields(first)}`, `  2: ${sbomFields(second)}`);
  const sbomPackages = (b: Build) =>
    sha256(
      JSON.stringify(
        (JSON.parse(sbom(b)).packages as { name: string; versionInfo?: string }[]).map((pkg) => `${pkg.name}@${pkg.versionInfo}`).sort(),
      ),
    );
  compare("SBOM package list (name@version, sorted)", sbomPackages(first), sbomPackages(second), true);
  section("reproducibility", lines.join("\n"));
  if (!same) {
    const diff: string[] = [];
    if (dist1.lines.join("\n") !== dist2.lines.join("\n")) {
      diff.push("dist build 2:", ...dist2.lines);
    }
    diff.push("config 1:", JSON.stringify(oci1.config, null, 1), "config 2:", JSON.stringify(oci2.config, null, 1));
    oci1.manifest.layers.forEach((layer, i) => {
      const other = oci2.manifest.layers[i];
      if (other !== undefined && other.digest !== layer.digest) {
        const a = layerEntries(oci1, layer.digest);
        const b = layerEntries(oci2, other.digest);
        diff.push(`layer ${i} entries that differ:`, ...a.filter((e) => !b.includes(e)).map((e) => `  1: ${e}`), ...b.filter((e) => !a.includes(e)).map((e) => `  2: ${e}`));
      }
    });
    section("differences", diff.join("\n"));
  }

  const inspected = inspectImage(oci1);
  section("image contents (build 1)", inspected.text);

  const image = loadImage(archive(first));
  const container = startContainer(image, `web-944-${process.pid}`);
  const ready = await waitReady(container);
  const top = podman(["top", container.name, "user", "huser", "group", "args"]).stdout;
  const pid = podman(["inspect", "--format", "{{.State.Pid}}", container.name]).stdout.trim();
  const uid = procStatus(pid).Uid;
  const served = await requests(container, dist1);
  const stopped = stopContainer(container);
  section(
    "running the image (build 1)",
    [
      `podman run --read-only --cap-drop=all --security-opt=no-new-privileges, image ${image}`,
      `/readyz answered 200 ${ready - container.startedAtClient} ms after podman run was started`,
      `podman top (user is the container's, huser the host's subordinate ID):\n${top.trim()}`,
      `Uid line of /proc/<pid>/status (real, effective, saved, fs; host IDs): ${uid}`,
      "",
      served.text,
    ].join("\n"),
  );
  section("container log and exit after podman stop (SIGTERM)", stopped);
  podman(["rmi", "--force", image], true);
  return same && inspected.ok && served.ok;
}

async function measure(): Promise<boolean> {
  section("host", hostInfo());
  section("skopeo", `${ensureSkopeo()}\n${sh(["skopeo", "--version"]).stdout}`);
  const head = sh(["git", "rev-parse", "HEAD"]).stdout.trim();
  const planPath = plan();
  section("plan", `commit ${head}`);
  const built = build("m", ROOT, planPath, {});
  const dist = listing(join(ROOT, "frontend", "dist"));
  const archive = join(built.out, "images", "web.oci.tar");
  const oci = openArchive(archive, "m");
  section("sizes", sizes(dist, oci, archive, join(built.targetDir, "aarch64-unknown-linux-musl", "release", "web")));
  section("digest", JSON.parse(readFileSync(join(built.out, "build.json"), "utf8")).images[0].digest);

  const image = loadImage(archive);
  const rows: string[] = ["run | loadavg at start | podman run → /readyz 200 (ms) | container start → `loaded the site` (ms) | container start → `listening` (ms)"];
  const memoryLines: string[] = [];
  let ok = true;
  for (let run = 1; run <= RUNS; run++) {
    // Let the previous container's exit settle.
    await Bun.sleep(3000);
    const load = loadavg();
    const container = startContainer(image, `web-944-m${run}-${process.pid}`);
    const ready = await waitReady(container);
    const startedNs = BigInt(podman(["inspect", "--format", "{{.State.StartedAt.UnixNano}}", container.name]).stdout.trim());
    const startedMs = Number(startedNs / 1_000_000n);
    const logs = podman(["logs", container.name]).stdout.split("\n").filter((line) => line.startsWith("{"));
    const at = (message: string) => {
      const line = logs.find((entry) => entry.includes(`"message":"${message}"`));
      return line === undefined ? Number.NaN : isoMs(JSON.parse(line).timestamp);
    };
    const loaded = at("loaded the site");
    const listening = at("listening");
    rows.push(`${run} | ${load} | ${ready - container.startedAtClient} | ${loaded - startedMs} | ${listening - startedMs}`);
    // Both log lines must be there, in order.
    ok &&= Number.isFinite(loaded) && Number.isFinite(listening) && loaded <= listening;
    if (run === 1) {
      memoryLines.push(`first log lines:\n${logs.slice(0, 3).join("\n")}`);
    }
    await Bun.sleep(2000);
    memoryLines.push(`run ${run} at rest, 2 s after ready: ${memory(container.name)}`);
    const load2 = await burst(container, dist, 5000, 32);
    memoryLines.push(`run ${run} burst: ${load2.text}`);
    ok &&= load2.failed === 0;
    memoryLines.push(`run ${run} after the burst: ${memory(container.name)}`);
    const stopped = stopContainer(container);
    ok &&= stopped.startsWith("exit code 0");
    memoryLines.push(`run ${run} ${stopped.split("\n")[0]} after podman stop`);
  }
  section("startup", rows.join("\n"));
  section("memory", memoryLines.join("\n"));
  section("loadavg at the end", loadavg());
  podman(["rmi", "--force", image], true);
  return ok;
}

let passed = false;
try {
  passed = mode === "repro" ? await repro() : await measure();
} catch (error) {
  section("ERROR", error instanceof Error ? (error.stack ?? error.message) : String(error));
} finally {
  cleanup();
}
console.log(`\nweb-image.ts ${mode}: ${passed ? "PASS" : "FAIL"}`);
process.exit(passed ? 0 : 1);
