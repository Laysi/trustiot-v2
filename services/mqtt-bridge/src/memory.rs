//! The connection cap against the container's memory limit (README.md, "記憶體").
//!
//! Anyone on the internet can open a WebSocket to the bridge and make it hold
//! [`relay::WORST_CASE_PER_CONNECTION`] bytes, so `max_connections` of them must fit in the memory
//! limit with room to spare, or a flood of connections gets the pod killed and every gateway with
//! it. The bridge refuses to start when they do not fit.

use std::fs;
use std::path::Path;

use crate::relay;

/// The share of the memory limit the connections may take: the rest is the runtime, the logs and
/// telemetry buffers, and the kernel's socket buffers, which the cgroup counts too.
const SHARE_NUMERATOR: u64 = 3;
const SHARE_DENOMINATOR: u64 = 4;

/// The memory the connections may take under `limit` bytes.
pub const fn budget(limit: u64) -> u64 {
    limit / SHARE_DENOMINATOR * SHARE_NUMERATOR
}

/// The worst case of `max_connections` connections, in bytes.
pub fn worst_case(max_connections: usize) -> u64 {
    let per_connection = u64::try_from(relay::WORST_CASE_PER_CONNECTION).unwrap_or(u64::MAX);
    u64::try_from(max_connections)
        .unwrap_or(u64::MAX)
        .saturating_mul(per_connection)
}

/// Checks `max_connections` against the memory limit of the cgroup this process runs in (cgroup
/// v2, as Kubernetes and podman set it). Without a limit (`max`, no cgroup v2, a file that cannot
/// be read) there is nothing to check against.
///
/// # Errors
///
/// When the connections' worst case exceeds the budget; the message names both settings.
pub fn check(max_connections: usize) -> Result<(), String> {
    let limit = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|membership| own_cgroup(&membership).map(str::to_owned))
        .and_then(|cgroup| {
            let file = Path::new("/sys/fs/cgroup")
                .join(cgroup.trim_start_matches('/'))
                .join("memory.max");
            fs::read_to_string(file).ok()
        })
        .and_then(|text| parse_limit(&text));
    check_against(max_connections, limit)
}

/// [`check`] with the limit already read.
pub fn check_against(max_connections: usize, limit: Option<u64>) -> Result<(), String> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let needed = worst_case(max_connections);
    if needed > budget(limit) {
        return Err(format!(
            "max_connections {max_connections} needs up to {} MiB ({} KiB per connection), more \
             than three quarters of the container's memory limit of {} MiB: raise the memory \
             limit or lower TRUSTIOT__MAX_CONNECTIONS (services/mqtt-bridge/README.md, 記憶體)",
            needed / (1024 * 1024),
            relay::WORST_CASE_PER_CONNECTION / 1024,
            limit / (1024 * 1024),
        ));
    }
    Ok(())
}

/// The cgroup v2 path in `/proc/self/cgroup` (the `0::<path>` line).
fn own_cgroup(membership: &str) -> Option<&str> {
    membership.lines().find_map(|line| line.strip_prefix("0::"))
}

/// A `memory.max` value: a number of bytes, or `max` for none.
fn parse_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_CONNECTIONS;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_default_cap_fits_the_charts_default_memory_limit() {
        // serviceDefaults.resources.limits.memory in deploy/chart/values.yaml.
        let chart_limit = 256 * MIB;
        assert!(worst_case(DEFAULT_MAX_CONNECTIONS) <= budget(chart_limit));
        assert_eq!(
            check_against(DEFAULT_MAX_CONNECTIONS, Some(chart_limit)),
            Ok(())
        );
    }

    #[test]
    fn a_cap_beyond_the_limit_is_refused_with_both_settings_named() {
        let error = check_against(4096, Some(256 * MIB)).expect_err("4,096 do not fit 256 MiB");
        assert!(error.contains("TRUSTIOT__MAX_CONNECTIONS"), "{error}");
        assert!(error.contains("256 MiB"), "{error}");
        assert_eq!(check_against(4096, Some(1024 * MIB)), Ok(()));
    }

    #[test]
    fn no_limit_means_nothing_to_check() {
        assert_eq!(check_against(65_536, None), Ok(()));
        assert_eq!(parse_limit("max\n"), None);
        assert_eq!(parse_limit("268435456\n"), Some(256 * MIB));
    }

    #[test]
    fn the_own_cgroup_is_the_v2_line() {
        assert_eq!(own_cgroup("0::/\n"), Some("/"));
        assert_eq!(
            own_cgroup("1:name=systemd:/x\n0::/kubepods/pod1/c1\n"),
            Some("/kubepods/pod1/c1")
        );
        assert_eq!(own_cgroup("12:memory:/x\n"), None);
    }
}
