import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen } from "@testing-library/react";
import { type ReactNode, useState } from "react";
import { Outlet, type RouteObject } from "react-router";

import { ApiError } from "../../api/errors.ts";
import { CompletionApiProvider } from "../../api/pending/CompletionApiProvider.tsx";
import { ROUTES } from "../../app/routes.tsx";
import { FakeCompletionServer } from "../../test/fake-completion.ts";
import { renderAt } from "../../test/render.tsx";
import { until } from "../../test/until.ts";
import { Composer } from "../Composer.tsx";
import type { CompletionClock } from "./context.tsx";

afterEach(() => {
  cleanup();
});

/** The pause the tests configure; the product's default is 250 ms (J11 says 200 to 300). */
const DELAY_MS = 300;

/** A clock the test moves by hand: nothing here waits for real time. */
class ManualClock implements CompletionClock {
  time = 1_000_000;
  private next = 1;
  private timers = new Map<number, { at: number; callback: () => void }>();

  setTimeout(callback: () => void, ms: number): unknown {
    const handle = this.next++;
    this.timers.set(handle, { at: this.time + ms, callback });
    return handle;
  }

  clearTimeout(handle: unknown): void {
    this.timers.delete(handle as number);
  }

  now(): number {
    return this.time;
  }

  get pending(): number {
    return this.timers.size;
  }

  /** Moves time forward, running every timer that falls due, in order. */
  advance(ms: number): void {
    const end = this.time + ms;
    for (;;) {
      const due = [...this.timers.entries()]
        .filter(([, timer]) => timer.at <= end)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      this.timers.delete(due[0]);
      this.time = Math.max(this.time, due[1].at);
      due[1].callback();
    }
    this.time = end;
  }
}

function Harness({ paused = false, initial = "" }: { paused?: boolean; initial?: string }) {
  const [text, setText] = useState(initial);
  const [sent, setSent] = useState<string[]>([]);
  return (
    <>
      <Composer
        label="Ask"
        placeholder="Ask"
        rows={3}
        value={text}
        onChange={setText}
        onSubmit={() => {
          setSent((all) => [...all, text]);
          setText("");
        }}
        sending={false}
        answering={false}
        stopping={false}
        onStop={() => {}}
        completionPaused={paused}
      />
      <p data-testid="sent">{sent.join("|")}</p>
      <button type="button">next control</button>
    </>
  );
}

function withCompletion(server: FakeCompletionServer, clock: ManualClock, children: ReactNode) {
  return (
    <CompletionApiProvider api={server} delayMs={DELAY_MS} clock={clock}>
      {children}
    </CompletionApiProvider>
  );
}

function setup(options: { paused?: boolean; provided?: boolean; initial?: string } = {}) {
  const server = new FakeCompletionServer();
  const clock = new ManualClock();
  server.now = () => clock.now();
  const harness = <Harness paused={options.paused ?? false} initial={options.initial ?? ""} />;
  const routes: RouteObject[] = [
    { path: "/harness", element: options.provided === false ? harness : withCompletion(server, clock, harness) },
  ];
  const rendered = renderAt("/harness", { routes });
  return { server, clock, ...rendered };
}

const box = async () => (await screen.findByRole("textbox", { name: "Ask" })) as HTMLTextAreaElement;
const ghost = () => document.querySelector("[data-ghost]")?.textContent ?? null;

/** What the browser does for a typed character: the key goes down, then the text changes. */
async function type(input: HTMLTextAreaElement, text: string) {
  const key = text.slice(-1);
  act(() => {
    input.focus();
    fireEvent.keyDown(input, { key });
  });
  act(() => {
    fireEvent.change(input, { target: { value: text } });
    input.setSelectionRange(text.length, text.length);
  });
}

async function pause(clock: ManualClock, ms = DELAY_MS) {
  await act(async () => {
    clock.advance(ms);
  });
}

async function settle() {
  await act(async () => {
    await new Promise((resume) => setTimeout(resume, 0));
  });
}

async function suggested(clock: ManualClock) {
  await pause(clock);
  await until(() => ghost() !== null, "the grey text to appear");
}

describe("grey-text completion in the composer", () => {
  test("a build without the completion provider shows no grey text, sends nothing and leaves Tab alone", async () => {
    const { server, clock } = setup({ provided: false });
    const input = await box();
    await type(input, "How much energy did tower 2 use");
    await pause(clock, 60_000);
    expect(ghost()).toBeNull();
    expect(server.requests).toHaveLength(0);
    expect(screen.queryByRole("button", { name: /Accept suggestion/ })).toBeNull();
    expect(fireEvent.keyDown(input, { key: "Tab" })).toBe(true);
  });

  test("the request goes out only after the configured pause, then the suggestion is shown after the caret", async () => {
    const { server, clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    expect(clock.pending).toBe(1);
    await pause(clock, DELAY_MS - 1);
    expect(server.requests).toHaveLength(0);
    await pause(clock, 1);
    expect(server.requests.map((request) => request.text)).toEqual(["Why is cooling tower 2 offline"]);
    await until(() => ghost() !== null, "the grey text to appear");
    expect(ghost()).toBe(" and since when?");
    // The text itself is untouched, and the grey text is not read out (design decision 9).
    expect(input.value).toBe("Why is cooling tower 2 offline");
    expect(document.querySelector("[data-ghost]")?.closest("[aria-hidden='true']")).not.toBeNull();
  });

  test("each key restarts the pause, so only the text after the last key is sent", async () => {
    const { server, clock } = setup();
    const input = await box();
    await type(input, "Why is tower 2");
    await pause(clock, DELAY_MS - 1);
    await type(input, "Why is tower 2 o");
    await pause(clock, DELAY_MS - 1);
    expect(server.requests).toHaveLength(0);
    await pause(clock, 1);
    expect(server.requests.map((request) => request.text)).toEqual(["Why is tower 2 o"]);
  });

  test("no request for fewer than 4 characters, while an input method composes, or with the caret in the middle", async () => {
    const { server, clock } = setup();
    const input = await box();
    await type(input, "abc");
    await pause(clock);
    expect(server.requests).toHaveLength(0);

    act(() => {
      fireEvent.compositionStart(input);
    });
    await type(input, "冷卻水塔");
    await pause(clock);
    expect(server.requests).toHaveLength(0);
    act(() => {
      fireEvent.compositionEnd(input);
    });
    await pause(clock);
    expect(server.requests.map((request) => request.text)).toEqual(["冷卻水塔"]);

    await type(input, "冷卻水塔離線");
    act(() => {
      input.setSelectionRange(2, 2);
    });
    await pause(clock);
    expect(server.requests).toHaveLength(1);
  });

  test("the next key press aborts the request in flight and no stale suggestion appears", async () => {
    const { server, clock } = setup();
    server.hold();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await pause(clock);
    const [first] = server.requests;
    expect(first?.signal.aborted).toBe(false);

    act(() => {
      fireEvent.keyDown(input, { key: "x" });
    });
    expect(first?.signal.aborted).toBe(true);
    await act(async () => {
      server.release();
    });
    await settle();
    expect(ghost()).toBeNull();
  });

  test("an answer that arrives after the next key is dropped even when the service ignores the abort", async () => {
    const { server, clock } = setup();
    server.hold();
    server.honorAbort = false;
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await pause(clock);
    expect(server.requests).toHaveLength(1);

    await type(input, "Why is cooling tower 2 offline now");
    await act(async () => {
      server.release();
    });
    await settle();
    expect(ghost()).toBeNull();
    // The old answer was for other text; the new text asks again after its own pause.
    await pause(clock);
    expect(server.requests.map((request) => request.text)).toEqual([
      "Why is cooling tower 2 offline",
      "Why is cooling tower 2 offline now",
    ]);
  });

  test("a key that does not change the text, such as an arrow, also cancels and clears the suggestion", async () => {
    const { clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    act(() => {
      fireEvent.keyDown(input, { key: "ArrowLeft" });
    });
    expect(ghost()).toBeNull();
  });

  test("Shift, Control and the other modifier keys alone do not cancel", async () => {
    const { clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    act(() => {
      fireEvent.keyDown(input, { key: "Shift" });
    });
    expect(ghost()).toBe(" and since when?");
  });

  test("Tab accepts the suggestion in one insertion, and nothing more is asked until the user types", async () => {
    const { server, clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);

    let notPrevented = true;
    act(() => {
      notPrevented = fireEvent.keyDown(input, { key: "Tab" });
    });
    expect(notPrevented).toBe(false);
    expect(input.value).toBe("Why is cooling tower 2 offline and since when?");
    expect(ghost()).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("Suggestion accepted");
    await pause(clock, 60_000);
    expect(server.requests).toHaveLength(1);

    // Typing again asks again.
    await type(input, `${input.value} Also`);
    await pause(clock);
    expect(server.requests).toHaveLength(2);
  });

  test("Tab keeps moving the focus when no suggestion is shown, including while a request is in flight", async () => {
    const { server, clock } = setup();
    server.hold();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await pause(clock);
    expect(server.requests).toHaveLength(1);
    let notPrevented = false;
    act(() => {
      notPrevented = fireEvent.keyDown(input, { key: "Tab" });
    });
    expect(notPrevented).toBe(true);
    expect(server.requests[0]?.signal.aborted).toBe(true);
    expect(input.value).toBe("Why is cooling tower 2 offline");
  });

  test("Shift + Tab never accepts", async () => {
    const { clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    let notPrevented = false;
    act(() => {
      notPrevented = fireEvent.keyDown(input, { key: "Tab", shiftKey: true });
    });
    expect(notPrevented).toBe(true);
    expect(input.value).toBe("Why is cooling tower 2 offline");
  });

  test("Esc dismisses the suggestion and asks nothing until the text changes", async () => {
    const { server, clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    let notPrevented = true;
    act(() => {
      notPrevented = fireEvent.keyDown(input, { key: "Escape" });
    });
    expect(notPrevented).toBe(false);
    expect(ghost()).toBeNull();
    await pause(clock, 60_000);
    expect(server.requests).toHaveLength(1);
    await type(input, "Why is cooling tower 2 offline?!");
    await pause(clock);
    expect(server.requests).toHaveLength(2);
  });

  test("an open mention list or template slot owns Tab: grey text is hidden and Tab does not accept it", async () => {
    // The composer's `completionPaused` is what the mention hook-up sets (Laysi/trustiot-v2#155).
    const server = new FakeCompletionServer();
    const clock = new ManualClock();
    function Switch() {
      const [paused, setPaused] = useState(false);
      return (
        <>
          <button type="button" onClick={() => setPaused((was) => !was)}>
            toggle mention list
          </button>
          <Harness paused={paused} />
        </>
      );
    }
    renderAt("/harness", { routes: [{ path: "/harness", element: withCompletion(server, clock, <Switch />) }] });
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    expect(ghost()).not.toBeNull();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "toggle mention list" }));
    });
    expect(ghost()).toBeNull();
    await pause(clock, 60_000);
    expect(server.requests).toHaveLength(1);
    let notPrevented = false;
    act(() => {
      input.focus();
      notPrevented = fireEvent.keyDown(input, { key: "Tab" });
    });
    expect(notPrevented).toBe(true);
    expect(input.value).toBe("Why is cooling tower 2 offline");

    // A key the mention list already used (it called preventDefault) is not taken again.
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "toggle mention list" }));
    });
    await pause(clock);
    await until(() => ghost() !== null, "the grey text to come back");
    const used = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
    used.preventDefault();
    act(() => {
      input.dispatchEvent(used);
    });
    expect(input.value).toBe("Why is cooling tower 2 offline");
  });

  test("the accept button for touch screens accepts and names the suggestion", async () => {
    const { clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    const button = screen.getByRole("button", { name: "Accept suggestion: and since when?" });
    // Pressing it must not take the focus out of the box, or the blur would clear the suggestion first.
    expect(fireEvent.mouseDown(button)).toBe(false);
    act(() => {
      fireEvent.click(button);
    });
    expect(input.value).toBe("Why is cooling tower 2 offline and since when?");
    expect(screen.queryByRole("button", { name: /Accept suggestion/ })).toBeNull();
  });

  test("moving the caret away from the end, or leaving the box, clears the suggestion", async () => {
    const { clock } = setup();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    act(() => {
      input.setSelectionRange(3, 3);
      fireEvent.click(input);
    });
    expect(ghost()).toBeNull();

    await type(input, "Why is cooling tower 3 offline");
    await suggested(clock);
    act(() => {
      fireEvent.blur(input);
    });
    expect(ghost()).toBeNull();
  });

  test("sending the question aborts the request in flight", async () => {
    const { server, clock } = setup();
    server.hold();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await pause(clock);
    act(() => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    expect(server.requests[0]?.signal.aborted).toBe(true);
    expect(screen.getByTestId("sent").textContent).toBe("Why is cooling tower 2 offline");
  });

  test("a failed completion shows nothing: no alert, no grey text", async () => {
    const { server, clock } = setup();
    server.failNext();
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await pause(clock);
    await settle();
    expect(server.requests).toHaveLength(1);
    expect(ghost()).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByRole("status").textContent).toBe("");
  });

  test("while the per-user limit applies no request is sent until the retry time has passed", async () => {
    const { server, clock } = setup();
    server.limit = { requests: 1, windowSecs: 90 };
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    await type(input, "Why is cooling tower 2 offline now");
    await pause(clock);
    // The second request is the one that is limited: 429 with retry_after_secs, shown once.
    await until(() => screen.queryByRole("status")?.textContent?.includes("paused") ?? false, "the paused note");
    expect(server.requests).toHaveLength(2);
    expect(screen.getByRole("status").textContent).toBe("Suggestions paused, back in about 2 min");

    // More typing during the pause sends nothing.
    await type(input, "Why is cooling tower 2 offline now?");
    await pause(clock);
    await type(input, "Why is cooling tower 2 offline now? And");
    await pause(clock, 30_000);
    expect(server.requests).toHaveLength(2);

    // Just before the retry time: still nothing. After it: the request goes out and is answered.
    await pause(clock, 59_000);
    await type(input, "Why is cooling tower 2 offline now? And tower 3");
    await pause(clock);
    expect(server.requests).toHaveLength(2);
    await pause(clock, 60_000);
    expect(screen.getByRole("status").textContent).toBe("");
    await type(input, "Why is cooling tower 2 offline now? And tower 3 offline");
    await pause(clock);
    expect(server.requests).toHaveLength(3);
    await until(() => ghost() !== null, "the grey text after the pause");
  });

  test("a 429 without a retry time pauses for a minute", async () => {
    const { server, clock } = setup();
    server.complete = async (request, options) => {
      server.requests.push({ text: request.text, signal: options.signal });
      throw new ApiError(429, null);
    };
    const input = await box();
    await type(input, "Why is cooling tower 2 offline");
    await pause(clock);
    await until(() => screen.queryByRole("status")?.textContent?.includes("paused") ?? false, "the paused note");
    expect(screen.getByRole("status").textContent).toBe("Suggestions paused, back in about 1 min");
    await type(input, "Why is cooling tower 2 offline now");
    await pause(clock, 59_000);
    expect(server.requests).toHaveLength(1);
  });
});

describe("the home page's composer", () => {
  test("shows grey text under the completion provider, which is how the UI preview turns it on", async () => {
    const server = new FakeCompletionServer();
    const clock = new ManualClock();
    const routes: RouteObject[] = [{ element: withCompletion(server, clock, <Outlet />), children: ROUTES }];
    renderAt("/", { routes });
    const input = (await screen.findByRole("textbox", { name: "Ask TrustIoT" })) as HTMLTextAreaElement;
    await type(input, "Why is cooling tower 2 offline");
    await suggested(clock);
    expect(ghost()).toBe(" and since when?");
    expect(input.getAttribute("aria-describedby")).not.toBeNull();
    const hint = document.getElementById(input.getAttribute("aria-describedby") ?? "");
    expect(hint?.textContent).toContain("Tab accepts one, Esc dismisses it");
  });

  test("shows none in the product build, which has no provider", async () => {
    renderAt("/");
    const input = (await screen.findByRole("textbox", { name: "Ask TrustIoT" })) as HTMLTextAreaElement;
    await type(input, "Why is cooling tower 2 offline");
    await settle();
    expect(ghost()).toBeNull();
    const hint = document.getElementById(input.getAttribute("aria-describedby") ?? "");
    expect(hint?.textContent).not.toContain("Tab");
  });
});
