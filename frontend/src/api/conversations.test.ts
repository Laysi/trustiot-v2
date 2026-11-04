import { describe, expect, test } from "bun:test";

import { createApiClient } from "./client.ts";
import { conversationApi } from "./conversations.ts";
import { ResponseShapeError } from "./errors.ts";

/** Answers every request with `body`. */
function server(body: unknown) {
  const fetch = async () =>
    new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
  return conversationApi(createApiClient({ fetch }));
}

const CONVERSATION = "01928f6e-7a3c-7d4e-8f00-0000000000c1";

const DETAIL = {
  id: CONVERSATION,
  title: "Line 3 vibration",
  created_at: "2026-11-02T08:00:00Z",
  last_activity_at: "2026-11-02T08:01:00Z",
  archived_at: null,
  messages: [
    { kind: "question", text: "Why does line 3 vibrate?" },
    { kind: "answer", text: "The bearing temperature rose first.", removed: ["code", "sql"] },
  ],
  latest_run: {
    id: "01928f6e-7a3c-7d4e-8f00-0000000000d1",
    status: "answered" as const,
    result: null,
    message_key: null,
    started_at: "2026-11-02T08:00:00Z",
    deadline_at: "2026-11-02T08:15:00Z",
    ended_at: "2026-11-02T08:01:00Z",
    stop_requested_at: null,
  },
};

describe("conversationApi", () => {
  test("a conversation keeps its latest run and what the guard removed from an answer", async () => {
    const detail = await server(DETAIL).getConversation(CONVERSATION);
    expect(detail.latest_run).toEqual(DETAIL.latest_run);
    expect(detail.messages[1]?.removed).toEqual(["code", "sql"]);
    expect(detail.messages[0]?.removed).toBeUndefined();
  });

  test("a conversation without a question yet has no latest run", async () => {
    const { latest_run: _, ...withoutRun } = DETAIL;
    const detail = await server(withoutRun).getConversation(CONVERSATION);
    expect(detail.latest_run).toBeUndefined();
  });

  test("a run status this page does not know is a shape error", async () => {
    const body = { ...DETAIL, latest_run: { ...DETAIL.latest_run, status: "paused" } };
    const error = await server(body)
      .getConversation(CONVERSATION)
      .catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ResponseShapeError);
    expect((error as ResponseShapeError).issues.map((issue) => issue.path)).toEqual(["/latest_run/status"]);
  });
});
