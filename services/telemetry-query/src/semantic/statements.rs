//! The semantic API's statements: one per answer, aggregate view and bucket unit, all static
//! (tools/sql-check), every value a query parameter. Each reads the caller's tenant view of an
//! aggregate table (`trustiot.tenant_telemetry_1m` or `_1h`), whose row policy returns the
//! account's tenant only, and filters `tenant_id = {tenant:UUID}` itself too.
//!
//! Parameters: `{tenant:UUID}`; `{series:Array(Tuple(UUID, String))}`, the (device, data point)
//! pairs; `{from:Int64}` and `{to:Int64}`, the range in Unix seconds (cells whose start is in
//! it); `{tz:String}` and `{step:UInt32}` for the buckets; `{limit:UInt32}`. The usage also
//! takes `{window_from:Int64}` and `{window_to:Int64}`, what it reads around the range, and the
//! constants of its rules ([`super::usage`]).
//!
//! A row is one bucket of one series: the device ID, the data point, the bucket's start in Unix
//! seconds, then the bucket's figures. Buckets follow the time zone `tz` as ClickHouse's copy
//! of the IANA Time Zone Database places them: local hours, midnights, Mondays and first days
//! of the month; minutes count from the Unix epoch.

/// The bucket's start of a cell's `bucket` (a UTC minute or hour), per unit.
macro_rules! local_bucket {
    (minutes) => {
        "toStartOfInterval(bucket, toIntervalMinute({step:UInt32}), {tz:String})"
    };
    (hour) => {
        "toStartOfHour(bucket, {tz:String})"
    };
    (day) => {
        "toStartOfDay(bucket, {tz:String})"
    };
    // `toMonday(value[, timezone])` takes the time zone as its second argument, as
    // `toStartOfMonth` does: the local Monday's date, made the local midnight. tests/semantic.rs
    // runs `1w` buckets of both answers against ClickHouse and compares them with the readings.
    (week) => {
        "toDateTime(toMonday(bucket, {tz:String}), {tz:String})"
    };
    (month) => {
        "toDateTime(toStartOfMonth(bucket, {tz:String}), {tz:String})"
    };
    (total) => {
        "toDateTime({from:Int64}, 'UTC')"
    };
}

/// The statistics of each bucket: readings, numeric readings, the sum of the numeric values
/// (a bool counts as 0 or 1), the smallest and largest numeric value with the start of the cell
/// it was in, and the first and last reading by time with its typed value.
macro_rules! statistics {
    ($view:literal, $unit:tt) => {
        concat!(
            "SELECT toString(device_id), point, toUnixTimestamp(",
            local_bucket!($unit),
            ") AS local,
  sum(readings), sum(numeric_readings), sum(value_sum),
  min(value_min), toUnixTimestamp(argMin(bucket, value_min)),
  max(value_max), toUnixTimestamp(argMax(bucket, value_max)),
  argMin(tuple(toUnixTimestamp64Milli(first.ts), toString(first.value_type), first.value_float,
    first.value_int, first.value_bool, first.value_string), first.ts),
  argMax(tuple(toUnixTimestamp64Milli(last.ts), toString(last.value_type), last.value_float,
    last.value_int, last.value_bool, last.value_string), last.ts)
FROM trustiot.",
            $view,
            "
WHERE tenant_id = {tenant:UUID} AND (device_id, point) IN {series:Array(Tuple(UUID, String))}
  AND bucket >= toDateTime({from:Int64}, 'UTC') AND bucket < toDateTime({to:Int64}, 'UTC')
GROUP BY device_id, point, local
ORDER BY device_id, point, local
LIMIT {limit:UInt32}
FORMAT JSONCompactEachRow"
        )
    };
}

/// The usage of each bucket of a cumulative data point (services/telemetry-query/README.md,
/// 「用量」), from the cells of the window around the range, in time order per series:
///
/// - A cell's first, last, minimum and maximum value; it counts only when its first and last
///   reading are numbers (int or float) and all four are finite (`usable`).
/// - Inside a cell with two or more readings: `last - first`, or, when the value fell inside it
///   (`last < max` or `min < first`), `(max - first) + last`: up to the peak, then from 0.
/// - Across from the previous cell: `first - previous last`, or `first` when the value fell:
///   a drop is a reset or a wrap that started again from 0.
/// - Each difference belongs to the cell of its later reading, so buckets add up.
/// - The cumulative check over the whole window: drops, rises (a comparison where the value
///   went up) and comparisons.
/// - Gaps: a spacing between a cell's first reading and the previous cell's last of at least
///   `{gap_factor}` times the series' median spacing.
/// - Suspicious steps: a positive difference whose rate, over at least the median spacing, is
///   more than `{factor}` times the series' median rate of the rises across cells, when at least
///   `{min_rises}` such rises exist.
///
/// Only the cells of the range are summed into buckets; the window's cells before it give the
/// first cell's previous reading (the baseline).
macro_rules! usage {
    ($view:literal, $unit:tt) => {
        concat!(
            "SELECT toString(device_id), point, toUnixTimestamp(",
            local_bucket!($unit),
            ") AS local,
  sum(inside + across), sum(readings), countIf(inside_drop) + countIf(across_drop),
  countIf(NOT usable), countIf(non_numeric), countIf(usable),
  countIf(gap), maxIf(spacing_ms, gap), argMaxIf(prev_last_ms, spacing_ms, gap),
  countIf(inside_suspicious) + countIf(across_suspicious),
  maxIf(inside, inside_suspicious), argMaxIf(last_ms, inside, inside_suspicious),
  maxIf(across, across_suspicious), argMaxIf(first_ms, across, across_suspicious),
  minIf(first_ms, usable), argMinIf(f, first_ms, usable),
  maxIf(last_ms, usable), argMaxIf(l, last_ms, usable),
  any(check_drops), any(check_rises), any(check_comparisons), toUnixTimestamp(any(window_first))
FROM (
  SELECT *,
    countIf(inside_drop) OVER whole + countIf(across_drop) OVER whole AS check_drops,
    countIf(inside_rise) OVER whole + countIf(across_rise) OVER whole AS check_rises,
    countIf(inside_compared) OVER whole + countIf(across_compared) OVER whole AS check_comparisons,
    min(bucket) OVER whole AS window_first,
    countIf(across_rise) OVER whole AS rate_basis,
    medianExactIf(across * 1000 / greatest(spacing_ms, 1), across_rise) OVER whole AS median_rate,
    medianExactIf(spacing_ms, across_compared) OVER whole AS median_spacing,
    across_compared AND spacing_ms >= {gap_factor:Float64} * median_spacing AS gap,
    rate_basis >= {min_rises:UInt32} AND inside > 0
      AND inside * 1000 / greatest(last_ms - first_ms, median_spacing, 1) > {factor:Float64} * median_rate
      AS inside_suspicious,
    rate_basis >= {min_rises:UInt32} AND across > 0
      AND across * 1000 / greatest(spacing_ms, median_spacing, 1) > {factor:Float64} * median_rate
      AS across_suspicious
  FROM (
    SELECT *,
      lagInFrame(l, 1, nan) OVER previous AS prev_l,
      lagInFrame(last_ms, 1, toInt64(0)) OVER previous AS prev_last_ms,
      usable AND readings > 1 AS inside_compared,
      inside_compared AND (l < high OR low < f) AS inside_drop,
      inside_compared AND NOT inside_drop AND l > f AS inside_rise,
      if(inside_compared, if(inside_drop, high - f + l, l - f), 0) AS inside,
      usable AND isFinite(prev_l) AS across_compared,
      across_compared AND f < prev_l AS across_drop,
      across_compared AND f > prev_l AS across_rise,
      if(across_compared, if(across_drop, f, f - prev_l), 0) AS across,
      first_ms - prev_last_ms AS spacing_ms
    FROM (
      SELECT device_id, point, bucket, readings,
        toUnixTimestamp64Milli(first.ts) AS first_ms,
        toUnixTimestamp64Milli(last.ts) AS last_ms,
        first.value_type IN ('bool', 'string') OR last.value_type IN ('bool', 'string') AS non_numeric,
        multiIf(first.value_type = 'int', toFloat64(first.value_int),
          first.value_type = 'float', first.value_float, nan) AS f,
        multiIf(last.value_type = 'int', toFloat64(last.value_int),
          last.value_type = 'float', last.value_float, nan) AS l,
        ifNull(value_min, nan) AS low,
        ifNull(value_max, nan) AS high,
        isFinite(f) AND isFinite(l) AND isFinite(low) AND isFinite(high) AS usable
      FROM trustiot.",
            $view,
            "
      WHERE tenant_id = {tenant:UUID} AND (device_id, point) IN {series:Array(Tuple(UUID, String))}
        AND bucket >= toDateTime({window_from:Int64}, 'UTC')
        AND bucket < toDateTime({window_to:Int64}, 'UTC')
    )
    WINDOW previous AS (PARTITION BY device_id, point ORDER BY bucket
      ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)
  )
  WINDOW whole AS (PARTITION BY device_id, point)
)
WHERE bucket >= toDateTime({from:Int64}, 'UTC') AND bucket < toDateTime({to:Int64}, 'UTC')
GROUP BY device_id, point, local
ORDER BY device_id, point, local
LIMIT {limit:UInt32}
FORMAT JSONCompactEachRow"
        )
    };
}

pub const STATISTICS_1M_MINUTES: &str = statistics!("tenant_telemetry_1m", minutes);
pub const STATISTICS_1M_HOUR: &str = statistics!("tenant_telemetry_1m", hour);
pub const STATISTICS_1M_DAY: &str = statistics!("tenant_telemetry_1m", day);
pub const STATISTICS_1M_WEEK: &str = statistics!("tenant_telemetry_1m", week);
pub const STATISTICS_1M_MONTH: &str = statistics!("tenant_telemetry_1m", month);
pub const STATISTICS_1M_TOTAL: &str = statistics!("tenant_telemetry_1m", total);
pub const STATISTICS_1H_HOUR: &str = statistics!("tenant_telemetry_1h", hour);
pub const STATISTICS_1H_DAY: &str = statistics!("tenant_telemetry_1h", day);
pub const STATISTICS_1H_WEEK: &str = statistics!("tenant_telemetry_1h", week);
pub const STATISTICS_1H_MONTH: &str = statistics!("tenant_telemetry_1h", month);
pub const STATISTICS_1H_TOTAL: &str = statistics!("tenant_telemetry_1h", total);

pub const USAGE_1M_MINUTES: &str = usage!("tenant_telemetry_1m", minutes);
pub const USAGE_1M_HOUR: &str = usage!("tenant_telemetry_1m", hour);
pub const USAGE_1M_DAY: &str = usage!("tenant_telemetry_1m", day);
pub const USAGE_1M_WEEK: &str = usage!("tenant_telemetry_1m", week);
pub const USAGE_1M_MONTH: &str = usage!("tenant_telemetry_1m", month);
pub const USAGE_1M_TOTAL: &str = usage!("tenant_telemetry_1m", total);
pub const USAGE_1H_HOUR: &str = usage!("tenant_telemetry_1h", hour);
pub const USAGE_1H_DAY: &str = usage!("tenant_telemetry_1h", day);
pub const USAGE_1H_WEEK: &str = usage!("tenant_telemetry_1h", week);
pub const USAGE_1H_MONTH: &str = usage!("tenant_telemetry_1h", month);
pub const USAGE_1H_TOTAL: &str = usage!("tenant_telemetry_1h", total);

/// Every statement, for the tests.
pub const ALL: [&str; 22] = [
    STATISTICS_1M_MINUTES,
    STATISTICS_1M_HOUR,
    STATISTICS_1M_DAY,
    STATISTICS_1M_WEEK,
    STATISTICS_1M_MONTH,
    STATISTICS_1M_TOTAL,
    STATISTICS_1H_HOUR,
    STATISTICS_1H_DAY,
    STATISTICS_1H_WEEK,
    STATISTICS_1H_MONTH,
    STATISTICS_1H_TOTAL,
    USAGE_1M_MINUTES,
    USAGE_1M_HOUR,
    USAGE_1M_DAY,
    USAGE_1M_WEEK,
    USAGE_1M_MONTH,
    USAGE_1M_TOTAL,
    USAGE_1H_HOUR,
    USAGE_1H_DAY,
    USAGE_1H_WEEK,
    USAGE_1H_MONTH,
    USAGE_1H_TOTAL,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_read_a_tenant_view_with_parameters_only() {
        for statement in ALL {
            assert!(
                statement.contains("tenant_id = {tenant:UUID}"),
                "{statement}"
            );
            assert!(
                statement.contains("(device_id, point) IN {series:Array(Tuple(UUID, String))}"),
                "{statement}"
            );
            assert!(
                statement.contains("FROM trustiot.tenant_telemetry_1m\n")
                    || statement.contains("FROM trustiot.tenant_telemetry_1h\n"),
                "{statement}"
            );
            assert_eq!(
                statement.matches("FROM trustiot.").count(),
                1,
                "{statement}"
            );
            assert!(!statement.contains("SETTINGS"), "{statement}");
            assert!(!statement.contains("PREWHERE"), "{statement}");
            assert!(!statement.contains(';'), "{statement}");
            assert!(
                statement.ends_with("FORMAT JSONCompactEachRow"),
                "{statement}"
            );
        }
    }

    #[test]
    fn the_hour_table_and_the_minute_table_differ_only_in_the_view() {
        for (minute, hour) in [
            (STATISTICS_1M_HOUR, STATISTICS_1H_HOUR),
            (STATISTICS_1M_DAY, STATISTICS_1H_DAY),
            (STATISTICS_1M_WEEK, STATISTICS_1H_WEEK),
            (STATISTICS_1M_MONTH, STATISTICS_1H_MONTH),
            (STATISTICS_1M_TOTAL, STATISTICS_1H_TOTAL),
            (USAGE_1M_HOUR, USAGE_1H_HOUR),
            (USAGE_1M_DAY, USAGE_1H_DAY),
            (USAGE_1M_WEEK, USAGE_1H_WEEK),
            (USAGE_1M_MONTH, USAGE_1H_MONTH),
            (USAGE_1M_TOTAL, USAGE_1H_TOTAL),
        ] {
            assert_eq!(minute.replace("_1m\n", "_1h\n"), hour);
        }
    }
}
