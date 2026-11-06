import { afterEach, describe, expect, setDefaultTimeout, test } from "bun:test";
import { cleanup, screen } from "@testing-library/react";

import { conversationPath } from "../app/paths.ts";
import { FakeQuestionServer } from "./fake-questions.ts";
import { NODE_PRINT_LIMIT, printNode } from "./print-node.ts";
import { renderAt } from "./render.tsx";

// The first render loads the conversation page lazily; on a loaded machine that takes a while.
setDefaultTimeout(90_000);

afterEach(() => {
  cleanup();
});

/** The message of the error `assert` throws. */
function failure(assert: () => void): string {
  try {
    assert();
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
  throw new Error("the assertion passed");
}

describe("a DOM node in a failed expect (Laysi/trustiot-v2#1138)", () => {
  test("a rendered element prints as its markup, not as the app it reaches", async () => {
    const server = new FakeQuestionServer();
    const conversation = server.add({
      title: "Energy this week",
      messages: [
        { kind: "question", text: "How did floor 3 use energy this week?" },
        { kind: "answer", text: "Floor 3 used 412 kWh." },
      ],
    });
    renderAt(conversationPath(conversation.id), { conversations: server.api() });
    const article = await screen.findByRole("article", { name: "TrustIoT's answer" }, { timeout: 60_000 });

    const message = failure(() => expect(article).toBeNull());
    // Before, this message was about 730 million characters (the window, the document and React's fibers).
    expect(message.length).toBeLessThan(NODE_PRINT_LIMIT + 1_000);
    expect(message).toContain("<article");
    expect(message).toContain("Floor 3 used 412 kWh.");
  });

  test("long markup is cut and says how much was left out; other nodes print their name and text", () => {
    const element = document.createElement("p");
    element.textContent = "x".repeat(NODE_PRINT_LIMIT * 2);
    const printed = printNode(element);
    expect(printed.startsWith("<p>xxx")).toBe(true);
    expect(printed.endsWith(`… (${"<p></p>".length + NODE_PRINT_LIMIT} more characters)`)).toBe(true);
    expect(printNode(document.createTextNode("kWh"))).toBe('#text "kWh"');
    expect(Bun.inspect(document.createElement("hr"))).toBe("<hr>");
  });
});
