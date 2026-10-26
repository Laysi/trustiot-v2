import { afterAll, describe, expect, test } from "bun:test";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
  truncateSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { SLOW_TEST_MS } from "../../deploy/chart/tests/timing.ts";
import { REGISTRY, type ReleasePlan, planImage } from "./lib.ts";
import {
  MAX_SITE_BYTES,
  SITE_EXTENSIONS,
  buildStaticSites,
  copyStaticSite,
  dotenvFiles,
  staticBuildCommands,
  staticSiteProblems,
} from "./static-build.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

const dirs: string[] = [];
afterAll(() => {
  for (const dir of dirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

/** A temporary directory holding `files` (path → content), removed after the file's tests. */
function tree(files: Record<string, string> = {}): string {
  const dir = mkdtempSync(join(tmpdir(), "release-static-test-"));
  dirs.push(dir);
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(dir, path)), { recursive: true });
    writeFileSync(join(dir, path), content);
  }
  return dir;
}

describe("the rules of services/web", () => {
  const site = readFileSync(join(ROOT, "services/web/src/site.rs"), "utf8");

  test("are the extensions and the size limit that services/web loads", () => {
    const table = /pub const CONTENT_TYPES: &\[\(&str, &str\)\] = &\[([\s\S]*?)\];/.exec(site)?.[1] ?? "";
    const extensions = [...table.matchAll(/\("([a-z0-9]+)",/g)].map((match) => match[1]);
    expect(extensions.length).toBeGreaterThan(0);
    expect(extensions.toSorted()).toEqual([...SITE_EXTENSIONS].toSorted());
    const limit = /pub const MAX_SITE_BYTES: u64 = ([\d *_]+);/.exec(site)?.[1] ?? "";
    const bytes = limit.split("*").reduce((product, factor) => product * Number(factor.trim().replaceAll("_", "")), 1);
    expect(bytes).toBe(MAX_SITE_BYTES);
  });

  test("are the extensions that frontend's check:dist allows", async () => {
    // C1 of Laysi/trustiot-v2#123: the build output check and the service agree on file types.
    const checkDist = join(ROOT, "frontend/tools/check-dist.ts");
    const { ALLOWED_EXTENSIONS } = (await import(checkDist)) as { ALLOWED_EXTENSIONS: ReadonlySet<string> };
    expect([...ALLOWED_EXTENSIONS].toSorted()).toEqual([...SITE_EXTENSIONS].toSorted());
  });

  test("accept the fixture site that services/web's tests serve", () => {
    expect(staticSiteProblems(join(ROOT, "services/web/tests/fixtures/site"))).toEqual([]);
  });
});

describe("staticBuildCommands", () => {
  test("install the locked packages without scripts, build, and check the output", () => {
    expect(staticBuildCommands("/proto/bun", "frontend")).toEqual([
      ["/proto/bun", "install", "--cwd", "frontend", "--frozen-lockfile", "--ignore-scripts"],
      ["/proto/bun", "run", "--cwd", "frontend", "build"],
      ["/proto/bun", "run", "--cwd", "frontend", "check:dist"],
    ]);
  });
});

describe("staticSiteProblems", () => {
  test("accepts index.html with files at the root and in directories", () => {
    const dist = tree({ "index.html": "<!doctype html>", "robots.txt": "", "static/B7f3kQ2x.js": "", "static/x/a-b_c.css": "" });
    expect(staticSiteProblems(dist)).toEqual([]);
  });

  test("lists every file services/web would refuse", () => {
    const dist = tree({
      "index.html": "",
      ".DS_Store": "",
      Makefile: "",
      "a.map": "",
      "a b.js": "",
      "static/a.JS": "",
      ".well-known/security.txt": "",
    });
    symlinkSync(join(dist, "index.html"), join(dist, "link.js"));
    expect(staticSiteProblems(dist)).toEqual([
      ".DS_Store: a name other than letters, digits, `_`, `-` and `.` (not first)",
      ".well-known: a name other than letters, digits, `_`, `-` and `.` (not first)",
      "Makefile: an extension the web service does not serve",
      "a b.js: a name other than letters, digits, `_`, `-` and `.` (not first)",
      "a.map: an extension the web service does not serve",
      "link.js: a symbolic link",
      "static/a.JS: an extension the web service does not serve",
    ]);
  });

  test("requires index.html as a file at the root", () => {
    expect(staticSiteProblems(tree({ "static/index.html": "" }))).toEqual(["there is no index.html file at the root"]);
    const linked = tree({ "page.html": "" });
    symlinkSync(join(linked, "page.html"), join(linked, "index.html"));
    expect(staticSiteProblems(linked)).toEqual(["index.html: a symbolic link", "there is no index.html file at the root"]);
    expect(staticSiteProblems(join(tree(), "dist"))[0]).toContain("dist/ does not exist");
  });

  test("refuses more bytes than services/web loads", () => {
    const dist = tree({ "index.html": "", "static/a.js": "", "static/b.js": "" });
    // Sparse files: the size counts, not the disk space.
    truncateSync(join(dist, "static/a.js"), MAX_SITE_BYTES);
    expect(staticSiteProblems(dist)).toEqual([]);
    truncateSync(join(dist, "static/b.js"), 1);
    expect(staticSiteProblems(dist)).toEqual([
      `the files hold ${MAX_SITE_BYTES + 1} bytes, more than the ${MAX_SITE_BYTES} the web service loads`,
    ]);
  });
});

describe("copyStaticSite", () => {
  const EPOCH = 1_791_630_056;

  test("copies files and directories with fixed modes and times", () => {
    const dist = tree({ "index.html": "<p>", "static/a.js": "a()", "static/x/b.css": "b{}" });
    chmodSync(join(dist, "index.html"), 0o600);
    chmodSync(join(dist, "static/a.js"), 0o755);
    chmodSync(join(dist, "static/x"), 0o700);
    const dest = join(tree(), "www");
    copyStaticSite(dist, dest, EPOCH);
    for (const [path, mode] of [
      ["", 0o755],
      ["static", 0o755],
      ["static/x", 0o755],
      ["index.html", 0o644],
      ["static/a.js", 0o644],
      ["static/x/b.css", 0o644],
    ] as const) {
      const stats = statSync(join(dest, path));
      expect(stats.mode & 0o777, path).toBe(mode);
      expect(stats.mtimeMs, path).toBe(EPOCH * 1000);
    }
    expect(readFileSync(join(dest, "static/x/b.css"), "utf8")).toBe("b{}");
  });

  test("refuses anything but files and directories", () => {
    const dist = tree({ "index.html": "" });
    symlinkSync(join(dist, "index.html"), join(dist, "link.html"));
    expect(() => copyStaticSite(dist, join(tree(), "www"), EPOCH)).toThrow("link.html is neither a regular file nor a directory");
  });
});

describe("dotenvFiles", () => {
  test("lists the files a build tool would read its environment from", () => {
    const dir = tree({ ".env": "", ".env.local": "", ".env.production.local": "", ".envrc": "", "env.ts": "", ".env-old": "" });
    expect(dotenvFiles(dir)).toEqual([".env", ".env.local", ".env.production.local"]);
  });
});

describe("buildStaticSites", () => {
  /**
   * A repository with the Bun project `site`: one local dependency (so it has a bun.lock and
   * installs without a network), a postinstall script that must not run, a build script that
   * writes dist/ (with a source map when `source-map` exists) and a check:dist that fails when
   * `fail-check` exists.
   */
  function repository(): { root: string; project: string } {
    const root = tree({
      "site/package.json": JSON.stringify({
        name: "site",
        private: true,
        dependencies: { dep: "file:./dep" },
        scripts: { postinstall: "bun postinstall.ts", build: "bun make-dist.ts", "check:dist": "bun check.ts" },
      }),
      "site/dep/package.json": JSON.stringify({ name: "dep", version: "1.0.0" }),
      "site/postinstall.ts": 'import { writeFileSync } from "node:fs";\nwriteFileSync("postinstall-ran", "");\n',
      "site/make-dist.ts": [
        'import { existsSync, mkdirSync, writeFileSync } from "node:fs";',
        'mkdirSync("dist/static", { recursive: true });',
        'writeFileSync("dist/index.html", "<!doctype html>");',
        'writeFileSync("dist/static/B7f3kQ2x.js", "");',
        'if (existsSync("source-map")) writeFileSync("dist/static/B7f3kQ2x.js.map", "{}");',
        "",
      ].join("\n"),
      "site/check.ts": 'import { existsSync } from "node:fs";\nif (existsSync("fail-check")) process.exit(1);\n',
    });
    const project = join(root, "site");
    // The lockfile, as the project would commit it; then a fresh checkout without node_modules.
    const lock = Bun.spawnSync([process.execPath, "install", "--cwd", project, "--ignore-scripts"], { stdout: "pipe", stderr: "pipe" });
    if (lock.exitCode !== 0 || !existsSync(join(project, "bun.lock"))) {
      throw new Error(`bun install did not write site/bun.lock: ${lock.stderr.toString()}`);
    }
    rmSync(join(project, "node_modules"), { recursive: true, force: true });
    return { root, project };
  }

  function planFor(staticSite: string | undefined): ReleasePlan {
    return {
      tag: "v1.2.0-rc.1",
      version: "1.2.0-rc.1",
      prerelease: true,
      commit: "0123456789abcdef0123456789abcdef01234567",
      sourceDateEpoch: 1_791_630_056,
      repository: "Laysi/trustiot-v2",
      images: [
        planImage({ kind: "rust", name: "identity", package: "identity" }, "1.2.0-rc.1"),
        planImage({ kind: "rust", name: "web", package: "web", ...(staticSite === undefined ? {} : { staticSite }) }, "1.2.0-rc.1"),
      ],
      chart: { name: "trustiot", repository: `${REGISTRY}/charts/trustiot`, version: "1.2.0-rc.1" },
    };
  }

  test(
    "builds and checks each static project from its lockfile without install scripts",
    async () => {
      const { root, project } = repository();
      expect(await buildStaticSites(planFor(undefined), root)).toEqual(new Map());
      mkdirSync(join(project, "dist"));
      writeFileSync(join(project, "dist/stale.js"), "");

      const sites = await buildStaticSites(planFor("site"), root);
      expect(sites).toEqual(new Map([["web", join(project, "dist")]]));
      expect(existsSync(join(project, "node_modules/dep/package.json"))).toBe(true);
      expect(existsSync(join(project, "postinstall-ran"))).toBe(false);
      expect(existsSync(join(project, "dist/stale.js"))).toBe(false);
      expect(existsSync(join(project, "dist/static/B7f3kQ2x.js"))).toBe(true);
    },
    SLOW_TEST_MS,
  );

  test("builds a project once and gives every image that names it the same build", async () => {
    const plan = planFor("site");
    plan.images.push(
      planImage({ kind: "rust", name: "dashboard-check", package: "dashboard-check", staticSite: "site" }, "1.2.0-rc.1"),
      planImage({ kind: "rust", name: "docs", package: "docs", staticSite: "other" }, "1.2.0-rc.1"),
    );
    const builds: string[] = [];
    const sites = await buildStaticSites(plan, "/repo", "bun", async (root, project) => {
      builds.push(project);
      return `${root}/${project}/dist`;
    });
    expect(builds).toEqual(["site", "other"]);
    expect(sites).toEqual(
      new Map([
        ["web", "/repo/site/dist"],
        ["dashboard-check", "/repo/site/dist"],
        ["docs", "/repo/other/dist"],
      ]),
    );
  });

  test(
    "stops on a failed check, an output services/web would refuse, or a dotenv file",
    async () => {
      const { root, project } = repository();
      writeFileSync(join(project, "fail-check"), "");
      await expect(buildStaticSites(planFor("site"), root)).rejects.toThrow("`bun run` exited with status 1");
      rmSync(join(project, "fail-check"));

      writeFileSync(join(project, "source-map"), "");
      await expect(buildStaticSites(planFor("site"), root)).rejects.toThrow(
        "site/dist cannot go into the image, the web service would refuse it: static/B7f3kQ2x.js.map: an extension the web service does not serve",
      );
      rmSync(join(project, "source-map"));

      // Refused before anything runs: the earlier build output is still there.
      writeFileSync(join(project, ".env.local"), "VITE_X=1\n");
      await expect(buildStaticSites(planFor("site"), root)).rejects.toThrow("site holds .env.local");
      expect(existsSync(join(project, "dist/static/B7f3kQ2x.js.map"))).toBe(true);
    },
    SLOW_TEST_MS,
  );
});
