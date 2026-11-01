/**
 * The build context of a Rust image, shared by release.yml's build job (pack.ts), the nightly's
 * service images (scripts/nightly/service-image.ts) and the image tests under deploy/images/, so
 * that each of them builds what a release publishes. The context holds only:
 *
 *   <bin>            the cross-compiled binary, mode 0755
 *   www/             the checked build output of the image's [static] project, if it has one
 *                    (static-build.ts copyStaticSite: fixed modes and times)
 *   Containerfile    RUST_CONTAINERFILE rendered for the binary, or the image directory's own
 *                    Containerfile as it is (image.toml `containerfile`), which reads the same
 *                    context
 *
 * An image's own Containerfile is checked again here, as plan checks it: base images pinned by
 * digest, and no RUN in a stage that builds for the image's platform.
 */

import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { targetStageRunProblems } from "./containerfile.ts";
import { RUST_CONTAINERFILE, ReleaseError, STATIC_CONTEXT_DIR, baseImageProblems, renderRustContainerfile } from "./lib.ts";
import { copyStaticSite } from "./static-build.ts";

/** What goes into the build context of one Rust image. */
export interface RustImageContext {
  /** The repository root, which the Containerfiles are read from. */
  root: string;
  /** The binary target name, also the file name in the context. */
  bin: string;
  /** The compiled aarch64 binary. */
  binary: string;
  /** image.toml's own Containerfile (relative to the repository root), when it names one. */
  containerfile?: string;
  /** The checked build output of the image's [static] project; given exactly when it has one. */
  site?: string;
  /** The time of the static site's files, in seconds: the commit time in a release. */
  epoch: number;
  /** The directory to write: missing or empty. */
  dir: string;
}

/** Writes the build context of a Rust image (see the file comment); returns its Containerfile. */
export function writeRustImageContext(context: RustImageContext): string {
  const { root, bin, binary, dir } = context;
  if (existsSync(dir) && readdirSync(dir).length > 0) {
    throw new ReleaseError(`the build context ${dir} is not empty`);
  }
  if (bin === STATIC_CONTEXT_DIR || bin === "Containerfile") {
    throw new ReleaseError(`a binary named ${bin} would take the place of another file of the build context`);
  }
  mkdirSync(dir, { recursive: true });
  copyFileSync(binary, join(dir, bin));
  chmodSync(join(dir, bin), 0o755);
  if (context.site !== undefined) {
    copyStaticSite(context.site, join(dir, STATIC_CONTEXT_DIR), context.epoch);
  }
  let text: string;
  if (context.containerfile === undefined) {
    text = renderRustContainerfile(readFileSync(join(root, RUST_CONTAINERFILE), "utf8"), bin, { staticSite: context.site !== undefined });
  } else {
    text = readFileSync(join(root, context.containerfile), "utf8");
    const problems = [...baseImageProblems(text), ...targetStageRunProblems(text)];
    if (problems.length > 0) {
      throw new ReleaseError(`${context.containerfile}: ${problems.join("; ")}`);
    }
  }
  const containerfile = join(dir, "Containerfile");
  writeFileSync(containerfile, text);
  return containerfile;
}
