//! The computation language: a named computation's formula, which turns dataset columns into one
//! number (design record 「頁面與 runtime」).
//!
//! The assistant parses and validates every formula here when the `dashboard` tool writes a
//! version; the sandbox runtime (`frontend/sandbox/`) parses and evaluates the same language in
//! TypeScript, at display time and in the check. Both run the shared test vectors in
//! `tests/fixtures/dashboard/formula-vectors.json`, so the two implementations cannot drift.
//!
//! # Grammar
//!
//! ```text
//! formula   = expr
//! expr      = term { ("+" | "-") term }            left-associative
//! term      = unary { ("*" | "/") unary }          left-associative
//! unary     = "-" unary | primary
//! primary   = number | "(" expr ")" | aggregate "(" column ")" | column "[" index "]"
//! aggregate = "sum" | "avg" | "min" | "max" | "count" | "first" | "last"
//! column    = dataset "." name                     dataset: names::is_name; name: names::is_column_name
//! index     = digits                               a row index below result::MAX_DATASET_ROWS
//! number    = digits [ "." digits ] [ ("e" | "E") [ "+" | "-" ] digits ]
//! digits    = "0"…"9" { "0"…"9" }
//! ```
//!
//! - Tokens may be separated by spaces, tabs, carriage returns and line feeds; nothing else is
//!   allowed outside the tokens. Identifiers are ASCII letters, digits and `_`, starting with a
//!   letter or `_`; aggregate names are lowercase.
//! - A number's `.` must be followed by a digit (`1.` is an invalid number), and a number must
//!   be finite (`1e999` is an invalid number). Leading zeros are allowed (`007` is 7).
//! - A column is always aggregated or indexed: the result is one number, never a column.
//! - At most [`MAX_FORMULA_BYTES`] bytes, [`MAX_FORMULA_NODES`] nodes (numbers, columns and
//!   operators) and [`MAX_FORMULA_DEPTH`] nested parentheses and unary minuses.
//!
//! # Values
//!
//! Every value is an IEEE 754 double or `null` (no value):
//!
//! - `dataset.column[i]`: the cell in row `i`; `null` when the row does not exist (a viewer may
//!   see fewer rows than the check did) or the cell is `null`.
//! - `sum`: the non-null cells added in row order, starting from 0; `null` when there are none.
//! - `avg`: that sum divided by the number of non-null cells; `null` when there are none.
//! - `min`, `max`: the first smallest or largest non-null cell in row order (only `0` and `-0`
//!   can tie with different values); `null` when there are none.
//! - `count`: the number of non-null cells, of any kind; `0` for an empty column.
//! - `first`, `last`: the cell in the first or last row; `null` when there are no rows or the
//!   cell is `null`.
//! - `+ - * /` and unary `-`: `null` when an operand is `null`; a result that is not finite
//!   (division by zero, overflow, `0/0`) is `null`.
//!
//! Every column except `count`'s must hold numbers ([`ColumnKind::Number`]). The parse does not
//! know the datasets; [`Formula::check`] compares the references with what is known of them.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

use super::names::{is_column_name, is_name};
use super::result::{ColumnKind, MAX_DATASET_ROWS};

/// Longest formula, in bytes: a formula combines a few aggregates; anything longer belongs in
/// the dataset's query, where the user can read how it is computed.
pub const MAX_FORMULA_BYTES: usize = 512;

/// Most numbers, columns and operators in one formula.
pub const MAX_FORMULA_NODES: usize = 64;

/// Deepest nesting of parentheses and unary minuses.
pub const MAX_FORMULA_DEPTH: usize = 32;

/// The aggregates of the language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Aggregate {
    Sum,
    Avg,
    Min,
    Max,
    Count,
    First,
    Last,
}

impl Aggregate {
    /// Every aggregate.
    pub const ALL: [Self; 7] = [
        Self::Sum,
        Self::Avg,
        Self::Min,
        Self::Max,
        Self::Count,
        Self::First,
        Self::Last,
    ];

    /// The name in a formula.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
            Self::Count => "count",
            Self::First => "first",
            Self::Last => "last",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|aggregate| aggregate.as_str() == name)
    }
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operator {
    Add,
    Subtract,
    Multiply,
    Divide,
}

impl Operator {
    /// The operator's character.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
        }
    }
}

/// `dataset.column` in a formula.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnRef {
    pub dataset: String,
    pub column: String,
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.dataset, self.column)
    }
}

/// A node of a parsed formula. `at` is the 1-based character position where the node starts,
/// for error messages.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Number(f64),
    Aggregate {
        function: Aggregate,
        column: ColumnRef,
        at: u32,
    },
    Index {
        column: ColumnRef,
        row: u32,
        at: u32,
    },
    Negate(Box<Expr>),
    Binary {
        operator: Operator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
}

/// What is wrong with a formula: a closed set of classes, each with a fixed English message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = DashboardFormulaErrorClass)]
pub enum FormulaErrorClass {
    /// Nothing but white space.
    Empty,
    /// Longer than [`MAX_FORMULA_BYTES`].
    TooLong,
    /// A character outside the grammar.
    UnexpectedCharacter,
    /// A token where the grammar does not allow it.
    UnexpectedToken,
    /// The formula ends before it is complete.
    UnexpectedEnd,
    /// A name followed by `(` that is not an aggregate.
    UnknownFunction,
    /// A dataset or column name that breaks the name rules.
    InvalidName,
    /// A column that is neither aggregated nor indexed.
    BareColumn,
    /// A number that is malformed or not finite.
    InvalidNumber,
    /// A row index that no dataset can have.
    IndexOutOfRange,
    /// Too many nodes or too deeply nested.
    TooComplex,
    /// A dataset the dashboard does not declare.
    UnknownDataset,
    /// A column the dataset does not have.
    UnknownColumn,
    /// An aggregate other than `count`, or an index, over a column that does not hold numbers.
    ColumnNotNumeric,
}

impl FormulaErrorClass {
    /// Every class.
    pub const ALL: [Self; 14] = [
        Self::Empty,
        Self::TooLong,
        Self::UnexpectedCharacter,
        Self::UnexpectedToken,
        Self::UnexpectedEnd,
        Self::UnknownFunction,
        Self::InvalidName,
        Self::BareColumn,
        Self::InvalidNumber,
        Self::IndexOutOfRange,
        Self::TooComplex,
        Self::UnknownDataset,
        Self::UnknownColumn,
        Self::ColumnNotNumeric,
    ];

    /// The fixed English message for the model.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Empty => "The formula is empty.",
            Self::TooLong => {
                "The formula is longer than 512 bytes; compute more in the dataset's query."
            }
            Self::UnexpectedCharacter => {
                "The formula has a character outside the language: use numbers, + - * /, \
                 parentheses, aggregates such as sum(dataset.column) and dataset.column[row]."
            }
            Self::UnexpectedToken => {
                "The formula has a token where it is not allowed: expected a number, an \
                 aggregate such as sum(dataset.column), dataset.column[row], or a parenthesis."
            }
            Self::UnexpectedEnd => "The formula ends before it is complete.",
            Self::UnknownFunction => {
                "Only the aggregates sum, avg, min, max, count, first and last can be called."
            }
            Self::InvalidName => {
                "A dataset name is lowercase letters, digits and _ starting with a letter; a \
                 column name is letters, digits and _ starting with a letter or _."
            }
            Self::BareColumn => {
                "A column must be aggregated, such as sum(dataset.column), or indexed, such as \
                 dataset.column[0]: a computation is one number."
            }
            Self::InvalidNumber => {
                "A number is malformed or too large: write digits, an optional fraction and an \
                 optional exponent, such as 3.6 or 1e3."
            }
            Self::IndexOutOfRange => {
                "A row index is not below the dataset's max_rows (at most 2000 rows)."
            }
            Self::TooComplex => {
                "The formula has more than 64 numbers, columns and operators, or nests more than \
                 32 levels; compute more in the dataset's query."
            }
            Self::UnknownDataset => "The formula reads a dataset the dashboard does not declare.",
            Self::UnknownColumn => "The formula reads a column the dataset does not have.",
            Self::ColumnNotNumeric => {
                "Only count can read a column that does not hold numbers; the other aggregates \
                 and indexes need a number column."
            }
        }
    }
}

/// Why a formula was refused: the class, and where (1-based character position) when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[schema(as = DashboardFormulaError)]
pub struct FormulaError {
    pub class: FormulaErrorClass,
    /// 1-based character position; one past the end for [`FormulaErrorClass::UnexpectedEnd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

impl FormulaError {
    const fn at(class: FormulaErrorClass, position: u32) -> Self {
        Self {
            class,
            position: Some(position),
        }
    }

    const fn whole(class: FormulaErrorClass) -> Self {
        Self {
            class,
            position: None,
        }
    }
}

impl fmt::Display for FormulaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.class.message())?;
        if let Some(position) = self.position {
            write!(f, " (at character {position})")?;
        }
        Ok(())
    }
}

impl std::error::Error for FormulaError {}

/// What is known of a dataset when a formula is checked.
#[derive(Clone, Debug, PartialEq)]
pub struct DatasetShape {
    /// The dataset's `max_rows`: no index can reach it.
    pub max_rows: u32,
    /// Its columns and their kinds, when known: always for a semantic source, for SQL only after
    /// it ran.
    pub columns: Option<Vec<(String, ColumnKind)>>,
}

/// One cell as a formula sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cell {
    Null,
    Number(f64),
    /// Text, a time or a boolean: counted by `count`, `null` for everything else.
    Other,
}

impl Cell {
    /// The cell of a dataset result's JSON value.
    #[must_use]
    pub fn of_json(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Number(number) => number
                .as_f64()
                .filter(|number| number.is_finite())
                .map_or(Self::Null, Self::Number),
            Value::Bool(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => Self::Other,
        }
    }

    fn number(self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(number),
            Self::Null | Self::Other => None,
        }
    }
}

/// The data a formula reads.
pub trait FormulaData {
    /// The cells of `dataset.column` in row order, or `None` when there is no such column.
    fn cells(&self, dataset: &str, column: &str) -> Option<Vec<Cell>>;
}

/// A parsed formula.
#[derive(Clone, Debug, PartialEq)]
pub struct Formula {
    root: Expr,
}

impl Formula {
    /// Parses `text` (see the module documentation for the grammar).
    ///
    /// # Errors
    ///
    /// The first problem found, with its position.
    pub fn parse(text: &str) -> Result<Self, FormulaError> {
        if text.len() > MAX_FORMULA_BYTES {
            return Err(FormulaError::whole(FormulaErrorClass::TooLong));
        }
        let tokens = lex(text)?;
        if tokens.is_empty() {
            return Err(FormulaError::whole(FormulaErrorClass::Empty));
        }
        let end = u32::try_from(text.chars().count())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let mut parser = Parser {
            tokens,
            next: 0,
            end,
            nodes: 0,
        };
        let root = parser.expr(0)?;
        match parser.peek() {
            None => Ok(Self { root }),
            Some(token) => Err(FormulaError::at(
                FormulaErrorClass::UnexpectedToken,
                token.at,
            )),
        }
    }

    /// The parsed tree.
    #[must_use]
    pub fn root(&self) -> &Expr {
        &self.root
    }

    /// Every column the formula reads, in order of appearance, with the aggregate that reads it
    /// (`None` for an index).
    #[must_use]
    pub fn columns(&self) -> Vec<(&ColumnRef, Option<Aggregate>)> {
        let mut found = Vec::new();
        collect_columns(&self.root, &mut found);
        found
    }

    /// Compares the formula's columns with what is known of the datasets: `datasets` returns the
    /// shape of a declared dataset, `None` for one the dashboard does not declare.
    ///
    /// # Errors
    ///
    /// The first column, in order of appearance, that names an undeclared dataset, a column the
    /// dataset does not have, a non-number column outside `count`, or a row at or past the
    /// dataset's `max_rows`.
    pub fn check(
        &self,
        datasets: impl Fn(&str) -> Option<DatasetShape>,
    ) -> Result<(), FormulaError> {
        check_node(&self.root, &datasets)
    }

    /// The formula's value over `data`, `None` for `null` (see the module documentation).
    #[must_use]
    pub fn evaluate(&self, data: &(impl FormulaData + ?Sized)) -> Option<f64> {
        evaluate(&self.root, data)
    }

    /// The canonical tree as JSON, the form of the shared test vectors: a number, `["sum",
    /// "dataset.column"]` (and the other aggregates), `["at", "dataset.column", row]`, `["neg",
    /// x]`, or `[operator, left, right]`.
    #[must_use]
    pub fn to_sexpr(&self) -> Value {
        sexpr(&self.root)
    }
}

fn collect_columns<'a>(node: &'a Expr, found: &mut Vec<(&'a ColumnRef, Option<Aggregate>)>) {
    match node {
        Expr::Number(_) => {}
        Expr::Aggregate {
            function, column, ..
        } => found.push((column, Some(*function))),
        Expr::Index { column, .. } => found.push((column, None)),
        Expr::Negate(inner) => collect_columns(inner, found),
        Expr::Binary { left, right, .. } => {
            collect_columns(left, found);
            collect_columns(right, found);
        }
    }
}

fn check_node(
    node: &Expr,
    datasets: &impl Fn(&str) -> Option<DatasetShape>,
) -> Result<(), FormulaError> {
    let (column, function, row, at) = match node {
        Expr::Number(_) => return Ok(()),
        Expr::Negate(inner) => return check_node(inner, datasets),
        Expr::Binary { left, right, .. } => {
            check_node(left, datasets)?;
            return check_node(right, datasets);
        }
        Expr::Aggregate {
            function,
            column,
            at,
        } => (column, Some(*function), None, *at),
        Expr::Index { column, row, at } => (column, None, Some(*row), *at),
    };
    let shape =
        datasets(&column.dataset).ok_or(FormulaError::at(FormulaErrorClass::UnknownDataset, at))?;
    if let Some(row) = row
        && row >= shape.max_rows
    {
        return Err(FormulaError::at(FormulaErrorClass::IndexOutOfRange, at));
    }
    if let Some(columns) = &shape.columns {
        let kind = columns
            .iter()
            .find(|(name, _)| *name == column.column)
            .map(|(_, kind)| *kind)
            .ok_or(FormulaError::at(FormulaErrorClass::UnknownColumn, at))?;
        if kind != ColumnKind::Number && function != Some(Aggregate::Count) {
            return Err(FormulaError::at(FormulaErrorClass::ColumnNotNumeric, at));
        }
    }
    Ok(())
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn evaluate(node: &Expr, data: &(impl FormulaData + ?Sized)) -> Option<f64> {
    match node {
        Expr::Number(number) => finite(*number),
        Expr::Negate(inner) => evaluate(inner, data).map(|value| -value),
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            let left = evaluate(left, data)?;
            let right = evaluate(right, data)?;
            finite(match operator {
                Operator::Add => left + right,
                Operator::Subtract => left - right,
                Operator::Multiply => left * right,
                Operator::Divide => left / right,
            })
        }
        Expr::Index { column, row, .. } => {
            let cells = data.cells(&column.dataset, &column.column)?;
            usize::try_from(*row)
                .ok()
                .and_then(|row| cells.get(row))
                .and_then(|cell| cell.number())
        }
        Expr::Aggregate {
            function, column, ..
        } => {
            let cells = data.cells(&column.dataset, &column.column)?;
            aggregate(*function, &cells)
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a column has at most 2,000 cells, exact in an f64"
)]
fn aggregate(function: Aggregate, cells: &[Cell]) -> Option<f64> {
    let numbers = || cells.iter().filter_map(|cell| cell.number());
    let sum = || {
        let mut count = 0_usize;
        let mut total = 0.0_f64;
        for number in numbers() {
            total += number;
            count += 1;
        }
        (count > 0).then_some((total, count))
    };
    let pick = |better: fn(f64, f64) -> bool| {
        let mut chosen: Option<f64> = None;
        for number in numbers() {
            if chosen.is_none_or(|current| better(number, current)) {
                chosen = Some(number);
            }
        }
        chosen
    };
    let value = match function {
        Aggregate::Sum => sum().map(|(total, _)| total),
        Aggregate::Avg => sum().map(|(total, count)| total / count as f64),
        Aggregate::Min => pick(|number, current| number < current),
        Aggregate::Max => pick(|number, current| number > current),
        Aggregate::Count => Some(cells.iter().filter(|cell| **cell != Cell::Null).count() as f64),
        Aggregate::First => cells.first().and_then(|cell| cell.number()),
        Aggregate::Last => cells.last().and_then(|cell| cell.number()),
    };
    value.and_then(finite)
}

fn sexpr(node: &Expr) -> Value {
    match node {
        Expr::Number(number) => json!(number),
        Expr::Aggregate {
            function, column, ..
        } => json!([function.as_str(), column.to_string()]),
        Expr::Index { column, row, .. } => json!(["at", column.to_string(), row]),
        Expr::Negate(inner) => json!(["neg", sexpr(inner)]),
        Expr::Binary {
            operator,
            left,
            right,
        } => json!([operator.as_str(), sexpr(left), sexpr(right)]),
    }
}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    /// A number and, when it is written as plain digits that fit, its integer value.
    Number {
        value: f64,
        integer: Option<u32>,
    },
    Ident(String),
    Dot,
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Plus,
    Minus,
    Star,
    Slash,
}

#[derive(Clone, Debug, PartialEq)]
struct Token {
    kind: TokenKind,
    /// 1-based character position.
    at: u32,
}

fn lex(text: &str) -> Result<Vec<Token>, FormulaError> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while let Some(&current) = chars.get(index) {
        let at = position(index);
        let single = match current {
            ' ' | '\t' | '\r' | '\n' => {
                index += 1;
                continue;
            }
            '.' => Some(TokenKind::Dot),
            '(' => Some(TokenKind::LeftParen),
            ')' => Some(TokenKind::RightParen),
            '[' => Some(TokenKind::LeftBracket),
            ']' => Some(TokenKind::RightBracket),
            '+' => Some(TokenKind::Plus),
            '-' => Some(TokenKind::Minus),
            '*' => Some(TokenKind::Star),
            '/' => Some(TokenKind::Slash),
            _ => None,
        };
        if let Some(kind) = single {
            tokens.push(Token { kind, at });
            index += 1;
        } else if current.is_ascii_digit() {
            let (kind, next) = lex_number(&chars, index)?;
            tokens.push(Token { kind, at });
            index = next;
        } else if current.is_ascii_alphabetic() || current == '_' {
            let start = index;
            while chars
                .get(index)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                index += 1;
            }
            let name: String = chars.get(start..index).unwrap_or_default().iter().collect();
            tokens.push(Token {
                kind: TokenKind::Ident(name),
                at,
            });
        } else {
            return Err(FormulaError::at(FormulaErrorClass::UnexpectedCharacter, at));
        }
    }
    Ok(tokens)
}

/// The 1-based position of the character at `index`.
fn position(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1)
}

fn lex_number(chars: &[char], start: usize) -> Result<(TokenKind, usize), FormulaError> {
    let invalid = || FormulaError::at(FormulaErrorClass::InvalidNumber, position(start));
    let digits = |from: usize| {
        let mut end = from;
        while chars.get(end).is_some_and(char::is_ascii_digit) {
            end += 1;
        }
        end
    };
    let mut end = digits(start);
    let mut plain = true;
    if chars.get(end) == Some(&'.') {
        let after = digits(end + 1);
        if after == end + 1 {
            return Err(invalid());
        }
        end = after;
        plain = false;
    }
    if matches!(chars.get(end), Some('e' | 'E')) {
        let mut from = end + 1;
        if matches!(chars.get(from), Some('+' | '-')) {
            from += 1;
        }
        let after = digits(from);
        if after == from {
            return Err(invalid());
        }
        end = after;
        plain = false;
    }
    let text: String = chars.get(start..end).unwrap_or_default().iter().collect();
    let value: f64 = text.parse().map_err(|_| invalid())?;
    if !value.is_finite() {
        return Err(invalid());
    }
    let integer = if plain { text.parse().ok() } else { None };
    Ok((TokenKind::Number { value, integer }, end))
}

// ---------------------------------------------------------------------------------------------
// Parser

struct Parser {
    tokens: Vec<Token>,
    next: usize,
    /// One past the last character, for errors at the end.
    end: u32,
    nodes: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.next)
    }

    fn take(&mut self) -> Result<Token, FormulaError> {
        let token = self
            .tokens
            .get(self.next)
            .cloned()
            .ok_or(FormulaError::at(FormulaErrorClass::UnexpectedEnd, self.end))?;
        self.next += 1;
        Ok(token)
    }

    fn expect(&mut self, kind: &TokenKind) -> Result<(), FormulaError> {
        let token = self.take()?;
        if token.kind == *kind {
            Ok(())
        } else {
            Err(FormulaError::at(
                FormulaErrorClass::UnexpectedToken,
                token.at,
            ))
        }
    }

    fn node(&mut self, at: u32) -> Result<(), FormulaError> {
        self.nodes += 1;
        if self.nodes > MAX_FORMULA_NODES {
            return Err(FormulaError::at(FormulaErrorClass::TooComplex, at));
        }
        Ok(())
    }

    fn expr(&mut self, depth: usize) -> Result<Expr, FormulaError> {
        let mut left = self.term(depth)?;
        while let Some(token) = self.peek() {
            let operator = match token.kind {
                TokenKind::Plus => Operator::Add,
                TokenKind::Minus => Operator::Subtract,
                _ => break,
            };
            let at = token.at;
            self.next += 1;
            self.node(at)?;
            let right = self.term(depth)?;
            left = Expr::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn term(&mut self, depth: usize) -> Result<Expr, FormulaError> {
        let mut left = self.unary(depth)?;
        while let Some(token) = self.peek() {
            let operator = match token.kind {
                TokenKind::Star => Operator::Multiply,
                TokenKind::Slash => Operator::Divide,
                _ => break,
            };
            let at = token.at;
            self.next += 1;
            self.node(at)?;
            let right = self.unary(depth)?;
            left = Expr::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn unary(&mut self, depth: usize) -> Result<Expr, FormulaError> {
        if let Some(token) = self.peek()
            && token.kind == TokenKind::Minus
        {
            let at = token.at;
            if depth >= MAX_FORMULA_DEPTH {
                return Err(FormulaError::at(FormulaErrorClass::TooComplex, at));
            }
            self.next += 1;
            self.node(at)?;
            return Ok(Expr::Negate(Box::new(self.unary(depth + 1)?)));
        }
        self.primary(depth)
    }

    fn primary(&mut self, depth: usize) -> Result<Expr, FormulaError> {
        let token = self.take()?;
        match token.kind {
            TokenKind::Number { value, .. } => {
                self.node(token.at)?;
                Ok(Expr::Number(value))
            }
            TokenKind::LeftParen => {
                if depth >= MAX_FORMULA_DEPTH {
                    return Err(FormulaError::at(FormulaErrorClass::TooComplex, token.at));
                }
                let inner = self.expr(depth + 1)?;
                self.expect(&TokenKind::RightParen)?;
                Ok(inner)
            }
            TokenKind::Ident(name) => self.named(name, token.at),
            _ => Err(FormulaError::at(
                FormulaErrorClass::UnexpectedToken,
                token.at,
            )),
        }
    }

    /// An aggregate call or an indexed column, starting with the identifier `name`.
    fn named(&mut self, name: String, at: u32) -> Result<Expr, FormulaError> {
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::LeftParen) => {
                let function = Aggregate::parse(&name)
                    .ok_or(FormulaError::at(FormulaErrorClass::UnknownFunction, at))?;
                self.next += 1;
                let column_at = self.peek().map_or(self.end, |token| token.at);
                let first = match self.take()?.kind {
                    TokenKind::Ident(first) => first,
                    _ => {
                        return Err(FormulaError::at(
                            FormulaErrorClass::UnexpectedToken,
                            column_at,
                        ));
                    }
                };
                let column = self.column(first, column_at)?;
                self.expect(&TokenKind::RightParen)?;
                self.node(at)?;
                Ok(Expr::Aggregate {
                    function,
                    column,
                    at,
                })
            }
            Some(TokenKind::Dot) => {
                let column = self.column(name, at)?;
                match self.peek() {
                    Some(token) if token.kind == TokenKind::LeftBracket => {}
                    _ => return Err(FormulaError::at(FormulaErrorClass::BareColumn, at)),
                }
                self.next += 1;
                let index = self.take()?;
                let row = match index.kind {
                    TokenKind::Number {
                        integer: Some(row), ..
                    } if row < MAX_DATASET_ROWS => row,
                    TokenKind::Number { value, .. }
                        if value.fract() == 0.0 && value >= f64::from(MAX_DATASET_ROWS) =>
                    {
                        return Err(FormulaError::at(
                            FormulaErrorClass::IndexOutOfRange,
                            index.at,
                        ));
                    }
                    _ => {
                        return Err(FormulaError::at(
                            FormulaErrorClass::UnexpectedToken,
                            index.at,
                        ));
                    }
                };
                self.expect(&TokenKind::RightBracket)?;
                self.node(at)?;
                Ok(Expr::Index { column, row, at })
            }
            _ => Err(FormulaError::at(FormulaErrorClass::UnexpectedToken, at)),
        }
    }

    /// `dataset "." column`, after the dataset's identifier was taken.
    fn column(&mut self, dataset: String, at: u32) -> Result<ColumnRef, FormulaError> {
        if !is_name(&dataset) {
            return Err(FormulaError::at(FormulaErrorClass::InvalidName, at));
        }
        self.expect(&TokenKind::Dot)?;
        let token = self.take()?;
        let TokenKind::Ident(column) = token.kind else {
            return Err(FormulaError::at(
                FormulaErrorClass::UnexpectedToken,
                token.at,
            ));
        };
        if !is_column_name(&column) {
            return Err(FormulaError::at(FormulaErrorClass::InvalidName, token.at));
        }
        Ok(ColumnRef { dataset, column })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Data(BTreeMap<(&'static str, &'static str), Vec<Cell>>);

    impl FormulaData for Data {
        fn cells(&self, dataset: &str, column: &str) -> Option<Vec<Cell>> {
            self.0
                .iter()
                .find(|((d, c), _)| *d == dataset && *c == column)
                .map(|(_, cells)| cells.clone())
        }
    }

    fn error(text: &str) -> (FormulaErrorClass, Option<u32>) {
        let error = Formula::parse(text).expect_err(text);
        (error.class, error.position)
    }

    #[test]
    fn precedence_and_associativity() {
        let tree = |text: &str| Formula::parse(text).expect(text).to_sexpr();
        assert_eq!(tree("1 - 2 - 3"), json!(["-", ["-", 1.0, 2.0], 3.0]));
        assert_eq!(tree("1 + 2 * 3"), json!(["+", 1.0, ["*", 2.0, 3.0]]));
        assert_eq!(tree("-2 * 3"), json!(["*", ["neg", 2.0], 3.0]));
        assert_eq!(tree("8 / 4 / 2"), json!(["/", ["/", 8.0, 4.0], 2.0]));
        assert_eq!(
            tree("sum(a.kwh) / count(a.kwh)"),
            json!(["/", ["sum", "a.kwh"], ["count", "a.kwh"]])
        );
        assert_eq!(tree("a.Kwh[3]"), json!(["at", "a.Kwh", 3]));
        assert_eq!(tree("--(1)"), json!(["neg", ["neg", 1.0]]));
    }

    #[test]
    fn parse_errors_have_a_class_and_a_position() {
        use FormulaErrorClass::*;
        assert_eq!(error(" \t\n"), (Empty, None));
        assert_eq!(error(&"1+".repeat(300)), (TooLong, None));
        assert_eq!(error("1 % 2"), (UnexpectedCharacter, Some(3)));
        assert_eq!(error("用電"), (UnexpectedCharacter, Some(1)));
        assert_eq!(error("1 2"), (UnexpectedToken, Some(3)));
        assert_eq!(error("sum(a.kwh"), (UnexpectedEnd, Some(10)));
        assert_eq!(error("total(a.kwh)"), (UnknownFunction, Some(1)));
        assert_eq!(error("SUM(a.kwh)"), (UnknownFunction, Some(1)));
        assert_eq!(error("Daily.kwh[0]"), (InvalidName, Some(1)));
        assert_eq!(error("a.kwh"), (BareColumn, Some(1)));
        assert_eq!(error("a.kwh + 1"), (BareColumn, Some(1)));
        assert_eq!(error("kwh"), (UnexpectedToken, Some(1)));
        assert_eq!(error("1."), (InvalidNumber, Some(1)));
        assert_eq!(error("2e"), (InvalidNumber, Some(1)));
        assert_eq!(error("1e999"), (InvalidNumber, Some(1)));
        assert_eq!(error("a.kwh[2000]"), (IndexOutOfRange, Some(7)));
        assert_eq!(error("a.kwh[1.5]"), (UnexpectedToken, Some(7)));
        assert_eq!(error("sum(a.kwh + 1)"), (UnexpectedToken, Some(11)));
        assert_eq!(error("sum(1)"), (UnexpectedToken, Some(5)));
        assert_eq!(
            error(&format!("{}1{}", "(".repeat(33), ")".repeat(33))),
            (TooComplex, Some(33))
        );
        assert_eq!(
            error(&format!("{}1", "-".repeat(33))),
            (TooComplex, Some(33))
        );
        assert_eq!(error(&vec!["1"; 33].join("+")), (TooComplex, Some(65)));
        assert!(Formula::parse(&vec!["1"; 32].join("+")).is_ok());
    }

    #[test]
    fn values_follow_the_null_rules() {
        let data = Data(BTreeMap::from([
            (
                ("a", "kwh"),
                vec![
                    Cell::Number(1.5),
                    Cell::Null,
                    Cell::Number(-2.0),
                    Cell::Number(4.0),
                ],
            ),
            (
                ("a", "name"),
                vec![Cell::Other, Cell::Null, Cell::Other, Cell::Other],
            ),
            (("e", "kwh"), vec![]),
            (("n", "kwh"), vec![Cell::Null, Cell::Null]),
        ]));
        let value = |text: &str| Formula::parse(text).expect(text).evaluate(&data);
        assert_eq!(value("sum(a.kwh)"), Some(3.5));
        assert_eq!(value("avg(a.kwh)"), Some(3.5 / 3.0));
        assert_eq!(value("min(a.kwh)"), Some(-2.0));
        assert_eq!(value("max(a.kwh)"), Some(4.0));
        assert_eq!(value("count(a.kwh)"), Some(3.0));
        assert_eq!(value("count(a.name)"), Some(3.0));
        assert_eq!(value("first(a.kwh)"), Some(1.5));
        assert_eq!(value("last(a.kwh)"), Some(4.0));
        assert_eq!(value("a.kwh[1]"), None);
        assert_eq!(value("a.kwh[9]"), None);
        assert_eq!(value("sum(e.kwh)"), None);
        assert_eq!(value("count(e.kwh)"), Some(0.0));
        assert_eq!(value("first(e.kwh)"), None);
        assert_eq!(value("sum(n.kwh)"), None);
        assert_eq!(value("first(n.kwh)"), None);
        assert_eq!(value("sum(a.kwh) / 0"), None);
        assert_eq!(value("0 / 0"), None);
        assert_eq!(value("1e308 * 10"), None);
        assert_eq!(value("a.kwh[1] + 1"), None);
        assert_eq!(value("sum(missing.kwh)"), None);
        assert_eq!(value("-a.kwh[0] * 2"), Some(-3.0));
    }

    #[test]
    fn checks_compare_columns_with_the_known_datasets() {
        let shapes = |name: &str| match name {
            "sem" => Some(DatasetShape {
                max_rows: 10,
                columns: Some(vec![
                    ("usage".to_owned(), ColumnKind::Number),
                    ("point".to_owned(), ColumnKind::Text),
                ]),
            }),
            "sql" => Some(DatasetShape {
                max_rows: 2000,
                columns: None,
            }),
            _ => None,
        };
        let check = |text: &str| Formula::parse(text).expect(text).check(shapes);
        assert_eq!(check("sum(sem.usage) + sql.anything[1999]"), Ok(()));
        assert_eq!(check("count(sem.point)"), Ok(()));
        let class = |text: &str| check(text).expect_err(text).class;
        assert_eq!(class("sum(nope.kwh)"), FormulaErrorClass::UnknownDataset);
        assert_eq!(class("sum(sem.kwh)"), FormulaErrorClass::UnknownColumn);
        assert_eq!(class("max(sem.point)"), FormulaErrorClass::ColumnNotNumeric);
        assert_eq!(class("sem.point[0]"), FormulaErrorClass::ColumnNotNumeric);
        assert_eq!(class("sem.usage[10]"), FormulaErrorClass::IndexOutOfRange);
        assert_eq!(
            check("1 + sum(nope.kwh)"),
            Err(FormulaError::at(FormulaErrorClass::UnknownDataset, 5))
        );
    }

    #[test]
    fn columns_are_listed_in_order() {
        let formula = Formula::parse("sum(a.x) / b.y[2] - count(a.z)").expect("parse");
        let columns: Vec<(String, Option<Aggregate>)> = formula
            .columns()
            .into_iter()
            .map(|(column, function)| (column.to_string(), function))
            .collect();
        assert_eq!(
            columns,
            vec![
                ("a.x".to_owned(), Some(Aggregate::Sum)),
                ("b.y".to_owned(), None),
                ("a.z".to_owned(), Some(Aggregate::Count)),
            ]
        );
    }

    #[test]
    fn every_class_has_a_message_and_a_wire_name() {
        for class in FormulaErrorClass::ALL {
            assert!(!class.message().is_empty());
            let wire = serde_json::to_value(class).expect("serialize");
            assert_eq!(
                serde_json::from_value::<FormulaErrorClass>(wire).expect("deserialize"),
                class
            );
        }
    }
}
