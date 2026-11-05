// The text rules of the composer's deterministic completion (docs/design/home-ai/design.md,
// 「@ 提及」 and 「問句範本」): where an `@` or `/` opens a list, where the blanks of a template are,
// how the mentions already in the box follow an edit, and how the list is searched and ranked.
// Everything here is a pure function of the text, so it is tested without a DOM.

/** What an `@` mention can stand for. A time range is a mention too: its words and its span. */
export type MentionKind = "device" | "point" | "model" | "range";

/** The structured data a mention stands for, sent beside the question so the service never matches by name. */
export type MentionTarget =
  | { kind: "device"; deviceId: string }
  | { kind: "point"; deviceId: string; point: string }
  | { kind: "model"; modelId: string }
  /** `from` inclusive and `to` exclusive, as instants (ISO 8601); an open range ends at the moment it was picked. */
  | { kind: "range"; id: string; from: string; to: string };

/** A mention in the box: the text `[start, end)` is `label`, `@` and the name. */
export interface Mention {
  start: number;
  end: number;
  label: string;
  target: MentionTarget;
}

/** The characters a mention may be followed by: a space, or punctuation that ends a phrase. */
const AFTER_MENTION = /[\s，。、？！,.?!;；:：)）]/u;

/** Whether the mention is still what it was: its text unchanged and not run into the word after it. */
function isIntact(mention: Mention, text: string): boolean {
  if (text.slice(mention.start, mention.end) !== mention.label) return false;
  const next = text.charAt(mention.end);
  return next === "" || AFTER_MENTION.test(next);
}

/** The mentions that are still intact in `text`; an edit inside one turns it back into plain text. */
export function intactMentions(mentions: readonly Mention[], text: string): Mention[] {
  return mentions.filter((mention) => isIntact(mention, text));
}

/** The one edit that turns `before` into `after`: `[start, end)` of `before` was replaced by `inserted` characters. */
export function diffEdit(before: string, after: string): { start: number; end: number; inserted: number } {
  const limit = Math.min(before.length, after.length);
  let start = 0;
  while (start < limit && before.charCodeAt(start) === after.charCodeAt(start)) start += 1;
  let tail = 0;
  while (
    tail < limit - start &&
    before.charCodeAt(before.length - 1 - tail) === after.charCodeAt(after.length - 1 - tail)
  ) {
    tail += 1;
  }
  return { start, end: before.length - tail, inserted: after.length - start - tail };
}

/** Moves the mentions past an edit of `[start, end)` that wrote `inserted` characters; one the edit touches is dropped. */
export function applyEdit(mentions: readonly Mention[], start: number, end: number, inserted: number): Mention[] {
  const shift = inserted - (end - start);
  const kept: Mention[] = [];
  for (const mention of mentions) {
    if (mention.end <= start) kept.push(mention);
    else if (mention.start >= end) kept.push({ ...mention, start: mention.start + shift, end: mention.end + shift });
  }
  return kept;
}

/** The mentions after the box changed from `before` to `after` by typing, pasting or deleting. */
export function reconcileMentions(mentions: readonly Mention[], before: string, after: string): Mention[] {
  const edit = diffEdit(before, after);
  return intactMentions(applyEdit(mentions, edit.start, edit.end, edit.inserted), after);
}

/** A blank of a question template: `［設備］`. */
export interface Slot {
  start: number;
  end: number;
  /** The words inside the brackets. */
  label: string;
}

const SLOT = /［([^］\n]*)］/gu;

/** The blanks in `text`, in order. */
export function findSlots(text: string): Slot[] {
  return Array.from(text.matchAll(SLOT), (match) => ({
    start: match.index,
    end: match.index + match[0].length,
    label: match[1] ?? "",
  }));
}

/**
 * The blank after the selection `[from, to)`, or with `backwards` the one before it. It does not
 * wrap round: after the last blank (or before the first) there is none, and Tab leaves the box.
 */
export function neighbourSlot(slots: readonly Slot[], from: number, to: number, backwards: boolean): Slot | null {
  if (backwards) return slots.findLast((slot) => slot.end <= from) ?? null;
  return slots.find((slot) => slot.start >= to) ?? null;
}

/** What the text before the caret asks to open. */
export type Trigger =
  | { mode: "mention"; start: number; query: string }
  | { mode: "template"; start: number; query: string };

/** `@` opens at the start, after a space or after punctuation; the query is up to 20 characters without a space. */
const MENTION_TRIGGER = /(^|[\s，。、？！,.?!;；:：(（])@([^\s@，。、？！]{0,20})$/u;
const TEMPLATE_TRIGGER = /^\/(\S{0,10})$/u;

/** The list the text before `caret` asks for, if any. `/` only counts when templates are on offer. */
export function detectTrigger(text: string, caret: number, templates: boolean): Trigger | null {
  const before = text.slice(0, caret);
  const mention = MENTION_TRIGGER.exec(before);
  if (mention) {
    const query = mention[2] ?? "";
    return { mode: "mention", start: caret - query.length - 1, query };
  }
  if (templates) {
    const template = TEMPLATE_TRIGGER.exec(before);
    if (template) return { mode: "template", start: 0, query: template[1] ?? "" };
  }
  return null;
}

/**
 * What the box holds once the mention of `name` replaces `[start, end)`, and where the caret goes:
 * a space on each side unless there is one already, so the mention never runs into a word.
 */
export function mentionInsertion(
  text: string,
  start: number,
  end: number,
  name: string,
): { text: string; caret: number; labelStart: number; labelEnd: number } {
  const label = `@${name}`;
  const before = text.slice(0, start);
  const after = text.slice(end);
  const lead = before !== "" && !/\s$/u.test(before) ? " " : "";
  const trail = /^\s/u.test(after) ? "" : " ";
  const labelStart = before.length + lead.length;
  const labelEnd = labelStart + label.length;
  return {
    text: `${before}${lead}${label}${trail}${after}`,
    // Past the space that follows the mention, whichever of the two it is.
    caret: labelEnd + 1,
    labelStart,
    labelEnd,
  };
}

/** The `[from, to)` of the first place the query is in the name, or null. Case does not matter. */
export function matchSpan(name: string, query: string): [number, number] | null {
  if (query === "") return null;
  const at = name.toLocaleLowerCase().indexOf(query.toLocaleLowerCase());
  return at < 0 ? null : [at, at + query.length];
}

/** Entries whose name (or one of its extra search words) holds the query: prefix matches first, then by name. */
export function rank<T extends { name: string; words?: string }>(entries: readonly T[], query: string): T[] {
  if (query === "") return [...entries];
  const needle = query.toLocaleLowerCase();
  const scored: { entry: T; score: number }[] = [];
  for (const entry of entries) {
    const at = entry.name.toLocaleLowerCase().indexOf(needle);
    if (at === 0) scored.push({ entry, score: 0 });
    else if (at > 0) scored.push({ entry, score: 1 });
    else if (entry.words?.toLocaleLowerCase().includes(needle)) scored.push({ entry, score: 2 });
  }
  scored.sort((a, b) => a.score - b.score || a.entry.name.localeCompare(b.entry.name));
  return scored.map((item) => item.entry);
}
