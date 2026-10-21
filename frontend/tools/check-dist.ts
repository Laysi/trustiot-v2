/**
 * Checks the build output, dist/, before it is packed into the web image (services/web). The
 * web service serves these files with `script-src 'self'; style-src 'self'` and no `data:`
 * (frontend/README.md), so anything inline would be blocked in the browser; this fails the build
 * instead. It also keeps the layout the web service relies on: index.html and the files of
 * public/ at the root, everything else under static/ with hash-only names, and only file types
 * the web service has a content type for.
 *
 * Usage, from the repository root: bun run --cwd frontend check:dist
 */

import { lstatSync, readdirSync, readFileSync } from "node:fs";
import { join, posix } from "node:path";
import { fileURLToPath } from "node:url";

export const DIST_DIR = fileURLToPath(new URL("../dist", import.meta.url));
export const PUBLIC_DIR = fileURLToPath(new URL("../public", import.meta.url));

/** The extensions the web service serves, each with its content type (services/web). */
export const ALLOWED_EXTENSIONS: ReadonlySet<string> = new Set([
  "html",
  "js",
  "css",
  "svg",
  "png",
  "ico",
  "woff2",
  "txt",
  "webmanifest",
]);

/** A name under static/: Vite's hash and an extension, nothing taken from the source. */
export const STATIC_NAME = /^[A-Za-z0-9_-]+\.[a-z0-9]+$/;
/** A name at the root (index.html and the files of public/). */
const ROOT_NAME = /^[a-z0-9][a-z0-9._-]*\.[a-z0-9]+$/;

export interface DistFile {
  /** Relative to dist/, `/`-separated. */
  path: string;
  /** null for something that is not a regular file (a symbolic link, a device). */
  content: string | null;
}

function extension(path: string): string {
  const name = posix.basename(path);
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1);
}

/** Markup and URLs the CSP blocks, or that would run script from a document. */
function inlineProblems(path: string, text: string, kind: "html" | "css" | "svg"): string[] {
  const problems: string[] = [];
  const lower = text.toLowerCase();
  if (lower.includes("javascript:")) problems.push(`${path}: contains a javascript: URL`);
  if (/\bdata:/.test(lower)) problems.push(`${path}: contains a data: URI (the CSP allows none)`);
  if (kind === "css") {
    if (/expression\s*\(/.test(lower)) problems.push(`${path}: contains a CSS expression()`);
    for (const match of lower.matchAll(/url\(\s*["']?([^"')\s]+)/g)) {
      const url = match[1] ?? "";
      if (/^[a-z][a-z0-9+.-]*:/.test(url) || url.startsWith("//")) {
        problems.push(`${path}: url(${url}) is not on this origin`);
      }
    }
    return problems;
  }
  if (/<style[\s>]/.test(lower)) problems.push(`${path}: has a <style> element (the CSP allows no inline style)`);
  if (/\sstyle\s*=/.test(lower)) problems.push(`${path}: has a style attribute (the CSP allows no inline style)`);
  if (/\son[a-z]+\s*=/.test(lower)) problems.push(`${path}: has an inline event handler attribute`);
  for (const match of lower.matchAll(/<script\b([^>]*)>/g)) {
    if (!/\ssrc\s*=/.test(match[1] ?? "")) {
      problems.push(`${path}: has an inline <script> (the CSP allows only script files)`);
    }
  }
  return problems;
}

/** Every src and href in index.html points at static/ or a root file of public/. */
function referenceProblems(html: string, rootFiles: ReadonlySet<string>): string[] {
  const problems: string[] = [];
  for (const match of html.matchAll(/\s(?:src|href)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/gi)) {
    const url = match[1] ?? match[2] ?? match[3] ?? "";
    const ok =
      (url.startsWith("/static/") && STATIC_NAME.test(url.slice("/static/".length))) ||
      (url.startsWith("/") && rootFiles.has(url.slice(1)));
    if (!ok) {
      problems.push(`index.html: ${url} is neither under /static/ nor a root file of public/`);
    }
  }
  return problems;
}

/** What is wrong with the build output; empty when it can be packed. */
export function checkDist(files: readonly DistFile[], publicFiles: ReadonlySet<string>): string[] {
  const problems: string[] = [];
  const rootFiles = new Set([...publicFiles, "index.html"]);
  if (!files.some((file) => file.path === "index.html")) {
    problems.push("index.html is missing");
  }
  for (const file of files) {
    const ext = extension(file.path);
    if (file.content === null) {
      problems.push(`${file.path}: not a regular file`);
      continue;
    }
    if (ext === "map") {
      problems.push(`${file.path}: source maps are not shipped`);
      continue;
    }
    if (!ALLOWED_EXTENSIONS.has(ext)) {
      problems.push(`${file.path}: .${ext || "(none)"} is not a file type the web service serves`);
    }
    const parts = file.path.split("/");
    if (parts.length === 1) {
      if (!rootFiles.has(file.path) || !ROOT_NAME.test(file.path)) {
        problems.push(`${file.path}: only index.html and the files of public/ belong at the root`);
      }
    } else if (parts.length !== 2 || parts[0] !== "static" || !STATIC_NAME.test(parts[1] ?? "")) {
      problems.push(`${file.path}: everything else belongs directly under static/ with a hash-only name`);
    }
    if (ext === "html") {
      if (file.path !== "index.html") {
        problems.push(`${file.path}: index.html is the only document`);
      }
      problems.push(...inlineProblems(file.path, file.content, "html"));
      if (file.path === "index.html") {
        problems.push(...referenceProblems(file.content, rootFiles));
      }
    } else if (ext === "css") {
      problems.push(...inlineProblems(file.path, file.content, "css"));
    } else if (ext === "svg") {
      problems.push(...inlineProblems(file.path, file.content, "svg"));
    }
  }
  return problems;
}

/** The files under `dir`, as checkDist takes them. Binary files are read as text; only markup is inspected. */
export function readDist(dir: string): DistFile[] {
  const files: DistFile[] = [];
  const walk = (relative: string) => {
    for (const entry of readdirSync(join(dir, relative), { withFileTypes: true })) {
      const path = relative === "" ? entry.name : `${relative}/${entry.name}`;
      const stats = lstatSync(join(dir, path));
      if (stats.isDirectory()) {
        walk(path);
      } else if (stats.isFile()) {
        files.push({ path, content: readFileSync(join(dir, path), "utf8") });
      } else {
        files.push({ path, content: null });
      }
    }
  };
  walk("");
  return files.sort((a, b) => a.path.localeCompare(b.path));
}

if (import.meta.main) {
  let problems: string[];
  try {
    const publicFiles = new Set(readdirSync(PUBLIC_DIR));
    problems = checkDist(readDist(DIST_DIR), publicFiles);
  } catch (error) {
    problems = [
      `cannot read ${DIST_DIR}: ${error instanceof Error ? error.message : String(error)} (run the build first)`,
    ];
  }
  if (problems.length > 0) {
    console.error(`The build output is not fit to serve:\n${problems.map((problem) => `- ${problem}`).join("\n")}`);
    process.exit(1);
  }
}
