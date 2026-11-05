import { afterEach, beforeEach, describe, expect, setDefaultTimeout, test } from "bun:test";
import { act, cleanup, fireEvent, screen } from "@testing-library/react";
import { useState } from "react";

import type { DeviceApi } from "../../api/devices.ts";
import { ApiError } from "../../api/errors.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { FakeQuestionTemplates } from "../../test/fake-question-templates.ts";
import { renderAt } from "../../test/render.tsx";
import { until } from "../../test/until.ts";
import { Composer } from "../Composer.tsx";
import { CompletionConfigContext } from "./config.ts";
import { TemplateSourceProvider } from "./templates.tsx";
import type { SubmittedMention } from "./useCompletion.ts";

// Lookups and re-renders on a loaded machine: each test waits for conditions (`until`, 20 s) and is bounded generously.
setDefaultTimeout(90_000);

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});
beforeEach(() => {
  window.localStorage.clear();
});

// The clock is a Sunday, so "last week" is 10/19 to 10/25 (design 05-B). The delays are short: the
// tests wait for what happened, never for a time to pass.
const NOW = Date.parse("2026-11-01T10:00:00Z");
const CONFIG = { skeletonDelayMs: 20, announceDelayMs: 20, now: () => NOW };

interface Sent {
  text: string;
  mentions: SubmittedMention[];
}

/** The composer as a page holds it: it owns the text and says what was sent. */
function Harness({
  sent,
  templates,
  config = CONFIG,
}: {
  sent: Sent[];
  templates?: FakeQuestionTemplates;
  config?: typeof CONFIG;
}) {
  const { session } = useSession();
  const [text, setText] = useState("");
  if (!session) return null;
  const composer = (
    <Composer
      label="Ask"
      placeholder=""
      rows={3}
      value={text}
      onChange={setText}
      onSubmit={({ mentions }) => sent.push({ text, mentions })}
      sending={false}
      answering={false}
      stopping={false}
      onStop={() => {}}
    />
  );
  return (
    <CompletionConfigContext.Provider value={config}>
      {templates ? <TemplateSourceProvider source={templates.api()}>{composer}</TemplateSourceProvider> : composer}
    </CompletionConfigContext.Provider>
  );
}

/** A tenant with two compressors (one model with two data points) and a cooling tower. */
function plant(): FakeDeviceServer {
  const server = new FakeDeviceServer();
  const first = server.addDevice({
    name: "Compressor A1",
    modelName: "Screw compressor",
    points: [
      { key: "pressure", data_type: "f32", description: "Discharge pressure" },
      { key: "energy", data_type: "f64" },
    ],
    pollIntervalMs: 10_000,
  });
  server.addDevice({ name: "Compressor A2", modelId: first.model_id, pollIntervalMs: 10_000 });
  server.addDevice({ name: "Cooling tower C1", modelName: "Pump drive", points: [{ key: "speed", data_type: "u16" }] });
  return server;
}

function show(options: {
  devices?: DeviceApi;
  templates?: FakeQuestionTemplates;
  config?: typeof CONFIG;
  sent?: Sent[];
}) {
  const sent = options.sent ?? [];
  const view = renderAt("/", {
    routes: [
      { path: "/", element: <Harness sent={sent} templates={options.templates} config={options.config ?? CONFIG} /> },
    ],
    devices: options.devices ?? plant().api(),
  });
  return { ...view, sent };
}

const box = () => screen.getByRole("textbox", { name: "Ask" }) as HTMLTextAreaElement;
const optionTexts = () => [...document.querySelectorAll('[role="option"]')].map((option) => option.textContent ?? "");
const optionNames = () =>
  [...document.querySelectorAll('[role="option"]')].map(
    (option) => option.querySelector("span.truncate")?.textContent ?? "",
  );
const selectedName = () =>
  document.querySelector('[role="option"][aria-selected="true"] span.truncate')?.textContent ?? null;
const statusTexts = () => [...document.querySelectorAll('[role="status"]')].map((node) => node.textContent ?? "");
const listOpen = () => document.querySelector('[role="listbox"]:not(.hidden)') !== null && optionTexts().length > 0;

async function type(text: string, caret = text.length) {
  const input = await screen.findByRole("textbox", { name: "Ask" });
  act(() => {
    fireEvent.change(input, { target: { value: text } });
    (input as HTMLTextAreaElement).setSelectionRange(caret, caret);
  });
}

const press = (key: string, init: { shiftKey?: boolean; keyCode?: number; isComposing?: boolean } = {}) => {
  let notPrevented = true;
  act(() => {
    notPrevented = fireEvent.keyDown(box(), { key, ...init });
  });
  return !notPrevented;
};

/** A device API whose lists wait for `release()`, to see what the list shows while a lookup runs. */
function held(server: FakeDeviceServer): { api: DeviceApi; release: () => void } {
  let release = () => {};
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const real = server.api();
  return {
    release,
    api: {
      ...real,
      listDevices: async (signal) => {
        await gate;
        return real.listDevices(signal);
      },
    },
  };
}

describe("the list after an @ (Laysi/trustiot-v2#155, acceptance: it lists within the configured delay)", () => {
  test("it opens at once with the time ranges, shows placeholders only after the configured delay, and lists the devices when they arrive", async () => {
    const gate = held(plant());
    show({ devices: gate.api });
    const events: string[] = [];
    await type("@");
    // Nothing has been awaited: the list is open and the time ranges are on it.
    expect(optionNames()).toContain("Last week");
    events.push("list open");
    expect(document.body.textContent).not.toContain("Looking up");
    await until(() => document.body.textContent?.includes("Looking up") ?? false, "the placeholder rows");
    events.push("placeholders");
    expect(optionNames().some((name) => name.startsWith("Compressor"))).toBe(false);
    act(() => gate.release());
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    events.push("devices");
    expect(document.body.textContent).not.toContain("Looking up");
    expect(events).toEqual(["list open", "placeholders", "devices"]);
  });

  test("a lookup that answers before the delay never shows the placeholders", async () => {
    const config = { ...CONFIG, skeletonDelayMs: 600_000 };
    show({ config });
    const sawPlaceholders: boolean[] = [];
    const observer = new MutationObserver(() =>
      sawPlaceholders.push(document.body.textContent?.includes("Looking up") ?? false),
    );
    observer.observe(document.body, { childList: true, subtree: true, characterData: true });
    await type("@");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    observer.disconnect();
    expect(sawPlaceholders.some(Boolean)).toBe(false);
  });

  test("a word narrows the list to what matches, the first match is selected and its matching part is marked", async () => {
    show({});
    await type("@comp");
    await until(() => optionNames().includes("Compressor A1"), "the matching devices");
    await until(() => optionNames().includes("Screw compressor"), "the data points and the model");
    expect(optionNames()).toEqual([
      "Compressor A1",
      "Compressor A2",
      "Compressor A1 · Discharge pressure",
      "Compressor A1 · energy",
      "Compressor A2 · Discharge pressure",
      "Compressor A2 · energy",
      "Screw compressor",
    ]);
    expect(optionNames()).not.toContain("Cooling tower C1");
    expect(selectedName()).toBe("Compressor A1");
    const marked = [...document.querySelectorAll('[role="option"] mark')].map((mark) => mark.textContent);
    expect(marked.every((text) => text?.toLowerCase() === "comp")).toBe(true);
    expect(marked.length).toBeGreaterThan(0);
  });

  test("a data point is listed under its device, found by its key, and a model by its name with its device count", async () => {
    show({});
    await type("@energy");
    await until(() => optionNames().includes("Compressor A1 · energy"), "the data points");
    expect(optionTexts().find((text) => text.startsWith("Compressor A2 · energy"))).toContain("energy · every 10 s");
    await type("@screw");
    await until(() => optionNames().includes("Screw compressor"), "the model");
    expect(optionTexts().find((text) => text.startsWith("Screw compressor"))).toContain("2 devices · 2 data points");
  });

  test("a time range shows its real dates in the user's time zone", async () => {
    show({});
    await type("@week");
    const row = optionTexts().find((text) => text.startsWith("Last week"));
    expect(row).toContain("10/19 (Mon) – 10/25 (Sun)");
  });
});

describe("choosing from the list", () => {
  test("ArrowDown and ArrowUp move the selection and wrap round; the box names the selected option", async () => {
    show({});
    await type("@comp");
    await until(() => optionNames().includes("Screw compressor"), "every group");
    const active = () => box().getAttribute("aria-activedescendant");
    expect(active()).toBe(document.querySelector('[role="option"][aria-selected="true"]')?.id ?? null);
    expect(box().getAttribute("aria-controls")).toBe(document.querySelector('[role="listbox"]')?.id ?? null);
    press("ArrowDown");
    expect(selectedName()).toBe("Compressor A2");
    expect(active()).toBe(document.querySelector('[role="option"][aria-selected="true"]')?.id ?? null);
    press("ArrowUp");
    press("ArrowUp");
    expect(selectedName()).toBe(optionNames().at(-1) ?? null);
  });

  test("Enter inserts the mention with a space after it, closes the list and does not send", async () => {
    const { sent } = show({});
    await type("@comp");
    await until(() => optionNames().includes("Compressor A2"), "the devices");
    press("ArrowDown");
    expect(press("Enter")).toBe(true);
    expect(box().value).toBe("@Compressor A2 ");
    expect(box().selectionStart).toBe("@Compressor A2 ".length);
    expect(listOpen()).toBe(false);
    expect(box().getAttribute("aria-controls")).toBeNull();
    expect(sent).toEqual([]);
  });

  test("Tab inserts too, and a mouse click on an option does", async () => {
    show({});
    await type("compare @comp");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    press("Tab");
    expect(box().value).toBe("compare @Compressor A1 ");
    await type("compare @Compressor A1 and @cool");
    await until(() => optionNames().includes("Cooling tower C1"), "the tower");
    const option = [...document.querySelectorAll('[role="option"]')].find((node) =>
      node.textContent?.startsWith("Cooling tower"),
    );
    act(() => {
      fireEvent.click(option as Element);
    });
    expect(box().value).toBe("compare @Compressor A1 and @Cooling tower C1 ");
  });

  test("the question goes out with what each mention stands for, not just its name", async () => {
    const server = plant();
    const { sent } = show({ devices: server.api() });
    await type("@Compressor");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    press("Enter");
    await type("@Compressor A1 vs @week");
    await until(() => optionNames().includes("Last week"), "the ranges");
    press("Enter");
    await type(`${box().value}energy?`, `${box().value}energy?`.length);
    expect(box().value).toBe("@Compressor A1 vs @Last week energy?");
    const [compressor] = [...server.devices.values()];
    act(() => {
      fireEvent.keyDown(box(), { key: "Enter" });
    });
    expect(sent).toHaveLength(1);
    expect(sent[0]?.mentions.map((mention) => mention.label)).toEqual(["@Compressor A1", "@Last week"]);
    expect(sent[0]?.mentions[0]?.target).toEqual({ kind: "device", deviceId: compressor?.device_id ?? "" });
    expect(sent[0]?.mentions[1]?.target).toEqual({
      kind: "range",
      id: "last_week",
      from: "2026-10-19T00:00:00.000Z",
      to: "2026-10-26T00:00:00.000Z",
    });
    for (const mention of sent[0]?.mentions ?? []) {
      expect(sent[0]?.text.slice(mention.start, mention.end)).toBe(mention.label);
    }
  });

  test("an edit inside a mention turns it back into plain text: it is no longer sent as a device", async () => {
    const { sent } = show({});
    await type("@Compressor");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    press("Enter");
    // Text typed before it moves it along; a letter changed inside it takes it away.
    await type("Hi @Compressor A1 ");
    await type("Hi @Compresor A1 ");
    act(() => {
      fireEvent.keyDown(box(), { key: "Enter" });
    });
    expect(sent[0]?.mentions).toEqual([]);
  });

  test("Escape closes the list and the same @ does not open it again; a new @ does", async () => {
    show({});
    await type("@com");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    expect(press("Escape")).toBe(true);
    expect(listOpen()).toBe(false);
    await type("@comp");
    expect(listOpen()).toBe(false);
    await type("@comp and @");
    await until(listOpen, "the list for the new @");
  });

  test("while an input method composes, Enter and the arrows are its own: nothing is inserted or sent", async () => {
    const { sent } = show({});
    await type("@comp");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    const before = selectedName();
    expect(press("ArrowDown", { keyCode: 229 })).toBe(false);
    expect(press("Enter", { keyCode: 229 })).toBe(false);
    expect(press("Enter", { isComposing: true })).toBe(false);
    expect(selectedName()).toBe(before);
    expect(box().value).toBe("@comp");
    expect(sent).toEqual([]);
  });

  test("with no match the list stays open and says what was not found; Enter inserts nothing and sends nothing", async () => {
    const { sent } = show({});
    await type("@freezer");
    await until(
      () => statusTexts().some((text) => text.includes("Nothing found for “freezer”")),
      "the no-result message",
    );
    expect(document.body.textContent).toContain("Acme Factory has no device, data point or model");
    expect(press("Enter")).toBe(true);
    expect(box().value).toBe("@freezer");
    expect(sent).toEqual([]);
  });

  test("the kind buttons narrow the list to one kind, and say which is on", async () => {
    show({});
    await type("@");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    const filter = (name: string) => screen.getByRole("button", { name }) as HTMLButtonElement;
    expect(filter("All").getAttribute("aria-pressed")).toBe("true");
    act(() => {
      fireEvent.click(filter("Time"));
    });
    expect(filter("Time").getAttribute("aria-pressed")).toBe("true");
    expect(optionNames()).toContain("Last quarter");
    expect(optionNames().some((name) => name.startsWith("Compressor"))).toBe(false);
  });

  test("a failed lookup is said in the list, the time ranges can still be picked, and Try again asks again", async () => {
    const server = plant();
    server.failNext("devices", fakeError(503, "unavailable", "error.unavailable"));
    show({ devices: server.api() });
    await type("@");
    await until(() => document.querySelector('[role="alert"]') !== null, "the failure message");
    expect(document.querySelector('[role="alert"]')?.textContent).toContain("The lists could not be looked up");
    expect(optionNames()).toContain("Last week");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    });
    await until(() => optionNames().includes("Compressor A1"), "the devices after the retry");
    expect(document.querySelector('[role="alert"]')).toBeNull();
  });

  test("the last items mentioned come first on a bare @", async () => {
    show({});
    await type("@cool");
    await until(() => optionNames().includes("Cooling tower C1"), "the tower");
    press("Enter");
    await type("");
    await type("@");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    expect(document.querySelector('[role="group"][aria-labelledby]')?.textContent).toContain("Recently used");
    expect(optionNames()[0]).toBe("Cooling tower C1");
    expect(optionNames().filter((name) => name === "Cooling tower C1")).toHaveLength(1);
  });

  test("leaving the box closes the list", async () => {
    show({});
    await type("@com");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
    act(() => {
      fireEvent.blur(box());
    });
    expect(listOpen()).toBe(false);
  });

  test("the @ button opens the list at the caret, with a space before it when a word is there", async () => {
    show({});
    await type("compare");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: /^Mention a device/ }));
    });
    expect(box().value).toBe("compare @");
    await until(() => optionNames().includes("Compressor A1"), "the devices");
  });
});

describe("question templates", () => {
  const templateRows = () => optionNames();

  test("a build without templates has no template button and a / opens nothing", async () => {
    show({});
    await type("/");
    expect(screen.queryByRole("button", { name: /Question templates/ })).toBeNull();
    expect(listOpen()).toBe(false);
  });

  test("a / at the start lists the templates by kind of question, with the tool and an example; a word narrows them", async () => {
    const templates = new FakeQuestionTemplates();
    show({ templates });
    await type("/");
    await until(() => templateRows().length > 0, "the templates");
    expect(document.body.textContent).toContain("Energy use");
    expect(document.body.textContent).toContain("Anomalies and lost links");
    expect(optionTexts()[0]).toContain("Energy tool · e.g. Energy used by Compressor A2 last week");
    expect(templates.calls).toEqual(["en"]);
    await type("/stretch");
    await until(() => templateRows().length === 1, "the narrowed templates");
    expect(templateRows()[0]).toContain("Anomalous stretches");
  });

  test("the template button opens the same list, and writes the / into an empty box", async () => {
    show({ templates: new FakeQuestionTemplates() });
    const button = await screen.findByRole("button", { name: /Question templates/ });
    act(() => {
      fireEvent.click(button);
    });
    expect(box().value).toBe("/");
    await until(() => templateRows().length > 0, "the templates");
  });

  test("a template is filled blank by blank: the first blank is selected and lists only devices, then the next lists time ranges", async () => {
    const { sent } = show({ templates: new FakeQuestionTemplates() });
    await type("/");
    await until(() => templateRows().length > 0, "the templates");
    press("Enter");
    const text = "Energy used by ［Device］ in ［Time range］";
    expect(box().value).toBe(text);
    expect([box().selectionStart, box().selectionEnd]).toEqual([
      text.indexOf("［Device］"),
      text.indexOf("［Device］") + "［Device］".length,
    ]);
    await until(() => optionNames().includes("Compressor A1"), "the devices for the blank");
    expect(optionNames().some((name) => name === "Last week")).toBe(false);
    expect(document.body.textContent).toContain("Fill in ［Device］ · blanks left: 2");
    // With blanks left Enter would send a half-written question: it does not, and says why.
    expect(screen.getByText("Blanks left to fill: 2")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    press("ArrowDown");
    press("Enter");
    expect(box().value).toBe("Energy used by @Compressor A2 in ［Time range］");
    expect(box().value.slice(box().selectionStart, box().selectionEnd)).toBe("［Time range］");
    expect(optionNames()).toContain("Last week");
    expect(screen.getByText("Blanks left to fill: 1")).toBeTruthy();
    press("Enter");
    expect(box().value).toBe("Energy used by @Compressor A2 in @Today ");
    expect(screen.queryByText(/Blanks left to fill/)).toBeNull();
    expect(listOpen()).toBe(false);
    act(() => {
      fireEvent.keyDown(box(), { key: "Enter" });
    });
    expect(sent).toHaveLength(1);
    expect(sent[0]?.mentions.map((mention) => mention.label)).toEqual(["@Compressor A2", "@Today"]);
  });

  test("Tab moves to the next blank and Shift+Tab back; after the last blank Tab leaves the box as it always does", async () => {
    show({ templates: new FakeQuestionTemplates() });
    await type("/");
    await until(() => templateRows().length > 0, "the templates");
    press("Enter");
    const selected = () => box().value.slice(box().selectionStart, box().selectionEnd);
    expect(selected()).toBe("［Device］");
    expect(press("Tab")).toBe(true);
    expect(selected()).toBe("［Time range］");
    await until(() => optionNames().includes("Last week"), "the ranges for the second blank");
    expect(press("Tab")).toBe(false);
    expect(press("Tab", { shiftKey: true })).toBe(true);
    expect(selected()).toBe("［Device］");
    expect(press("Tab", { shiftKey: true })).toBe(false);
  });

  test("a blank can be deleted or written over: it stops counting", async () => {
    show({ templates: new FakeQuestionTemplates() });
    await type("/");
    await until(() => templateRows().length > 0, "the templates");
    press("Enter");
    await type("Energy used by my pump in ［Time range］");
    expect(screen.getByText("Blanks left to fill: 1")).toBeTruthy();
    await type("Energy used by my pump in last week");
    expect(screen.queryByText(/Blanks left to fill/)).toBeNull();
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(false);
  });

  test("a template that fails to load says so in the list, and the box keeps working", async () => {
    const templates = new FakeQuestionTemplates();
    templates.failNext(new ApiError(503, { code: "unavailable", message_key: "error.unavailable" }));
    show({ templates });
    await type("/");
    await until(() => document.querySelector('[role="alert"]') !== null, "the failure message");
    expect(document.querySelector('[role="alert"]')?.textContent).toContain("The templates could not be loaded");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    });
    await until(() => templateRows().length > 0, "the templates after the retry");
  });
});
