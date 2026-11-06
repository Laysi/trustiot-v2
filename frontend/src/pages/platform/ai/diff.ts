/**
 * A line-level difference of two texts (longest common subsequence), with a word-level one inside
 * a changed pair of lines, and the editor's variable check. Text is only ever compared and
 * printed as text: nothing here produces markup.
 */

export type DiffKind = "context" | "add" | "del";

export interface DiffLine {
  kind: DiffKind;
  text: string;
  /** The line's number in the old text (not for an added line). */
  oldNo?: number;
  /** The line's number in the new text (not for a deleted line). */
  newNo?: number;
}

/** A piece of a line, and whether it is what changed within it. */
export interface Segment {
  text: string;
  changed: boolean;
}

/** The lines of `text`; the empty text has none. */
export function splitLines(text: string): string[] {
  return text === "" ? [] : text.split(/\r\n|\r|\n/);
}

/** The longest common subsequence of `a` and `b` as pairs of indexes. */
function lcs<T>(a: readonly T[], b: readonly T[]): [number, number][] {
  const rows = a.length + 1;
  const cols = b.length + 1;
  const table = new Uint32Array(rows * cols);
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i * cols + j] =
        a[i] === b[j]
          ? (table[(i + 1) * cols + j + 1] as number) + 1
          : Math.max(table[(i + 1) * cols + j] as number, table[i * cols + j + 1] as number);
    }
  }
  const pairs: [number, number][] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      pairs.push([i, j]);
      i += 1;
      j += 1;
    } else if ((table[(i + 1) * cols + j] as number) >= (table[i * cols + j + 1] as number)) {
      i += 1;
    } else {
      j += 1;
    }
  }
  return pairs;
}

/** What turns `before` into `after`, line by line, in reading order (a deleted line before the added one that replaces it). */
export function diffLines(before: string, after: string): DiffLine[] {
  const a = splitLines(before);
  const b = splitLines(after);
  const lines: DiffLine[] = [];
  let i = 0;
  let j = 0;
  for (const [ci, cj] of [...lcs(a, b), [a.length, b.length] as [number, number]]) {
    while (i < ci) {
      lines.push({ kind: "del", text: a[i] as string, oldNo: i + 1 });
      i += 1;
    }
    while (j < cj) {
      lines.push({ kind: "add", text: b[j] as string, newNo: j + 1 });
      j += 1;
    }
    if (ci < a.length) {
      lines.push({ kind: "context", text: a[ci] as string, oldNo: ci + 1, newNo: cj + 1 });
      i = ci + 1;
      j = cj + 1;
    }
  }
  return lines;
}

/** How many lines were added and deleted. */
export function countChanges(lines: readonly DiffLine[]): { added: number; deleted: number } {
  return {
    added: lines.filter((line) => line.kind === "add").length,
    deleted: lines.filter((line) => line.kind === "del").length,
  };
}

export function isChanged(lines: readonly DiffLine[]): boolean {
  return lines.some((line) => line.kind !== "context");
}

/** Words, spaces and single other characters (so Chinese text differs by character). */
function tokens(text: string): string[] {
  return text.match(/[A-Za-z0-9_]+|\s+|[\s\S]/gu) ?? [];
}

function merge(parts: Segment[]): Segment[] {
  const merged: Segment[] = [];
  for (const part of parts) {
    const last = merged.at(-1);
    if (last && last.changed === part.changed) last.text += part.text;
    else merged.push({ ...part });
  }
  return merged;
}

/** The words that differ between a deleted line and the added one that replaces it. */
export function wordSegments(oldText: string, newText: string): { old: Segment[]; new: Segment[] } {
  const a = tokens(oldText);
  const b = tokens(newText);
  const common = lcs(a, b);
  const oldChanged = new Set(a.keys());
  const newChanged = new Set(b.keys());
  for (const [i, j] of common) {
    oldChanged.delete(i);
    newChanged.delete(j);
  }
  return {
    old: merge(a.map((text, index) => ({ text, changed: oldChanged.has(index) }))),
    new: merge(b.map((text, index) => ({ text, changed: newChanged.has(index) }))),
  };
}

/** One row of the side-by-side view: the old line and the new one (either may be missing). */
export interface SplitRow {
  left: DiffLine | null;
  right: DiffLine | null;
}

/** Lines laid out in two columns: a run of deletions sits next to the run of additions that follows it. */
export function splitRows(lines: readonly DiffLine[]): SplitRow[] {
  const rows: SplitRow[] = [];
  let index = 0;
  while (index < lines.length) {
    const line = lines[index] as DiffLine;
    if (line.kind === "context") {
      rows.push({ left: line, right: line });
      index += 1;
      continue;
    }
    const deleted: DiffLine[] = [];
    const added: DiffLine[] = [];
    while (index < lines.length && (lines[index] as DiffLine).kind === "del") {
      deleted.push(lines[index] as DiffLine);
      index += 1;
    }
    while (index < lines.length && (lines[index] as DiffLine).kind === "add") {
      added.push(lines[index] as DiffLine);
      index += 1;
    }
    for (let row = 0; row < Math.max(deleted.length, added.length); row += 1) {
      rows.push({ left: deleted[row] ?? null, right: added[row] ?? null });
    }
  }
  return rows;
}

/**
 * `current` with the change the default made from `from` to `to` applied: a line the default
 * added goes after the line that precedes it there (at the end when `current` has no such line),
 * a line it deleted is dropped where `current` still has it exactly. The editor shows the result
 * as a draft, so the administrator decides; nothing is merged without being seen.
 */
export function applyDefaultChange(current: string, from: string, to: string): string {
  const lines = splitLines(current);
  // Where the next added line goes: after the last line of the default that `current` still has,
  // or at the end once a line of the default is missing from it.
  let cursor = 0;
  for (const line of diffLines(from, to)) {
    if (line.kind === "context") {
      // The default's lines come in order, so the match is the next one from the cursor on.
      const at = lines.indexOf(line.text, cursor);
      cursor = at >= 0 ? at + 1 : lines.length;
    } else if (line.kind === "del") {
      const ahead = lines.indexOf(line.text, cursor);
      const at = ahead >= 0 ? ahead : lines.indexOf(line.text);
      if (at >= 0) {
        lines.splice(at, 1);
        if (at < cursor) cursor -= 1;
      }
    } else {
      lines.splice(cursor, 0, line.text);
      cursor += 1;
    }
  }
  return lines.join("\n");
}

const VARIABLE = /\{\{\s*([^{}\s]*)\s*\}\}/g;

/** One `{{name}}` in a prompt. */
export interface Variable {
  name: string;
  /** The text as written, braces included. */
  raw: string;
  /** 1-based. */
  line: number;
  /** Offsets in the whole text. */
  start: number;
  end: number;
}

/** Every `{{name}}` in `text`, in order. */
export function findVariables(text: string): Variable[] {
  const found: Variable[] = [];
  for (const match of text.matchAll(VARIABLE)) {
    const start = match.index ?? 0;
    found.push({
      name: match[1] as string,
      raw: match[0],
      line: text.slice(0, start).split("\n").length,
      start,
      end: start + match[0].length,
    });
  }
  return found;
}

/** The variables of `text` the service does not list. */
export function unknownVariables(text: string, allowed: readonly string[]): Variable[] {
  return findVariables(text).filter((variable) => !allowed.includes(variable.name));
}
