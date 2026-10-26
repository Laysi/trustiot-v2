/**
 * The static sites of release stage 2 (build.ts): a Rust image whose image.toml has a `[static]`
 * table also holds that Bun project's build output, which its service serves. The web image
 * holds the SPA (frontend/) at /srv/www for services/web (Laysi/trustiot-v2#123).
 *
 * The project is installed from its bun.lock without lifecycle scripts, built with
 * `bun run build` (which writes `dist/`) and checked with its own `bun run check:dist`. Then
 * staticSiteProblems checks dist/ against the rules services/web applies when it loads the files
 * at startup (services/web/src/site.rs), so no image is published whose service would refuse its
 * own files. pack.ts copies dist/ into the build context with copyStaticSite, which fixes the
 * modes and times so that the image does not depend on the umask or on when the build ran.
 *
 * The build job runs third-party build code here too (the project's build tools), with no
 * secrets and a read-only token, like the Cargo build.
 */

import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, rmSync, utimesSync } from "node:fs";
import { join } from "node:path";
import { log, run } from "./exec.ts";
import { type ReleasePlan, ReleaseError } from "./lib.ts";

/** The directory of the project that `bun run build` writes. */
export const DIST_DIR = "dist";

/**
 * The file extensions services/web serves (`CONTENT_TYPES` in services/web/src/site.rs); a file
 * with any other extension, or none, stops the service at startup.
 */
export const SITE_EXTENSIONS: ReadonlySet<string> = new Set([
  "css",
  "html",
  "ico",
  "js",
  "png",
  "svg",
  "txt",
  "webmanifest",
  "woff2",
]);

/** The most bytes services/web loads (`MAX_SITE_BYTES` in services/web/src/site.rs). */
export const MAX_SITE_BYTES = 32 * 1024 * 1024;

/** A name services/web serves as it is (`plain_name`): ASCII letters, digits, `_`, `-`, `.`, not first. */
const PLAIN_NAME = /^[A-Za-z0-9_-][A-Za-z0-9_.-]*$/;

/** The modes of the build output in the image: readable by every user, writable by none but root. */
const FILE_MODE = 0o644;
const DIR_MODE = 0o755;

/**
 * The commands that build `project` (relative to the repository root, where they run) with
 * `bun`: install exactly the locked packages without running their scripts or the project's,
 * build, and run the project's own check of the output.
 */
export function staticBuildCommands(bun: string, project: string): string[][] {
  return [
    [bun, "install", "--cwd", project, "--frozen-lockfile", "--ignore-scripts"],
    [bun, "run", "--cwd", project, "build"],
    [bun, "run", "--cwd", project, "check:dist"],
  ];
}

/**
 * The dotenv files at the root of a project directory. Vite, like other build tools, reads
 * `.env`, `.env.local`, `.env.<mode>` and `.env.<mode>.local` there and builds their values into
 * the site, so a release build refuses to run with any of them: a developer's local file must
 * never end up in a published image.
 */
export function dotenvFiles(dir: string): string[] {
  return readdirSync(dir)
    .filter((name) => /^\.env(?:\..+)?$/.test(name))
    .sort();
}

function extension(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1);
}

/**
 * What services/web would refuse in the build output `dist` when loading it (services/web,
 * `Site::load`): no index.html at its root, a symbolic link or another special file, a name
 * that is not plain, an extension it does not serve, or more than MAX_SITE_BYTES in all. Empty
 * when the service can serve it.
 */
export function staticSiteProblems(dist: string): string[] {
  if (!existsSync(dist) || !lstatSync(dist).isDirectory()) {
    return [`${DIST_DIR}/ does not exist; the project's build script must write it`];
  }
  const problems: string[] = [];
  let bytes = 0;
  let index = false;
  const walk = (relative: string) => {
    for (const name of readdirSync(join(dist, relative)).sort()) {
      const path = relative === "" ? name : `${relative}/${name}`;
      if (!PLAIN_NAME.test(name)) {
        problems.push(`${path}: a name other than letters, digits, \`_\`, \`-\` and \`.\` (not first)`);
        continue;
      }
      const stats = lstatSync(join(dist, path));
      if (stats.isSymbolicLink()) {
        problems.push(`${path}: a symbolic link`);
      } else if (stats.isDirectory()) {
        walk(path);
      } else if (!stats.isFile()) {
        problems.push(`${path}: neither a regular file nor a directory`);
      } else {
        if (!SITE_EXTENSIONS.has(extension(name))) {
          problems.push(`${path}: an extension the web service does not serve`);
        }
        index ||= path === "index.html";
        bytes += stats.size;
      }
    }
  };
  walk("");
  if (!index) {
    problems.push("there is no index.html file at the root");
  }
  if (bytes > MAX_SITE_BYTES) {
    problems.push(`the files hold ${bytes} bytes, more than the ${MAX_SITE_BYTES} the web service loads`);
  }
  return problems;
}

/**
 * Copies the build output `dist` to `dest` (which must not exist) for the image: regular files
 * and directories only, files FILE_MODE and directories DIR_MODE, and every time `epoch` (the
 * commit time, as for the other layers).
 */
export function copyStaticSite(dist: string, dest: string, epoch: number): void {
  const copy = (from: string, to: string) => {
    mkdirSync(to);
    for (const name of readdirSync(from).sort()) {
      const source = join(from, name);
      const target = join(to, name);
      const stats = lstatSync(source);
      if (stats.isDirectory()) {
        copy(source, target);
      } else if (stats.isFile()) {
        copyFileSync(source, target);
        chmodSync(target, FILE_MODE);
        utimesSync(target, epoch, epoch);
      } else {
        throw new ReleaseError(`${source} is neither a regular file nor a directory`);
      }
    }
    // After the entries: adding them changes the directory's time.
    chmodSync(to, DIR_MODE);
    utimesSync(to, epoch, epoch);
  };
  copy(dist, dest);
}

/** Runs one of staticBuildCommands from the repository root; throws when it fails. */
export type StaticBuildRunner = (cmd: string[]) => Promise<unknown>;

/**
 * Builds and checks one project; returns its build output. `runCommand` runs each command from
 * the repository root (the release streams their output; the nightly logs it in its run log).
 */
export async function buildStaticSite(
  root: string,
  project: string,
  bun: string,
  runCommand: StaticBuildRunner = (cmd) => run(cmd, { cwd: root, stream: true }),
): Promise<string> {
  const dir = join(root, project);
  const dotenv = dotenvFiles(dir);
  if (dotenv.length > 0) {
    throw new ReleaseError(
      `${project} holds ${dotenv.join(", ")}: the build would put their values into the site; remove them for a release build`,
    );
  }
  const dist = join(dir, DIST_DIR);
  // Nothing of an earlier build may end up in the image.
  rmSync(dist, { recursive: true, force: true });
  for (const cmd of staticBuildCommands(bun, project)) {
    log("build", `${project}: bun ${cmd.slice(1).join(" ")}`);
    await runCommand(cmd);
  }
  const problems = staticSiteProblems(dist);
  if (problems.length > 0) {
    throw new ReleaseError(`${project}/${DIST_DIR} cannot go into the image, the web service would refuse it: ${problems.join("; ")}`);
  }
  return dist;
}

/**
 * Builds the `[static]` project of every planned image that has one, with `bun` (this process's
 * own Bun, the one .prototools pins, by default); returns image → its checked build output. A
 * project is built once, and every image that names it gets that one build: the web image and
 * the dashboard check image hold the same frontend build of the release.
 */
export async function buildStaticSites(
  plan: ReleasePlan,
  root: string,
  bun: string = process.execPath,
  build: (root: string, project: string, bun: string) => Promise<string> = buildStaticSite,
): Promise<Map<string, string>> {
  const sites = new Map<string, string>();
  const built = new Map<string, string>();
  for (const image of plan.images) {
    if (image.staticSite !== undefined) {
      let dist = built.get(image.staticSite);
      if (dist === undefined) {
        dist = await build(root, image.staticSite, bun);
        built.set(image.staticSite, dist);
      }
      sites.set(image.name, dist);
    }
  }
  return sites;
}
