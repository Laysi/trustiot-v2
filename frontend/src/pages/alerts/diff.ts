export interface DiffLine {
  /** The line's place in the result, which is its identity (the same text can occur twice). */
  id: number;
  kind: "same" | "add" | "del";
  text: string;
}

/** A line-by-line difference of two scripts (longest common subsequence); scripts are tens of lines. */
export function diffLines(before: string, after: string): DiffLine[] {
  const a = before.split("\n");
  const b = after.split("\n");
  const table: number[][] = Array.from({ length: a.length + 1 }, () => new Array<number>(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      const row = table[i] as number[];
      row[j] =
        a[i] === b[j]
          ? ((table[i + 1] as number[])[j + 1] as number) + 1
          : Math.max((table[i + 1] as number[])[j] as number, row[j + 1] as number);
    }
  }
  const lines: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      lines.push({ id: lines.length, kind: "same", text: a[i] as string });
      i += 1;
      j += 1;
    } else if (((table[i + 1] as number[])[j] as number) >= ((table[i] as number[])[j + 1] as number)) {
      lines.push({ id: lines.length, kind: "del", text: a[i] as string });
      i += 1;
    } else {
      lines.push({ id: lines.length, kind: "add", text: b[j] as string });
      j += 1;
    }
  }
  for (; i < a.length; i += 1) lines.push({ id: lines.length, kind: "del", text: a[i] as string });
  for (; j < b.length; j += 1) lines.push({ id: lines.length, kind: "add", text: b[j] as string });
  return lines;
}
