import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  decide,
  failureComment,
  failureIssueBody,
  FAILURE_LABEL,
  GitHub,
  ISSUE_TITLE,
  notify,
  type RunInfo,
} from "./notify.ts";
import { failureHeadline, formatDuration, isOk, renderMarkdown, tail, type Report } from "./report.ts";
import { requireDigestPinned } from "./run.ts";

const DIGEST = `sha256:${"a".repeat(64)}`;

function report(overrides: Partial<Report> = {}): Report {
  return {
    startedAt: "2026-10-11T18:17:00.000Z",
    finishedAt: "2026-10-11T18:25:00.000Z",
    k3sImage: `rancher/k3s:v1.36.5-k3s1@${DIGEST}`,
    ok: false,
    shapes: [
      {
        shape: "onprem",
        ok: true,
        durationMs: 90_000,
        checks: [{ name: "helm install --wait", ok: true, detail: "deployed", durationMs: 30_000 }],
      },
      {
        shape: "saas",
        ok: false,
        durationMs: 200_000,
        checks: [
          { name: "helm install --wait", ok: true, detail: "deployed", durationMs: 30_000 },
          { name: "every workload of the release is ready", ok: false, detail: "timed out | waiting\nfor rollout", durationMs: 180_000 },
        ],
        diagnostics: "### events\nBackOff pulling image",
      },
    ],
    ...overrides,
  };
}

const info: RunInfo = {
  runUrl: "https://github.com/Laysi/trustiot-v2/actions/runs/42",
  sha: "0123456789abcdef0123456789abcdef01234567",
  eventName: "schedule",
  result: "failure",
  failedSteps: ["Run the smoke test"],
};

describe("report", () => {
  test("isOk needs every expected shape to pass and no infrastructure error", () => {
    const passing = report().shapes.map((s) => ({ ...s, ok: true }));
    expect(isOk({ shapes: passing }, 2)).toBe(true);
    expect(isOk({ shapes: passing }, 3)).toBe(false);
    expect(isOk({ shapes: passing, infraError: "k3d failed" }, 2)).toBe(false);
    expect(isOk({ shapes: report().shapes }, 2)).toBe(false);
  });

  test("renders failed checks, escapes table cells and keeps the diagnostics of failed shapes only", () => {
    const markdown = renderMarkdown(report());
    expect(markdown).toContain("## nightly smoke test: FAILED");
    expect(markdown).toContain("### values-onprem.yaml: passed (1m30s)");
    expect(markdown).toContain("### values-saas.yaml: FAILED (3m20s)");
    expect(markdown).toContain("timed out \\| waiting for rollout");
    expect(markdown).toContain("BackOff pulling image");
    expect(markdown.match(/<details>/g)).toHaveLength(1);
  });

  test("headline names the first failed check of each failed shape", () => {
    expect(failureHeadline(report())).toBe("values-saas.yaml: every workload of the release is ready");
    expect(failureHeadline(report({ infraError: "k3d cluster create failed\nmore" }))).toBe(
      "cluster setup or teardown failed: k3d cluster create failed",
    );
  });

  test("tail keeps the end of a long text", () => {
    expect(tail("abcdef", 10)).toBe("abcdef");
    expect(tail("abcdef", 3)).toContain("def");
    expect(tail("abcdef", 3)).toContain("3 characters cut");
    expect(formatDuration(59_400)).toBe("59s");
    expect(formatDuration(61_000)).toBe("1m01s");
  });
});

describe("requireDigestPinned", () => {
  test("accepts name:tag@sha256 and rejects anything that can move", () => {
    const pinned = `rancher/k3s:v1.36.5-k3s1@${DIGEST}`;
    expect(requireDigestPinned(pinned)).toBe(pinned);
    expect(() => requireDigestPinned("rancher/k3s:v1.36.5-k3s1")).toThrow("pinned by digest");
    expect(() => requireDigestPinned(undefined)).toThrow("pinned by digest");
    expect(() => requireDigestPinned("rancher/k3s@sha256:abc")).toThrow("pinned by digest");
  });
});

describe("decide", () => {
  test("maps event and result to an action", () => {
    expect(decide("schedule", "failure")).toBe("failure");
    expect(decide("schedule", "cancelled")).toBe("failure");
    expect(decide("workflow_dispatch", "success")).toBe("recovery");
    expect(decide("pull_request", "failure")).toBe("nothing");
    expect(decide("push", "failure")).toBe("nothing");
  });
});

describe("texts", () => {
  test("the issue body follows the template headings and names the run", () => {
    const body = failureIssueBody(info, report(), renderMarkdown(report()), new Date("2026-10-11T19:00:00Z"));
    for (const heading of ["## 背景 (Why)", "## 範圍 (What)", "## 現狀 (State)", "## 驗收條件 (Acceptance Criteria)", "## 指標 (Pointers)"]) {
      expect(body).toContain(heading);
    }
    expect(body).toContain("2026-10-11");
    expect(body).toContain(info.runUrl);
    expect(body).toContain("values-saas.yaml: every workload of the release is ready");
  });

  test("without a report the texts say which steps failed", () => {
    const body = failureIssueBody(info, undefined, undefined, new Date("2026-10-11T19:00:00Z"));
    expect(body).toContain("沒有產生報告");
    expect(body).toContain("Run the smoke test");
    expect(failureComment(info, undefined, undefined, new Date("2026-10-11T19:00:00Z"))).toContain("仍然失敗");
  });

  test("a very long report stays under GitHub's comment limit", () => {
    const huge = "x".repeat(200_000);
    expect(failureIssueBody(info, report(), huge).length).toBeLessThan(65_536);
    expect(failureComment(info, report(), huge).length).toBeLessThan(65_536);
  });
});

interface Call {
  method: string;
  path: string;
  body?: unknown;
}

/** A GitHub client that records requests and answers from a script. */
class FakeGitHub extends GitHub {
  calls: Call[] = [];
  constructor(private readonly answers: (call: Call) => { status: number; data: unknown }) {
    super("https://api.invalid", "token", "Laysi/trustiot-v2");
  }
  override async request<T>(method: string, path: string, body?: unknown): Promise<{ status: number; data: T }> {
    const call: Call = { method, path, body };
    this.calls.push(call);
    return this.answers(call) as { status: number; data: T };
  }
}

describe("notify", () => {
  let dir: string;
  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "nightly-notify-"));
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  function env(extra: Record<string, string>): NodeJS.ProcessEnv {
    return {
      GITHUB_REPOSITORY: "Laysi/trustiot-v2",
      GITHUB_RUN_ID: "42",
      GITHUB_SHA: info.sha,
      NIGHTLY_REPORT_DIR: dir,
      NIGHTLY_JOB_NAME: "smoke",
      ...extra,
    };
  }

  function writeReport(value: Report): void {
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "report.json"), JSON.stringify(value));
    writeFileSync(join(dir, "report.md"), renderMarkdown(value));
  }

  const jobs = { jobs: [{ name: "smoke", steps: [{ name: "Run the smoke test", conclusion: "failure" }] }] };

  test("a failed night with no open issue opens one with the failure label", async () => {
    writeReport(report());
    const github = new FakeGitHub((call) => {
      if (call.path.startsWith("/actions/runs/")) return { status: 200, data: jobs };
      if (call.method === "GET") return { status: 200, data: [] };
      if (call.path === "/issues") return { status: 201, data: { number: 77 } };
      return { status: 201, data: {} };
    });
    const message = await notify(github, env({ GITHUB_EVENT_NAME: "schedule", NIGHTLY_RESULT: "failure" }));
    expect(message).toBe("opened issue #77");
    const create = github.calls.find((c) => c.method === "POST" && c.path === "/issues");
    const body = create?.body as { title: string; labels: string[]; type: string };
    expect(body.title).toBe(ISSUE_TITLE);
    expect(body.labels).toContain(FAILURE_LABEL);
    expect(body.type).toBe("Bug");
    expect(github.calls.some((c) => c.method === "POST" && c.path === "/labels")).toBe(true);
  });

  test("a failed night comments on the issue that is already open", async () => {
    writeReport(report());
    const github = new FakeGitHub((call) => {
      if (call.path.startsWith("/actions/runs/")) return { status: 200, data: jobs };
      if (call.method === "GET") return { status: 200, data: [{ number: 5 }, { number: 6, pull_request: {} }] };
      return { status: 201, data: {} };
    });
    expect(await notify(github, env({ GITHUB_EVENT_NAME: "schedule", NIGHTLY_RESULT: "failure" }))).toBe("commented on issue #5");
    expect(github.calls.some((c) => c.path === "/issues")).toBe(false);
  });

  test("a failure without any report still notifies, naming the failed steps", async () => {
    const github = new FakeGitHub((call) => {
      if (call.path.startsWith("/actions/runs/")) return { status: 200, data: jobs };
      if (call.method === "GET") return { status: 200, data: [] };
      return { status: 201, data: { number: 8 } };
    });
    await notify(github, env({ GITHUB_EVENT_NAME: "workflow_dispatch", NIGHTLY_RESULT: "failure", NIGHTLY_REPORT_DIR: join(dir, "absent") }));
    const create = github.calls.find((c) => c.method === "POST" && c.path === "/issues");
    expect((create?.body as { body: string }).body).toContain("Run the smoke test");
  });

  test("a passing night closes an open failure issue and does nothing otherwise", async () => {
    writeReport(report({ ok: true }));
    const open = new FakeGitHub((call) => ({ status: 200, data: call.method === "GET" ? [{ number: 5 }] : {} }));
    expect(await notify(open, env({ GITHUB_EVENT_NAME: "schedule", NIGHTLY_RESULT: "success" }))).toBe("closed issue #5");
    const close = open.calls.find((c) => c.method === "PATCH");
    expect(close).toEqual({ method: "PATCH", path: "/issues/5", body: { state: "closed", state_reason: "completed" } });

    const none = new FakeGitHub(() => ({ status: 200, data: [] }));
    expect(await notify(none, env({ GITHUB_EVENT_NAME: "schedule", NIGHTLY_RESULT: "success" }))).toBe("nothing to report");
    expect(none.calls.every((c) => c.method === "GET")).toBe(true);
  });
});
