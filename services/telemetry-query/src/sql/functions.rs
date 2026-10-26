//! Step 6 of the check: the functions an AI's SQL may call. An allowlist, so a function a newer
//! ClickHouse adds stays out until someone adds it here.
//!
//! Never listed: table functions, settings, user, role, profile, database, host, version, query
//! and port functions, dictionaries (`dictGet*`, `dictHas`), `joinGet`, `hasColumnInTable`,
//! `throwIf`, `sleep*`, the function forms of `IN` (`in`, `notIn`, `globalIn`),
//! `initializeAggregation`, `runningAccumulate`, model evaluation, generators such as `range`
//! and `arrayJoin`, `formatQuery`, the `-State`, `-Merge` and `-Resample` combinators, and
//! internal functions. `today` and `yesterday` are left out because they read the server's time
//! zone; `toDate(now(), '<zone>')` says which day.
//!
//! The container test `sql_api.rs` checks every entry against the pinned ClickHouse's
//! `system.functions`: the name exists, its `case_insensitive` matches, and the function an alias
//! names is listed too.

/// What a function does, which decides where it may appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A plain function.
    Scalar,
    /// A plain function whose first argument may be a lambda (`x -> …`).
    HigherOrder,
    /// An aggregate: `DISTINCT` arguments and `OVER` are allowed.
    Aggregate,
    /// A window function: `OVER` is allowed, combinators are not.
    Window,
}

/// Where a function that buckets, formats or reads time takes its time zone, which must then
/// be a string literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeZone {
    /// Always: the call has exactly `arity` arguments and the last is the zone.
    Always(usize),
    /// As [`Self::Always`], unless the first argument is a string literal (`toDate('2026-10-01')`
    /// names a day in no zone).
    UnlessText(usize),
    /// As [`Self::Always`], but only when the first argument is a string literal (reading a
    /// time from text).
    WhenText(usize),
}

/// One allowed function.
#[derive(Clone, Copy, Debug)]
pub struct Function {
    /// The name as ClickHouse defines it; the checked SQL always spells it so.
    pub name: &'static str,
    /// Whether ClickHouse matches the name in any case.
    pub case_insensitive: bool,
    pub kind: Kind,
    /// Whether it takes literal parameters before its arguments (`quantile(0.9)(x)`).
    pub parametric: bool,
    /// Whether the combinators of [`COMBINATORS`] may extend it (`sumIf`).
    pub combinable: bool,
    pub time_zone: Option<TimeZone>,
}

const fn scalar(name: &'static str) -> Function {
    Function {
        name,
        case_insensitive: false,
        kind: Kind::Scalar,
        parametric: false,
        combinable: false,
        time_zone: None,
    }
}

const fn aggregate(name: &'static str) -> Function {
    Function {
        kind: Kind::Aggregate,
        combinable: true,
        ..scalar(name)
    }
}

const fn window(name: &'static str) -> Function {
    Function {
        kind: Kind::Window,
        ..scalar(name)
    }
}

const fn higher_order(name: &'static str) -> Function {
    Function {
        kind: Kind::HigherOrder,
        ..scalar(name)
    }
}

impl Function {
    const fn any_case(self) -> Self {
        Self {
            case_insensitive: true,
            ..self
        }
    }

    const fn parametric(self) -> Self {
        Self {
            parametric: true,
            ..self
        }
    }

    const fn alone(self) -> Self {
        Self {
            combinable: false,
            ..self
        }
    }

    const fn zone(self, time_zone: TimeZone) -> Self {
        Self {
            time_zone: Some(time_zone),
            ..self
        }
    }
}

/// The aggregate function combinators an aggregate of [`FUNCTIONS`] may carry, as name
/// suffixes: at most one of these.
pub const COMBINATORS: &[&str] = &[
    "IfOrDefault",
    "IfOrNull",
    "DistinctIf",
    "OrDefault",
    "Distinct",
    "OrNull",
    "If",
];

/// The allowed functions.
pub const FUNCTIONS: &[Function] = &[
    // Aggregates.
    aggregate("count").any_case(),
    aggregate("sum").any_case(),
    aggregate("avg").any_case(),
    aggregate("min").any_case(),
    aggregate("max").any_case(),
    aggregate("any"),
    aggregate("anyLast"),
    aggregate("argMin"),
    aggregate("argMax"),
    aggregate("uniq"),
    aggregate("uniqExact"),
    aggregate("quantile").parametric(),
    aggregate("quantiles").parametric(),
    aggregate("quantileExact").parametric(),
    aggregate("quantilesExact").parametric(),
    aggregate("quantileTDigest").parametric(),
    aggregate("median").parametric(),
    aggregate("medianExact").parametric(),
    aggregate("stddevPop"),
    aggregate("stddevSamp"),
    aggregate("varPop"),
    aggregate("varSamp"),
    aggregate("corr").any_case(),
    aggregate("covarPop"),
    aggregate("covarSamp"),
    aggregate("groupArray").parametric(),
    aggregate("topK").parametric(),
    aggregate("avgWeighted"),
    aggregate("deltaSum"),
    aggregate("simpleLinearRegression"),
    aggregate("first_value").any_case().alone(),
    aggregate("last_value").any_case().alone(),
    // Window functions.
    window("row_number").any_case(),
    window("rank").any_case(),
    window("dense_rank").any_case(),
    window("denseRank"),
    window("percent_rank").any_case(),
    window("percentRank"),
    window("nth_value").any_case(),
    window("lagInFrame"),
    window("leadInFrame"),
    // Time.
    scalar("now").any_case(),
    scalar("now64").any_case(),
    scalar("toStartOfMinute"),
    scalar("toStartOfFiveMinutes"),
    scalar("toStartOfTenMinutes"),
    scalar("toStartOfFifteenMinutes"),
    scalar("toStartOfHour").zone(TimeZone::Always(2)),
    scalar("toStartOfDay").zone(TimeZone::Always(2)),
    scalar("toStartOfWeek").zone(TimeZone::Always(3)),
    scalar("toStartOfMonth").zone(TimeZone::Always(2)),
    scalar("toStartOfQuarter").zone(TimeZone::Always(2)),
    scalar("toStartOfYear").zone(TimeZone::Always(2)),
    scalar("toStartOfInterval").zone(TimeZone::Always(3)),
    scalar("toDate").zone(TimeZone::UnlessText(2)),
    scalar("toDateTime").zone(TimeZone::WhenText(2)),
    scalar("toDateTime64").zone(TimeZone::WhenText(3)),
    scalar("parseDateTimeBestEffort").zone(TimeZone::WhenText(2)),
    scalar("toHour").zone(TimeZone::Always(2)),
    scalar("toMinute"),
    scalar("toSecond"),
    scalar("toDayOfWeek").zone(TimeZone::Always(3)),
    scalar("toDayOfMonth").zone(TimeZone::Always(2)),
    scalar("toDayOfYear").zone(TimeZone::Always(2)),
    scalar("toMonth").zone(TimeZone::Always(2)),
    scalar("toYear").zone(TimeZone::Always(2)),
    scalar("toYYYYMM").zone(TimeZone::Always(2)),
    scalar("toYYYYMMDD").zone(TimeZone::Always(2)),
    scalar("formatDateTime").zone(TimeZone::Always(3)),
    scalar("dateDiff").any_case().zone(TimeZone::Always(4)),
    scalar("addSeconds"),
    scalar("addMinutes"),
    scalar("addHours"),
    scalar("addDays"),
    scalar("addWeeks"),
    scalar("addMonths"),
    scalar("addYears"),
    scalar("subtractSeconds"),
    scalar("subtractMinutes"),
    scalar("subtractHours"),
    scalar("subtractDays"),
    scalar("subtractWeeks"),
    scalar("subtractMonths"),
    scalar("subtractYears"),
    scalar("toUnixTimestamp"),
    scalar("toUnixTimestamp64Milli"),
    scalar("fromUnixTimestamp64Milli"),
    scalar("toTimeZone"),
    scalar("toTimezone"),
    // Conversion. `CAST` and `::` become these (check.rs).
    scalar("toFloat64"),
    scalar("toFloat32"),
    scalar("toInt64"),
    scalar("toInt32"),
    scalar("toUInt64"),
    scalar("toUInt32"),
    scalar("toUInt8"),
    scalar("toString"),
    scalar("toFloat64OrNull"),
    scalar("toInt64OrNull"),
    scalar("toTypeName"),
    // Nulls.
    scalar("ifNull").any_case(),
    scalar("coalesce").any_case(),
    scalar("nullIf").any_case(),
    scalar("isNull").any_case(),
    scalar("isNotNull"),
    scalar("assumeNotNull"),
    // Arithmetic. `floor` and `ceil` are what `FLOOR(x)` and `CEIL(x)` become.
    scalar("abs").any_case(),
    scalar("round").any_case(),
    scalar("roundBankers"),
    scalar("floor").any_case(),
    scalar("ceil").any_case(),
    scalar("trunc").any_case(),
    scalar("sqrt").any_case(),
    scalar("pow").any_case(),
    scalar("power").any_case(),
    scalar("exp").any_case(),
    scalar("log").any_case(),
    scalar("ln").any_case(),
    scalar("log10").any_case(),
    scalar("log2").any_case(),
    scalar("greatest").any_case(),
    scalar("least").any_case(),
    scalar("sign").any_case(),
    scalar("pi").any_case(),
    scalar("intDiv"),
    scalar("intDivOrZero"),
    scalar("modulo"),
    scalar("isNaN"),
    scalar("isFinite"),
    scalar("isInfinite"),
    // Conditions.
    scalar("if").any_case(),
    scalar("multiIf"),
    // Strings. `substring` is what `SUBSTRING(s, a, b)` becomes; `trim` is left out because
    // sqlparser reads it as the SQL-standard `TRIM`.
    scalar("length").any_case(),
    scalar("lower").any_case(),
    scalar("upper").any_case(),
    scalar("concat").any_case(),
    scalar("substring").any_case(),
    scalar("position").any_case(),
    scalar("startsWith"),
    scalar("endsWith"),
    scalar("trimBoth"),
    scalar("trimLeft"),
    scalar("trimRight"),
    scalar("lowerUTF8"),
    scalar("upperUTF8"),
    scalar("lengthUTF8"),
    scalar("substringUTF8"),
    scalar("replaceAll"),
    scalar("leftPad"),
    scalar("rightPad"),
    // Tuples, such as the `first` and `last` columns of the cell views.
    scalar("tuple"),
    scalar("tupleElement"),
    // Arrays.
    scalar("has"),
    scalar("indexOf"),
    scalar("empty"),
    scalar("notEmpty"),
    scalar("arrayElement"),
    scalar("arraySlice"),
    scalar("arrayReverse"),
    scalar("arrayDistinct"),
    scalar("arrayConcat"),
    scalar("arrayDifference"),
    higher_order("arrayMap"),
    higher_order("arrayFilter"),
    higher_order("arrayCount"),
    higher_order("arrayExists"),
    higher_order("arrayAll"),
    higher_order("arrayFirst"),
    higher_order("arrayLast"),
    higher_order("arraySort"),
    higher_order("arrayReverseSort"),
    higher_order("arraySum"),
    higher_order("arrayMin"),
    higher_order("arrayMax"),
    higher_order("arrayAvg"),
    higher_order("arrayCumSum"),
];

/// An allowed function a call names.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The spelling the checked SQL uses: the defined name, with the combinator.
    pub canonical: String,
    pub function: &'static Function,
    /// Whether a combinator extends it.
    pub combined: bool,
}

/// The allowed function `name` calls, if any: an entry of [`FUNCTIONS`], in any case when the
/// entry allows it, or a combinable aggregate with one of [`COMBINATORS`].
#[must_use]
pub fn resolve(name: &str) -> Option<Resolved> {
    if let Some(function) = entry(name) {
        return Some(Resolved {
            canonical: function.name.to_owned(),
            function,
            combined: false,
        });
    }
    COMBINATORS.iter().find_map(|combinator| {
        let base = name.strip_suffix(combinator)?;
        let function = entry(base).filter(|function| function.combinable)?;
        Some(Resolved {
            canonical: format!("{}{combinator}", function.name),
            function,
            combined: true,
        })
    })
}

fn entry(name: &str) -> Option<&'static Function> {
    FUNCTIONS.iter().find(|function| {
        if function.case_insensitive {
            function.name.eq_ignore_ascii_case(name)
        } else {
            function.name == name
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn names_are_unique_and_never_a_refused_function() {
        let mut seen = HashSet::new();
        for function in FUNCTIONS {
            assert!(seen.insert(function.name), "{} twice", function.name);
            // Names that differ only in case (`toTimeZone`, `toTimezone`) are both
            // ClickHouse's; one that matches in any case would make the lookup ambiguous.
            let same_but_case = FUNCTIONS
                .iter()
                .filter(|other| other.name.eq_ignore_ascii_case(function.name))
                .count();
            assert!(
                !function.case_insensitive || same_but_case == 1,
                "{} matches another name in any case",
                function.name
            );
            assert!(
                !function.combinable || function.kind == Kind::Aggregate,
                "{}",
                function.name
            );
            assert!(
                !function.parametric || function.kind == Kind::Aggregate,
                "{}",
                function.name
            );
        }
        for refused in [
            "getSetting",
            "currentUser",
            "currentRoles",
            "currentDatabase",
            "hostName",
            "version",
            "queryID",
            "dictGet",
            "joinGet",
            "hasColumnInTable",
            "throwIf",
            "sleep",
            "in",
            "notIn",
            "globalIn",
            "initializeAggregation",
            "runningAccumulate",
            "range",
            "arrayJoin",
            "formatQuery",
            "today",
            "yesterday",
            "trim",
            "numbers",
            "remote",
            "merge",
            "file",
            "url",
            "s3",
            "sumState",
            "sumMerge",
            "sumResample",
            "row_numberIf",
            "first_valueIf",
            "sumIfIf",
            "sumIfState",
        ] {
            assert!(resolve(refused).is_none(), "{refused}");
        }
    }

    #[test]
    fn names_resolve_to_their_canonical_spelling() {
        for (name, canonical, combined) in [
            ("count", "count", false),
            ("COUNT", "count", false),
            ("countIf", "countIf", true),
            ("COUNTIf", "countIf", true),
            ("Sum", "sum", false),
            ("avgIfOrNull", "avgIfOrNull", true),
            ("sumDistinctIf", "sumDistinctIf", true),
            ("argMaxIf", "argMaxIf", true),
            ("quantileIf", "quantileIf", true),
            ("toStartOfHour", "toStartOfHour", false),
            ("toTimeZone", "toTimeZone", false),
            ("toTimezone", "toTimezone", false),
            ("DENSE_RANK", "dense_rank", false),
            ("denseRank", "denseRank", false),
            ("IF", "if", false),
            ("nullIf", "nullIf", false),
        ] {
            let resolved = resolve(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(resolved.canonical, canonical, "{name}");
            assert_eq!(resolved.combined, combined, "{name}");
        }
        // Case-sensitive names match only as ClickHouse spells them.
        for name in [
            "TOSTARTOFHOUR",
            "tostartofhour",
            "Any",
            "multiif",
            "totimezone",
            "DenseRank",
        ] {
            assert!(resolve(name).is_none(), "{name}");
        }
    }
}
