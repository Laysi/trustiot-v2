import type { Element, ElementContent, Properties, Root, RootContent, Text } from "hast";

import { spaPath } from "../app/paths.ts";

// The assistant's answers are text the model wrote, and a model that read a prompt injection can
// write anything into them. Markdown is then a way out for the tenant's data: an image whose
// address carries the data is fetched the moment the page draws it, and a link that carries it
// needs one click (docs/architecture/ai-assistant.md, 「安全」 point 5). So an answer is drawn by
// these rules, whatever the Markdown says:
//
// - Only text, headings, lists, tables, quotes and code: every other element is replaced by its
//   content (`ALLOWED_ELEMENTS`), and no element keeps an attribute this file does not set.
// - No image element at all, of any address: an image is its alternative text.
// - A link is a link only when it is a path of this SPA (`spaPath`, the check the sign-in page's
//   `next` goes through); any other address is plain text.
// - Raw HTML is plain text: it is never parsed into elements.
//
// The rules are applied to the HTML tree (hast) after the Markdown is parsed, so they see the
// address a browser would get: entities and backslash escapes already decoded, reference-style
// links and images already resolved, the address percent-encoded (mdast-util-to-hast's
// `normalizeUri`). react-markdown then checks the result once more (`urlTransform`,
// `allowedElements`), and the components draw a link only for an SPA path (MarkdownText.tsx).

/** The elements an answer may have; every other element is replaced by its content. */
export const ALLOWED_ELEMENTS: readonly string[] = [
  "p",
  "br",
  "hr",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "ul",
  "ol",
  "li",
  "blockquote",
  "pre",
  "code",
  "em",
  "strong",
  "del",
  "table",
  "thead",
  "tbody",
  "tr",
  "th",
  "td",
  "a",
  "span",
];

/** The class of the text that stands for an image. */
export const IMAGE_TEXT_CLASS = "answer-image-text";

/** A code block's language, as remark writes it (`language-ts`); anything else is dropped. */
const LANGUAGE_CLASS = /^language-[A-Za-z0-9_+#.-]{1,40}$/;

/** A table cell's alignment as a class: the `align` attribute would become an inline style. */
const ALIGN_CLASSES: Record<string, string> = { left: "text-left", center: "text-center", right: "text-right" };

/** An address scheme with its slashes (`https://`, `mailto:`): an autolink's text has none. */
const SCHEME = /^[a-z][a-z0-9+.-]*:(?:\/\/)?/i;

export interface SafeAnswerOptions {
  /** The words that stand in for an image, given its alternative text (`""` when it has none). */
  imageText(alt: string): string;
}

const text = (value: string): Text => ({ type: "text", value });

function textContent(node: ElementContent | RootContent): string {
  if (node.type === "text") return node.value;
  if (node.type === "element") return node.children.map(textContent).join("");
  return "";
}

/** The attributes an allowed element keeps: nothing but a code language, a cell's alignment, a list's start. */
function safeProperties(element: Element): Properties {
  const { tagName, properties } = element;
  if (tagName === "code") {
    const classes = Array.isArray(properties.className) ? properties.className : [];
    const language = classes.find((name) => typeof name === "string" && LANGUAGE_CLASS.test(name));
    return language === undefined ? {} : { className: [String(language)] };
  }
  if (tagName === "th" || tagName === "td") {
    const align = typeof properties.align === "string" ? ALIGN_CLASSES[properties.align] : undefined;
    return align === undefined ? {} : { className: [align] };
  }
  if (tagName === "ol") {
    const start = properties.start;
    return typeof start === "number" && Number.isSafeInteger(start) && start >= 0 ? { start } : {};
  }
  return {};
}

/** A link: an SPA path stays a link (and only its path), any other address becomes plain text. */
function safeLink(element: Element, children: ElementContent[]): ElementContent[] {
  const href = typeof element.properties.href === "string" ? element.properties.href : "";
  const path = spaPath(href);
  if (path !== null) {
    return [{ type: "element", tagName: "a", properties: { href: path }, children }];
  }
  // The address is shown after the words, as text, unless the words are the address already (an
  // autolink) or it points inside the answer (a footnote's `#…`, which has nothing to point at).
  const words = children.map(textContent).join("").trim();
  const shown = href === "" || href.startsWith("#") || words === href || words === href.replace(SCHEME, "");
  return shown ? children : [...children, text(` (${href})`)];
}

function safeChildren(nodes: readonly (ElementContent | RootContent)[], options: SafeAnswerOptions): ElementContent[] {
  const out: ElementContent[] = [];
  for (const node of nodes) {
    switch (node.type) {
      case "text":
        out.push(text(node.value));
        break;
      // Raw HTML (`<img>`, `<a>`, `<iframe>`, `<script>`…) is shown as the text it is.
      case "raw":
        out.push(text(node.value));
        break;
      case "element": {
        if (node.tagName === "img") {
          const alt = typeof node.properties.alt === "string" ? node.properties.alt : "";
          out.push({
            type: "element",
            tagName: "span",
            properties: { className: [IMAGE_TEXT_CLASS] },
            children: [text(options.imageText(alt))],
          });
          break;
        }
        const children = safeChildren(node.children, options);
        if (node.tagName === "a") {
          out.push(...safeLink(node, children));
        } else if (ALLOWED_ELEMENTS.includes(node.tagName) && node.tagName !== "span") {
          out.push({ type: "element", tagName: node.tagName, properties: safeProperties(node), children });
        } else {
          // Footnote sections, task-list checkboxes, superscripts, a `span` from anywhere else:
          // only their content.
          out.push(...children);
        }
        break;
      }
      // Comments and doctypes are not shown.
      default:
        break;
    }
  }
  return out;
}

/** The rehype plugin that applies the rules above to an answer's tree. */
export function rehypeSafeAnswer(options: SafeAnswerOptions) {
  return (tree: Root): Root => ({ type: "root", children: safeChildren(tree.children, options) });
}

/** react-markdown's last check of every address it sees: an SPA path, or nothing. */
export function answerUrlTransform(url: string): string {
  return spaPath(url) ?? "";
}
