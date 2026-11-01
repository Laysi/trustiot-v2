// The computation language (frontend/sandbox/README.md, 「計算語言」): the TypeScript twin of
// crates/contract/src/api/dashboard/formula.rs. The runtime evaluates a computation for
// `f|name` at display time, and the comparison evaluates it again from the checker's own data.
// Both implementations run crates/contract/tests/fixtures/dashboard/formula-vectors.json
// (formula.test.ts), so they parse, refuse and evaluate the same way: same error classes at the
// same character positions, same values to the last bit.
import { COLUMN_NAME_PATTERN, MAX_DATASET_ROWS, NAME_PATTERN } from "./protocol.ts";

/** Longest formula, in UTF-8 bytes (`formula::MAX_FORMULA_BYTES`). */
export const MAX_FORMULA_BYTES = 512;
/** Most numbers, columns and operators (`formula::MAX_FORMULA_NODES`). */
export const MAX_FORMULA_NODES = 64;
/** Deepest nesting of parentheses and unary minuses (`formula::MAX_FORMULA_DEPTH`). */
export const MAX_FORMULA_DEPTH = 32;

export const AGGREGATES = ["sum", "avg", "min", "max", "count", "first", "last"] as const;
export type Aggregate = (typeof AGGREGATES)[number];

export const FORMULA_ERROR_CLASSES = [
  "empty",
  "too_long",
  "unexpected_character",
  "unexpected_token",
  "unexpected_end",
  "unknown_function",
  "invalid_name",
  "bare_column",
  "invalid_number",
  "index_out_of_range",
  "too_complex",
  "unknown_dataset",
  "unknown_column",
  "column_not_numeric",
] as const;
export type FormulaErrorClass = (typeof FORMULA_ERROR_CLASSES)[number];

/** Why a formula was refused; `position` is the 1-based character position when known. */
export interface FormulaError {
  class: FormulaErrorClass;
  position?: number;
}

export type Operator = "+" | "-" | "*" | "/";

/** A node of a parsed formula; `at` is where an aggregate or index starts (1-based). */
export type Expr =
  | { kind: "number"; value: number }
  | { kind: "aggregate"; aggregate: Aggregate; dataset: string; column: string; at: number }
  | { kind: "index"; dataset: string; column: string; row: number; at: number }
  | { kind: "negate"; inner: Expr }
  | { kind: "binary"; operator: Operator; left: Expr; right: Expr };

export type ParseResult = { ok: true; expr: Expr } | { ok: false; error: FormulaError };

/** What is known of a dataset when a formula is checked (`formula::DatasetShape`). */
export interface DatasetShape {
  maxRows: number;
  /** Column names and types, when known. */
  columns: ReadonlyArray<{ name: string; type: string }> | null;
}

/** One cell as a formula sees it: a finite number, `null`, or anything else (`other`). */
export type Cell = number | null | "other";

type Token =
  | { kind: "number"; value: number; integer: boolean; at: number }
  | { kind: "ident"; name: string; at: number }
  | { kind: "." | "(" | ")" | "[" | "]" | "+" | "-" | "*" | "/"; at: number };

class Refusal {
  constructor(readonly error: FormulaError) {}
}

function refuse(errorClass: FormulaErrorClass, position?: number): Refusal {
  return new Refusal(position === undefined ? { class: errorClass } : { class: errorClass, position });
}

function isDigit(c: string | undefined): boolean {
  return c !== undefined && c >= "0" && c <= "9";
}

function isIdentStart(c: string | undefined): boolean {
  return c !== undefined && ((c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || c === "_");
}

function isIdentPart(c: string | undefined): boolean {
  return isIdentStart(c) || isDigit(c);
}

/** The UTF-8 length of `text`, as Rust's `str::len` counts it. */
function utf8Length(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4;
        i++;
      } else bytes += 3;
    } else bytes += 3;
  }
  return bytes;
}

/** The characters of `text` as code points, the way Rust's `str::chars` sees them. */
function codePoints(text: string): string[] {
  const chars: string[] = [];
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code >= 0xd800 && code <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        chars[chars.length] = text.slice(i, i + 2);
        i++;
        continue;
      }
    }
    chars[chars.length] = text.charAt(i);
  }
  return chars;
}

const SINGLE: Record<string, Token["kind"]> = {
  ".": ".",
  "(": "(",
  ")": ")",
  "[": "[",
  "]": "]",
  "+": "+",
  "-": "-",
  "*": "*",
  "/": "/",
};

function lex(chars: readonly string[]): Token[] {
  const tokens: Token[] = [];
  let index = 0;
  while (index < chars.length) {
    const current = chars[index];
    const at = index + 1;
    if (current === " " || current === "\t" || current === "\r" || current === "\n") {
      index++;
      continue;
    }
    const single = current === undefined ? undefined : Object.hasOwn(SINGLE, current) ? SINGLE[current] : undefined;
    if (single !== undefined) {
      tokens[tokens.length] = { kind: single, at } as Token;
      index++;
    } else if (isDigit(current)) {
      const [token, next] = lexNumber(chars, index);
      tokens[tokens.length] = token;
      index = next;
    } else if (isIdentStart(current)) {
      const start = index;
      while (isIdentPart(chars[index])) index++;
      tokens[tokens.length] = { kind: "ident", name: chars.slice(start, index).join(""), at };
    } else {
      throw refuse("unexpected_character", at);
    }
  }
  return tokens;
}

function lexNumber(chars: readonly string[], start: number): [Token, number] {
  const invalid = () => refuse("invalid_number", start + 1);
  const digits = (from: number) => {
    let end = from;
    while (isDigit(chars[end])) end++;
    return end;
  };
  let end = digits(start);
  let plain = true;
  if (chars[end] === ".") {
    const after = digits(end + 1);
    if (after === end + 1) throw invalid();
    end = after;
    plain = false;
  }
  if (chars[end] === "e" || chars[end] === "E") {
    let from = end + 1;
    if (chars[from] === "+" || chars[from] === "-") from++;
    const after = digits(from);
    if (after === from) throw invalid();
    end = after;
    plain = false;
  }
  const value = Number(chars.slice(start, end).join(""));
  if (!Number.isFinite(value)) throw invalid();
  return [{ kind: "number", value, integer: plain, at: start + 1 }, end];
}

class Parser {
  private next = 0;
  private nodes = 0;

  constructor(
    private readonly tokens: readonly Token[],
    /** One past the last character, for errors at the end. */
    private readonly end: number,
  ) {}

  peek(): Token | undefined {
    return this.tokens[this.next];
  }

  private take(): Token {
    const token = this.tokens[this.next];
    if (token === undefined) throw refuse("unexpected_end", this.end);
    this.next++;
    return token;
  }

  private expect(kind: Token["kind"]): void {
    const token = this.take();
    if (token.kind !== kind) throw refuse("unexpected_token", token.at);
  }

  private node(at: number): void {
    this.nodes++;
    if (this.nodes > MAX_FORMULA_NODES) throw refuse("too_complex", at);
  }

  expr(depth: number): Expr {
    let left = this.term(depth);
    for (let token = this.peek(); token !== undefined; token = this.peek()) {
      if (token.kind !== "+" && token.kind !== "-") break;
      this.next++;
      this.node(token.at);
      const right = this.term(depth);
      left = { kind: "binary", operator: token.kind, left, right };
    }
    return left;
  }

  private term(depth: number): Expr {
    let left = this.unary(depth);
    for (let token = this.peek(); token !== undefined; token = this.peek()) {
      if (token.kind !== "*" && token.kind !== "/") break;
      this.next++;
      this.node(token.at);
      const right = this.unary(depth);
      left = { kind: "binary", operator: token.kind, left, right };
    }
    return left;
  }

  private unary(depth: number): Expr {
    const token = this.peek();
    if (token !== undefined && token.kind === "-") {
      if (depth >= MAX_FORMULA_DEPTH) throw refuse("too_complex", token.at);
      this.next++;
      this.node(token.at);
      return { kind: "negate", inner: this.unary(depth + 1) };
    }
    return this.primary(depth);
  }

  private primary(depth: number): Expr {
    const token = this.take();
    switch (token.kind) {
      case "number":
        this.node(token.at);
        return { kind: "number", value: token.value };
      case "(": {
        if (depth >= MAX_FORMULA_DEPTH) throw refuse("too_complex", token.at);
        const inner = this.expr(depth + 1);
        this.expect(")");
        return inner;
      }
      case "ident":
        return this.named(token.name, token.at);
      default:
        throw refuse("unexpected_token", token.at);
    }
  }

  /** An aggregate call or an indexed column, starting with the identifier `name`. */
  private named(name: string, at: number): Expr {
    const following = this.peek();
    if (following !== undefined && following.kind === "(") {
      let aggregate: Aggregate | undefined;
      for (let i = 0; i < AGGREGATES.length; i++) {
        if (AGGREGATES[i] === name) aggregate = AGGREGATES[i];
      }
      if (aggregate === undefined) throw refuse("unknown_function", at);
      this.next++;
      const columnAt = this.peek()?.at ?? this.end;
      const first = this.take();
      if (first.kind !== "ident") throw refuse("unexpected_token", columnAt);
      const column = this.column(first.name, columnAt);
      this.expect(")");
      this.node(at);
      return { kind: "aggregate", aggregate, dataset: column.dataset, column: column.column, at };
    }
    if (following !== undefined && following.kind === ".") {
      const column = this.column(name, at);
      const bracket = this.peek();
      if (bracket === undefined || bracket.kind !== "[") throw refuse("bare_column", at);
      this.next++;
      const index = this.take();
      let row: number;
      if (index.kind === "number" && index.integer && index.value < MAX_DATASET_ROWS) {
        row = index.value;
      } else if (index.kind === "number" && Number.isInteger(index.value) && index.value >= MAX_DATASET_ROWS) {
        throw refuse("index_out_of_range", index.at);
      } else {
        throw refuse("unexpected_token", index.at);
      }
      this.expect("]");
      this.node(at);
      return { kind: "index", dataset: column.dataset, column: column.column, row, at };
    }
    throw refuse("unexpected_token", at);
  }

  /** `dataset "." column`, after the dataset's identifier was taken. */
  private column(dataset: string, at: number): { dataset: string; column: string } {
    if (!NAME_PATTERN.test(dataset)) throw refuse("invalid_name", at);
    this.expect(".");
    const token = this.take();
    if (token.kind !== "ident") throw refuse("unexpected_token", token.at);
    if (!COLUMN_NAME_PATTERN.test(token.name)) throw refuse("invalid_name", token.at);
    return { dataset, column: token.name };
  }
}

/** Parses `text` (`Formula::parse`): the tree, or the first problem with its position. */
export function parseFormula(text: string): ParseResult {
  try {
    if (utf8Length(text) > MAX_FORMULA_BYTES) throw refuse("too_long");
    const chars = codePoints(text);
    const tokens = lex(chars);
    if (tokens.length === 0) throw refuse("empty");
    const parser = new Parser(tokens, chars.length + 1);
    const expr = parser.expr(0);
    const rest = parser.peek();
    if (rest !== undefined) throw refuse("unexpected_token", rest.at);
    return { ok: true, expr };
  } catch (error) {
    if (error instanceof Refusal) return { ok: false, error: error.error };
    throw error;
  }
}

/**
 * Compares a formula's columns with what is known of the datasets (`Formula::check`): the first
 * column, in order of appearance, that names an undeclared dataset, a row at or past its
 * `max_rows`, a missing column, or a non-number column outside `count`; `null` when none does.
 */
export function checkFormula(expr: Expr, shape: (dataset: string) => DatasetShape | null): FormulaError | null {
  switch (expr.kind) {
    case "number":
      return null;
    case "negate":
      return checkFormula(expr.inner, shape);
    case "binary":
      return checkFormula(expr.left, shape) ?? checkFormula(expr.right, shape);
    case "aggregate":
    case "index": {
      const known = shape(expr.dataset);
      if (known === null) return { class: "unknown_dataset", position: expr.at };
      if (expr.kind === "index" && expr.row >= known.maxRows) {
        return { class: "index_out_of_range", position: expr.at };
      }
      if (known.columns !== null) {
        let type: string | undefined;
        for (let i = 0; i < known.columns.length; i++) {
          const column = known.columns[i];
          if (column !== undefined && column.name === expr.column) {
            type = column.type;
            break;
          }
        }
        if (type === undefined) return { class: "unknown_column", position: expr.at };
        const counting = expr.kind === "aggregate" && expr.aggregate === "count";
        if (type !== "number" && !counting) return { class: "column_not_numeric", position: expr.at };
      }
      return null;
    }
  }
}

/** A result cell as a formula sees it (`Cell::of_json`). */
export function cellOf(value: unknown): Cell {
  if (value === null || value === undefined) return null;
  if (typeof value === "number") return Number.isFinite(value) ? value : null;
  return "other";
}

function finite(value: number): number | null {
  return Number.isFinite(value) ? value : null;
}

function aggregate(fn: Aggregate, cells: readonly Cell[]): number | null {
  let total = 0;
  let count = 0;
  let chosen: number | null = null;
  switch (fn) {
    case "sum":
    case "avg":
      for (let i = 0; i < cells.length; i++) {
        const cell = cells[i];
        if (typeof cell === "number") {
          total += cell;
          count++;
        }
      }
      if (count === 0) return null;
      return finite(fn === "sum" ? total : total / count);
    case "min":
    case "max":
      for (let i = 0; i < cells.length; i++) {
        const cell = cells[i];
        if (typeof cell !== "number") continue;
        if (chosen === null || (fn === "min" ? cell < chosen : cell > chosen)) chosen = cell;
      }
      return chosen === null ? null : finite(chosen);
    case "count":
      for (let i = 0; i < cells.length; i++) {
        if (cells[i] !== null) count++;
      }
      return count;
    case "first":
    case "last": {
      const cell = cells.length === 0 ? null : cells[fn === "first" ? 0 : cells.length - 1];
      return typeof cell === "number" ? finite(cell) : null;
    }
  }
}

/**
 * The formula's value over the data (`Formula::evaluate`): `cells` returns a column's cells in
 * row order, `null` when there is no such column; the result is a finite number or `null`.
 */
export function evaluateFormula(expr: Expr, cells: (dataset: string, column: string) => Cell[] | null): number | null {
  switch (expr.kind) {
    case "number":
      return finite(expr.value);
    case "negate": {
      const inner = evaluateFormula(expr.inner, cells);
      return inner === null ? null : -inner;
    }
    case "binary": {
      const left = evaluateFormula(expr.left, cells);
      if (left === null) return null;
      const right = evaluateFormula(expr.right, cells);
      if (right === null) return null;
      switch (expr.operator) {
        case "+":
          return finite(left + right);
        case "-":
          return finite(left - right);
        case "*":
          return finite(left * right);
        case "/":
          return finite(left / right);
      }
      return null;
    }
    case "index": {
      const column = cells(expr.dataset, expr.column);
      if (column === null) return null;
      const cell = expr.row < column.length ? column[expr.row] : null;
      return typeof cell === "number" ? cell : null;
    }
    case "aggregate": {
      const column = cells(expr.dataset, expr.column);
      return column === null ? null : aggregate(expr.aggregate, column);
    }
  }
}

/** The canonical tree as JSON (`Formula::to_sexpr`), the form of the shared test vectors. */
export function toSexpr(expr: Expr): unknown {
  switch (expr.kind) {
    case "number":
      return expr.value;
    case "aggregate":
      return [expr.aggregate, `${expr.dataset}.${expr.column}`];
    case "index":
      return ["at", `${expr.dataset}.${expr.column}`, expr.row];
    case "negate":
      return ["neg", toSexpr(expr.inner)];
    case "binary":
      return [expr.operator, toSexpr(expr.left), toSexpr(expr.right)];
  }
}
