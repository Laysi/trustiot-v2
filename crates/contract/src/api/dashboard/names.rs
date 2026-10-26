//! Names and references.
//!
//! A dashboard names its datasets, computations and constants with [`is_name`] names, one
//! namespace for all three. A dataset's columns carry the names its query gave them, which must
//! pass [`is_column_name`]: a formula writes `dataset.column`, and a reference separates its
//! parts with `|`.
//!
//! A reference ([`Ref`]) says where a value on the page comes from. The runtime writes it into
//! the `data-tq` attribute of every element that shows a value (`frontend/sandbox/README.md`,
//! 「參照」), and the check resolves it against the data it was given:
//!
//! | Form | Value |
//! | --- | --- |
//! | `c\|<dataset>\|<column>\|<row>` | one cell; `row` is the 0-based index in the result, below [`MAX_DATASET_ROWS`], without leading zeros |
//! | `f\|<computation>` | a computation's value |
//! | `k\|<constant>` | a constant's value |
//! | `m\|<dataset>\|<field>` | a fact about a dataset's run: [`MetaField`] |
//! | `t\|<dataset>\|<column>\|<value>` | an axis tick over a column: a number as JavaScript's `String(number)` writes it, or a time as `YYYY-MM-DDThh:mm:ss.sss` (the wall-clock time, no zone) |
//!
//! Every reference has exactly one spelling, so the runtime, the check and the report compare
//! them as text.

use std::fmt;
use std::str::FromStr;

use super::result::MAX_DATASET_ROWS;

/// Longest dataset, computation or constant name, in bytes: long enough to describe the data
/// (`daily_energy_line_b`), short enough to keep references and formulas readable.
pub const MAX_NAME_LEN: usize = 48;

/// [`is_name`] as a regular expression, for schemas and the TypeScript side.
pub const NAME_PATTERN: &str = "^[a-z][a-z0-9_]{0,47}$";

/// Longest column name, in bytes. Telemetry columns and semantic columns are far shorter; a
/// query names its computed columns with `AS`.
pub const MAX_COLUMN_NAME_LEN: usize = 64;

/// [`is_column_name`] as a regular expression.
pub const COLUMN_NAME_PATTERN: &str = "^[A-Za-z_][A-Za-z0-9_]{0,63}$";

/// Longest reference, in bytes: a tick reference with the longest names and value.
pub const MAX_REF_LEN: usize = 160;

/// Longest number text in a tick reference.
const MAX_TICK_NUMBER_LEN: usize = 32;

/// Whether `name` is a dataset, computation or constant name: a lowercase ASCII letter, then
/// up to 47 lowercase letters, digits and `_`.
#[must_use]
pub fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= MAX_NAME_LEN
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// Whether `name` is a column name a dashboard can use: an ASCII letter or `_`, then up to 63
/// ASCII letters, digits and `_`.
#[must_use]
pub fn is_column_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= MAX_COLUMN_NAME_LEN
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// A fact about one run of a dataset, shown with an `m|…` reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetaField {
    /// When the query ran (a time).
    QueriedAt,
    /// How many rows the result has (a number).
    RowCount,
    /// The start of the range a semantic dataset covered (a time); unmapped for SQL.
    RangeFrom,
    /// The end of that range, exclusive (a time); unmapped for SQL.
    RangeTo,
}

impl MetaField {
    /// Every field.
    pub const ALL: [Self; 4] = [
        Self::QueriedAt,
        Self::RowCount,
        Self::RangeFrom,
        Self::RangeTo,
    ];

    /// The name in a reference.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QueriedAt => "queried_at",
            Self::RowCount => "row_count",
            Self::RangeFrom => "range_from",
            Self::RangeTo => "range_to",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == text)
    }
}

/// The value of an axis tick in a `t|…` reference, as its canonical text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TickValue {
    /// A number, as JavaScript's `String(number)` writes a finite number: `0`, `1500`, `-2.5`,
    /// `1.5e-7`, `1e+21`.
    Number(String),
    /// A wall-clock time, `YYYY-MM-DDThh:mm:ss.sss`, as `Date.toISOString()` writes it without
    /// the final `Z` (the runtime keeps wall-clock times in a `Date`'s UTC fields).
    Time(String),
}

impl TickValue {
    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Number(text) | Self::Time(text) => text,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        if is_js_number(text) {
            Some(Self::Number(text.to_owned()))
        } else if is_tick_time(text) {
            Some(Self::Time(text.to_owned()))
        } else {
            None
        }
    }
}

/// Where a value shown on a page comes from (see the module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ref {
    /// `c|dataset|column|row`.
    Cell {
        dataset: String,
        column: String,
        row: u32,
    },
    /// `f|name`.
    Computation { name: String },
    /// `k|name`.
    Constant { name: String },
    /// `m|dataset|field`.
    Meta { dataset: String, field: MetaField },
    /// `t|dataset|column|value`.
    Tick {
        dataset: String,
        column: String,
        value: TickValue,
    },
}

/// The text is not a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a dashboard reference")]
pub struct InvalidRef;

impl Ref {
    /// The dataset the reference reads, if it reads one directly.
    #[must_use]
    pub fn dataset(&self) -> Option<&str> {
        match self {
            Self::Cell { dataset, .. }
            | Self::Meta { dataset, .. }
            | Self::Tick { dataset, .. } => Some(dataset),
            Self::Computation { .. } | Self::Constant { .. } => None,
        }
    }
}

impl FromStr for Ref {
    type Err = InvalidRef;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() > MAX_REF_LEN {
            return Err(InvalidRef);
        }
        let parts: Vec<&str> = text.split('|').collect();
        let parsed = match parts.as_slice() {
            ["c", dataset, column, row] if is_name(dataset) && is_column_name(column) => {
                Self::Cell {
                    dataset: (*dataset).to_owned(),
                    column: (*column).to_owned(),
                    row: parse_row(row).ok_or(InvalidRef)?,
                }
            }
            ["f", name] if is_name(name) => Self::Computation {
                name: (*name).to_owned(),
            },
            ["k", name] if is_name(name) => Self::Constant {
                name: (*name).to_owned(),
            },
            ["m", dataset, field] if is_name(dataset) => Self::Meta {
                dataset: (*dataset).to_owned(),
                field: MetaField::parse(field).ok_or(InvalidRef)?,
            },
            ["t", dataset, column, value] if is_name(dataset) && is_column_name(column) => {
                Self::Tick {
                    dataset: (*dataset).to_owned(),
                    column: (*column).to_owned(),
                    value: TickValue::parse(value).ok_or(InvalidRef)?,
                }
            }
            _ => return Err(InvalidRef),
        };
        Ok(parsed)
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cell {
                dataset,
                column,
                row,
            } => write!(f, "c|{dataset}|{column}|{row}"),
            Self::Computation { name } => write!(f, "f|{name}"),
            Self::Constant { name } => write!(f, "k|{name}"),
            Self::Meta { dataset, field } => write!(f, "m|{dataset}|{}", field.as_str()),
            Self::Tick {
                dataset,
                column,
                value,
            } => write!(f, "t|{dataset}|{column}|{}", value.as_str()),
        }
    }
}

/// A row index: `0`, or a decimal without leading zeros, below [`MAX_DATASET_ROWS`].
fn parse_row(text: &str) -> Option<u32> {
    let canonical = text == "0"
        || (text
            .bytes()
            .next()
            .is_some_and(|byte| matches!(byte, b'1'..=b'9'))
            && text.bytes().all(|byte| byte.is_ascii_digit()));
    if !canonical || text.len() > 4 {
        return None;
    }
    let row: u32 = text.parse().ok()?;
    (row < MAX_DATASET_ROWS).then_some(row)
}

/// Whether `text` is a finite number exactly as JavaScript's `String(number)` writes it: the
/// text the runtime puts in a tick reference.
fn is_js_number(text: &str) -> bool {
    text.len() <= MAX_TICK_NUMBER_LEN
        && text
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .is_some_and(|number| js_number_string(number) == text)
}

/// A finite number as JavaScript's `String(number)` writes it (ECMA-262 `Number::toString`):
/// the shortest digits that read back as the same number, in plain decimal from 1e-6 up to
/// 1e21, and as `d.ddde±n` outside it; `0` for both zeros. Rust's `{}` and `{:e}` print the
/// same shortest digits.
pub fn js_number_string(number: f64) -> String {
    if number == 0.0 {
        return "0".to_owned();
    }
    let magnitude = number.abs();
    if (1e-6..1e21).contains(&magnitude) {
        return format!("{number}");
    }
    let scientific = format!("{number:e}");
    match scientific.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => scientific,
    }
}

/// Whether `text` is a wall-clock time `YYYY-MM-DDThh:mm:ss.sss` that exists, as
/// `Date.toISOString()` writes it without the final `Z`.
fn is_tick_time(text: &str) -> bool {
    const SHAPE: &[u8] = b"dddd-dd-ddTdd:dd:dd.ddd";
    text.len() == SHAPE.len()
        && text.bytes().zip(SHAPE).all(|(byte, shape)| match shape {
            b'd' => byte.is_ascii_digit(),
            other => byte == *other,
        })
        && chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.3f").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_their_patterns() {
        for name in ["a", "daily_energy", "line_b2", &"a".repeat(MAX_NAME_LEN)] {
            assert!(is_name(name), "{name}");
        }
        for name in [
            "",
            "Daily",
            "1st",
            "_x",
            "daily-energy",
            "能源",
            &"a".repeat(MAX_NAME_LEN + 1),
        ] {
            assert!(!is_name(name), "{name:?}");
        }
        for name in ["kwh", "Kwh_Total", "_bucket", "x1", &"c".repeat(64)] {
            assert!(is_column_name(name), "{name}");
        }
        for name in ["", "1x", "sum(kwh)", "a.b", "a|b", "用電", &"c".repeat(65)] {
            assert!(!is_column_name(name), "{name:?}");
        }
    }

    #[test]
    fn references_parse_and_print_back() {
        for text in [
            "c|daily|kwh|0",
            "c|daily|Kwh_Total|1999",
            "f|ratio_ab",
            "k|target",
            "m|daily|queried_at",
            "m|daily|row_count",
            "m|line_b|range_from",
            "m|line_b|range_to",
            "t|daily|kwh|0",
            "t|daily|kwh|1500",
            "t|daily|kwh|-2.5",
            "t|daily|kwh|0.25",
            "t|daily|kwh|1.5e-7",
            "t|daily|kwh|1e+21",
            "t|daily|kwh|100000",
            "t|daily|kwh|0.000001",
            "t|daily|kwh|1e-7",
            "t|daily|kwh|0.30000000000000004",
            "t|daily|kwh|-1.7976931348623157e+308",
            "t|daily|day|2026-10-20T00:00:00.000",
        ] {
            let parsed: Ref = text.parse().unwrap_or_else(|_| panic!("{text}"));
            assert_eq!(parsed.to_string(), text);
        }
        assert_eq!(
            "c|daily|kwh|31".parse::<Ref>(),
            Ok(Ref::Cell {
                dataset: "daily".to_owned(),
                column: "kwh".to_owned(),
                row: 31
            })
        );
    }

    #[test]
    fn malformed_references_are_refused() {
        for text in [
            "",
            "c|daily|kwh",
            "c|daily|kwh|01",
            "c|daily|kwh|-1",
            "c|daily|kwh|2000",
            "c|daily|kwh|1.0",
            "c|Daily|kwh|0",
            "c|daily|sum(kwh)|0",
            "c|daily|kwh|0|",
            "f|",
            "f|Ratio",
            "k|a|b",
            "m|daily|duration",
            "x|daily",
            "t|daily|kwh|01",
            "t|daily|kwh|-0",
            "t|daily|kwh|1.50",
            "t|daily|kwh|1e21",
            "t|daily|kwh|10e+21",
            "t|daily|kwh|.5",
            "t|daily|kwh|NaN",
            "t|daily|kwh|Infinity",
            "t|daily|kwh|1e+5",
            "t|daily|kwh|0.0000001",
            "t|daily|kwh|0.3000000000000000444",
            "t|daily|kwh|+1",
            "t|daily|kwh|1E+21",
            "t|daily|kwh|inf",
            "t|daily|day|2026-10-20T00:00:00",
            "t|daily|day|2026-10-20T00:00:00.000Z",
            "t|daily|day|2026-02-30T00:00:00.000",
            "C|daily|kwh|0",
            " c|daily|kwh|0",
        ] {
            assert_eq!(text.parse::<Ref>(), Err(InvalidRef), "{text:?}");
        }
        let long = format!("t|{}|{}|1", "a".repeat(48), "b".repeat(64));
        assert!(long.len() < MAX_REF_LEN);
        assert!(long.parse::<Ref>().is_ok());
        assert!("x".repeat(MAX_REF_LEN + 1).parse::<Ref>().is_err());
    }

    #[test]
    fn numbers_print_as_javascript_does() {
        for (number, text) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1500.0, "1500"),
            (-2.5, "-2.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e-6, "0.000001"),
            (1.5e-7, "1.5e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (-1.25e22, "-1.25e+22"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
        ] {
            assert_eq!(js_number_string(number), text, "{number:e}");
        }
    }

    #[test]
    fn the_patterns_describe_the_checks() {
        assert_eq!(
            NAME_PATTERN,
            format!("^[a-z][a-z0-9_]{{0,{}}}$", MAX_NAME_LEN - 1)
        );
        assert_eq!(
            COLUMN_NAME_PATTERN,
            format!("^[A-Za-z_][A-Za-z0-9_]{{0,{}}}$", MAX_COLUMN_NAME_LEN - 1)
        );
    }
}
