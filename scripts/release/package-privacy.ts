/**
 * The anonymous-pull check of verify.ts: every published image and the chart must refuse an
 * anonymous request, so the GHCR packages are private (docs/architecture/ci-release.md, "GHCR
 * 套件的可見性").
 *
 * verify.ts runs it after every other check and collects the results instead of stopping at the
 * first public package, so a release whose packages are public still gets all of its other
 * checks and a complete report before verify fails. The deciding logic is here, without tools,
 * so package-privacy.test.ts covers it.
 */

import { ReleaseError } from "./lib.ts";

/** What skopeo reports when the registry refuses an anonymous request. */
const ANONYMOUS_REFUSED = /unauthorized|denied|authentication required|forbidden/i;
/** The longest registry message kept in a result. */
const DETAIL_MAX = 200;

export type PrivacyState =
  /** An anonymous request was refused. */
  | "private"
  /** An anonymous request succeeded: anyone can pull the package. */
  | "public"
  /** The request failed for another reason, so the visibility is unknown. */
  | "unknown"
  /** Local test mode: a throwaway registry on this machine has no visibility. */
  | "skipped";

export interface PackagePrivacy {
  /** Image name, or the chart name. */
  name: string;
  /** The tagged reference that was requested anonymously. */
  ref: string;
  state: PrivacyState;
  /** For `unknown`: the first line of the registry's answer. */
  detail?: string;
}

/** Classifies the result of `skopeo inspect --raw --no-creds docker://<ref>`. */
export function classifyAnonymousPull(name: string, ref: string, exitCode: number, stderr: string): PackagePrivacy {
  if (exitCode === 0) {
    return { name, ref, state: "public" };
  }
  if (ANONYMOUS_REFUSED.test(stderr)) {
    return { name, ref, state: "private" };
  }
  const detail = (stderr.trim().split("\n")[0] ?? "").slice(0, DETAIL_MAX);
  return { name, ref, state: "unknown", detail: detail === "" ? `skopeo exited with status ${exitCode}` : detail };
}

/** The end of a report line (zh-TW) that states the package's privacy result. */
export function privacyNote(result: PackagePrivacy): string {
  switch (result.state) {
    case "private":
      return "；匿名拉取被拒（套件為私有）。";
    case "public":
      return "；**未通過：可以匿名拉取，套件必須是私有的。**";
    case "unknown":
      return `；**未通過：無法判斷套件是否私有**（匿名拉取的回應：\`${result.detail ?? ""}\`）。`;
    case "skipped":
      return "。";
  }
}

/**
 * The error verify fails with when any package is not known to be private, naming every
 * affected reference (public ones first, by name, so the one-line `error` output names them);
 * undefined when every package is private or the check was skipped.
 */
export function privacyError(results: readonly PackagePrivacy[]): ReleaseError | undefined {
  const exposed = results.filter((result) => result.state === "public");
  const unknown = results.filter((result) => result.state === "unknown");
  if (exposed.length === 0 && unknown.length === 0) {
    return undefined;
  }
  const parts: string[] = [];
  if (exposed.length > 0) {
    parts.push(
      `${exposed.map((result) => result.name).join(", ")} can be pulled anonymously (${exposed.map((result) => result.ref).join(", ")}); ` +
        "the packages must be private",
    );
  }
  if (unknown.length > 0) {
    parts.push(`cannot tell whether ${unknown.map((result) => `${result.ref} (${result.detail ?? ""})`).join(", ")} is private`);
  }
  return new ReleaseError(`${parts.join("; ")}. Every other check passed; the report lists the results`);
}

/** The verdict line (zh-TW) at the top of a report whose packages are not all private. */
export function privacySummary(results: readonly PackagePrivacy[]): string | undefined {
  const failed = results.filter((result) => result.state === "public" || result.state === "unknown");
  if (failed.length === 0) {
    return undefined;
  }
  const named = failed.map((result) => `\`${result.ref}\`（${result.state === "public" ? "可以匿名拉取" : "無法判斷是否私有"}）`);
  return `**驗證未通過**：下列套件不能確定是私有的：${named.join("、")}。其他檢查都已通過，結果如下。`;
}
