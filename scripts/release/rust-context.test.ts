import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { RUST_CONTAINERFILE } from "./lib.ts";
import { writeRustImageContext } from "./rust-context.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const PINNED = `gcr.io/distroless/cc-debian13:nonroot@sha256:${"a".repeat(64)}`;
const OWN = "deploy/images/checker/Containerfile";

describe("writeRustImageContext", () => {
  let root: string;
  let binary: string;
  let site: string;
  let dir: string;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "rust-context-"));
    mkdirSync(join(root, dirname(RUST_CONTAINERFILE)), { recursive: true });
    writeFileSync(join(root, RUST_CONTAINERFILE), readFileSync(join(REPO_ROOT, RUST_CONTAINERFILE)));
    mkdirSync(join(root, dirname(OWN)), { recursive: true });
    writeFileSync(join(root, OWN), `FROM ${PINNED}\nCOPY checker /usr/local/bin/checker\nCOPY www/sandbox/ /srv/www/sandbox/\n`);
    binary = join(root, "built-checker");
    writeFileSync(binary, "binary");
    site = join(root, "dist");
    mkdirSync(join(site, "sandbox"), { recursive: true });
    writeFileSync(join(site, "index.html"), "index");
    writeFileSync(join(site, "sandbox", "check.html"), "check");
    dir = join(root, "context");
  });
  afterEach(() => rmSync(root, { recursive: true, force: true }));

  test("holds the binary and the shared Containerfile rendered for it", () => {
    const containerfile = writeRustImageContext({ root, bin: "checker", binary, epoch: 0, dir });
    expect(containerfile).toBe(join(dir, "Containerfile"));
    expect(readdirSync(dir).sort()).toEqual(["Containerfile", "checker"]);
    expect(statSync(join(dir, "checker")).mode & 0o777).toBe(0o755);
    expect(readFileSync(containerfile, "utf8")).toContain("COPY --chmod=0555 checker /usr/local/bin/checker");
  });

  test("holds the image's own Containerfile as it is, and the site with fixed modes and times", () => {
    const epoch = 1_700_000_000;
    const containerfile = writeRustImageContext({ root, bin: "checker", binary, containerfile: OWN, site, epoch, dir });
    expect(readFileSync(containerfile, "utf8")).toBe(readFileSync(join(root, OWN), "utf8"));
    expect(readdirSync(dir).sort()).toEqual(["Containerfile", "checker", "www"]);
    const check = statSync(join(dir, "www", "sandbox", "check.html"));
    expect(check.mode & 0o777).toBe(0o644);
    expect(check.mtimeMs).toBe(epoch * 1000);
    expect(statSync(join(dir, "www", "sandbox")).mode & 0o777).toBe(0o755);
  });

  test.each([
    ["an unpinned base image", `FROM gcr.io/distroless/cc-debian13:nonroot\nCOPY checker /checker\n`, "digest"],
    ["a RUN in the arm64 stage", `FROM ${PINNED}\nRUN ["/checker", "--version"]\n`, "RUN"],
  ])("refuses an own Containerfile with %s", (_label, text, message) => {
    writeFileSync(join(root, OWN), text);
    expect(() => writeRustImageContext({ root, bin: "checker", binary, containerfile: OWN, epoch: 0, dir })).toThrow(message);
  });

  test("refuses a directory that is not empty, and a binary named like another file of the context", () => {
    mkdirSync(dir);
    writeFileSync(join(dir, "left-over"), "");
    expect(() => writeRustImageContext({ root, bin: "checker", binary, epoch: 0, dir })).toThrow("is not empty");
    rmSync(dir, { recursive: true });
    for (const bin of ["www", "Containerfile"]) {
      expect(() => writeRustImageContext({ root, bin, binary, epoch: 0, dir })).toThrow("would take the place of another file");
    }
  });
});
