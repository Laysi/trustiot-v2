import type { ConversationMessage } from "../api/conversations.ts";

/** The longest file name this makes, in characters before `.md`. */
const MAX_NAME = 80;

/** Names Windows refuses for a file, whatever the extension: `con`, `con.txt` and `COM¹.log` alike. */
const RESERVED_NAME = /^(con|prn|aux|nul|com[0-9¹²³]|lpt[0-9¹²³])(\.|$)/iu;

/**
 * A conversation as a Markdown file: its title as the heading, then each question and answer in
 * order, under a heading that says which it is. Every text is the API's, unchanged: nothing is
 * rendered, escaped or turned into HTML, so the file reads as the conversation did.
 */
export function conversationMarkdown(
  title: string,
  messages: readonly ConversationMessage[],
  labels: { question: string; answer: string },
): string {
  const parts = [`# ${title}`];
  for (const message of messages) {
    parts.push(`## ${message.kind === "question" ? labels.question : labels.answer}`, message.text);
  }
  return `${parts.join("\n\n")}\n`;
}

/**
 * A file name for `title` that every system takes: no path separators, no characters Windows
 * refuses (`<>:"/\|?*`), no control, format or text-direction characters (which could make the
 * name read as another extension), no leading or trailing dots and spaces, at most 80
 * characters; `fallback` when nothing is left. It always ends in `.md`.
 */
export function markdownFileName(title: string, fallback: string): string {
  const cleaned = title
    .normalize("NFC")
    .replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}<>:"/\\|?*]/gu, " ")
    .replace(/\s+/gu, " ")
    .replace(/^[\s.]+|[\s.]+$/gu, "");
  const name = [...cleaned]
    .slice(0, MAX_NAME)
    .join("")
    .replace(/[\s.]+$/u, "");
  return `${name === "" || RESERVED_NAME.test(name) ? fallback : name}.md`;
}

/** Hands the browser `content` as a file to save: a `Blob` URL, never an HTML page. */
export function downloadText(content: string, fileName: string, type = "text/markdown;charset=utf-8"): void {
  const url = URL.createObjectURL(new Blob([content], { type }));
  const link = document.createElement("a");
  link.href = url;
  link.download = fileName;
  link.rel = "noopener";
  document.body.append(link);
  link.click();
  link.remove();
  // The browser reads the URL after the click returns, so it is released a minute later.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}
