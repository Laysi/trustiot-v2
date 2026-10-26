// Page text that crosses into a report: script error messages (`check::MAX_SCRIPT_MESSAGE_CHARS`).
import { MAX_SCRIPT_MESSAGE_CHARS } from "./protocol.ts";

/** Characters a message may not carry (`spec::is_hidden_char`): controls and invisible formatting. */
const HIDDEN = /[\p{Cc}\u00AD\u061C\u180E\u200B-\u200F\u202A-\u202E\u2060-\u2064\u2066-\u206F\uFEFF\uFFF9-\uFFFB]/gu;

/**
 * `text` as a problem may carry it: hidden characters turned into spaces, trimmed, at most 200
 * characters; `undefined` when nothing visible is left.
 */
export function visibleMessage(text: string): string | undefined {
  const visible = text.replace(HIDDEN, " ").trim();
  if (visible === "") return undefined;
  const chars = Array.from(visible);
  return chars.length > MAX_SCRIPT_MESSAGE_CHARS
    ? chars.slice(0, MAX_SCRIPT_MESSAGE_CHARS).join("").trimEnd()
    : visible;
}
