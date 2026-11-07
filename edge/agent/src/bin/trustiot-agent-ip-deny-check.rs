//! `trustiot-agent-ip-deny-check`: confirms that systemd attached the agent unit's
//! `IPAddressDeny=` to the unit's cgroup (Laysi/trustiot-v2#866). Who runs it and what happens
//! when it fails: `README.md`, section "systemd", 「`IPAddressDeny=` 沒有掛上時」.
//!
//! systemd turns `IPAddressDeny=` into BPF programs attached to the unit's cgroup, one for
//! ingress and one for egress. When it cannot (no cgroup BPF in the kernel, no cgroup v2, no
//! permission to load BPF programs), it starts the unit all the same and says so only at debug
//! level, so that layer would be missing without anyone noticing. The agent's unit runs this
//! executable as `ExecStartPre=-+` on every start: in the unit's own cgroup, as root (the query
//! needs `CAP_NET_ADMIN`), and with `-`, so a failure is logged at error level but does not stop
//! the agent, whose link to the cloud stays up (ssh does not depend on the agent either way). `+`
//! also lifts the unit's `SystemCallFilter=` for this command (systemd applies none of its
//! sandboxing, seccomp included, to a `+` command), so `bpf(2)`, which is in `@privileged`, is
//! allowed here and nowhere else in the unit; `tests/egress.rs` runs the shipped unit and sees the
//! query answer.
//!
//! It reads its own cgroup from `/proc/self/cgroup` (the unified hierarchy's `0::` line), refuses
//! to check any cgroup but `trustiot-agent.service`'s, opens that cgroup's directory under
//! `/sys/fs/cgroup` and asks the kernel how many programs are attached there for
//! `BPF_CGROUP_INET_INGRESS` and `BPF_CGROUP_INET_EGRESS` (`BPF_PROG_QUERY`, one `bpf(2)` call
//! each). It exits 0 when both have one, 1 when either has none or the query fails, and 2 when
//! it is run anywhere but in the agent's cgroup or with an argument. Every failure but the
//! argument check is logged at error level (`<3>`) and names `IPAddressDeny=`. It takes no
//! arguments, reads no environment variable, writes no file and starts no program.

#![allow(
    clippy::print_stderr,
    reason = "the log goes to standard error, which systemd sends to the journal"
)]

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::process::ExitCode;

/// The kernel's view of this process's cgroups.
const PROC_CGROUP: &str = "/proc/self/cgroup";
/// Where the unified (v2) hierarchy is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// The only cgroup this checks: the agent's unit, which runs it.
const UNIT: &str = "trustiot-agent.service";

/// `BPF_PROG_QUERY` of `enum bpf_cmd` (`include/uapi/linux/bpf.h`).
const BPF_PROG_QUERY: libc::c_long = 16;
/// `BPF_CGROUP_INET_INGRESS` and `BPF_CGROUP_INET_EGRESS` of `enum bpf_attach_type`.
const DIRECTIONS: [(&str, u32); 2] = [("ingress", 0), ("egress", 1)];

/// The `query` member of `union bpf_attr` (`include/uapi/linux/bpf.h`), up to `revision`. The
/// kernel copies the bytes it was given and treats the rest of its larger union as zero.
#[repr(C, align(8))]
#[derive(Default)]
struct ProgQuery {
    target_fd: u32,
    attach_type: u32,
    query_flags: u32,
    attach_flags: u32,
    prog_ids: u64,
    prog_cnt: u32,
    padding: u32,
    prog_attach_flags: u64,
    link_ids: u64,
    link_attach_flags: u64,
    revision: u64,
}

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        eprintln!(
            "trustiot-agent-ip-deny-check takes no arguments: it checks the IP firewall of the \
             cgroup it runs in, which must be {UNIT}'s"
        );
        return ExitCode::from(2);
    }
    let cgroup = match std::fs::read_to_string(PROC_CGROUP) {
        Ok(contents) => match unified(&contents) {
            Some(path) => path.to_owned(),
            None => {
                eprintln!(
                    "<3>{PROC_CGROUP} has no cgroup v2 entry: systemd attaches IPAddressDeny= \
                     only on the unified hierarchy, so the agent's unit runs without it \
                     (loopback, metadata, multicast and broadcast are then refused only by the \
                     egress rules)"
                );
                return ExitCode::FAILURE;
            }
        },
        Err(error) => {
            eprintln!("<3>reading {PROC_CGROUP} failed: {error}; IPAddressDeny= is not confirmed");
            return ExitCode::FAILURE;
        }
    };
    if !is_agents(&cgroup) {
        // Logged as an error that names IPAddressDeny=, like every other failure: the agent's
        // unit runs this with `-`, so a run outside its cgroup (a later Delegate=, say) would
        // otherwise let the agent start without the confirmation and without a trace.
        eprintln!(
            "<3>this process is in {cgroup}, not in {UNIT}'s cgroup: IPAddressDeny= is not \
             confirmed (trustiot-agent-ip-deny-check checks only that cgroup and runs from that \
             unit's ExecStartPre=)"
        );
        return ExitCode::from(2);
    }
    match attached(&cgroup) {
        Ok(counts) if counts.iter().all(|&(_, count)| count > 0) => {
            eprintln!(
                "IPAddressDeny= is attached to {cgroup}: {}",
                describe(&counts)
            );
            ExitCode::SUCCESS
        }
        Ok(counts) => {
            eprintln!(
                "<3>IPAddressDeny= is not attached to {cgroup} ({}): the agent runs without it, \
                 so while the egress rules are flushed it can reach loopback, the metadata \
                 addresses, multicast and the limited broadcast address until the egress check \
                 loads the rules again (within 11 s)",
                describe(&counts)
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!(
                "<3>querying the IP firewall of {cgroup} failed: {error}; IPAddressDeny= is not \
                 confirmed"
            );
            ExitCode::FAILURE
        }
    }
}

/// The path of the unified hierarchy's entry (`0::<path>`) in `/proc/self/cgroup`.
fn unified(contents: &str) -> Option<&str> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| path.starts_with('/'))
}

/// Whether `path` is the agent unit's own cgroup (systemd puts every process of a unit without
/// `Delegate=` there, `ExecStartPre=` included), and nothing that could leave `/sys/fs/cgroup`.
fn is_agents(path: &str) -> bool {
    path.rsplit('/').next() == Some(UNIT)
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// How many programs are attached to the cgroup for each direction.
fn attached(cgroup: &str) -> io::Result<[(&'static str, u32); 2]> {
    let directory = File::open(format!("{CGROUP_ROOT}{cgroup}"))?;
    let target_fd = u32::try_from(directory.as_raw_fd())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut counts = [("", 0); 2];
    for (slot, &(direction, attach_type)) in counts.iter_mut().zip(&DIRECTIONS) {
        *slot = (direction, query(target_fd, attach_type)?);
    }
    Ok(counts)
}

/// How many program IDs one query asks for. A cgroup has one program per direction for
/// `IPAddressDeny=` and a few more for other settings; more than this is still counted (below).
const ID_CAPACITY: u32 = 64;

/// `bpf(BPF_PROG_QUERY)` for one attach type of the cgroup open at `target_fd`: the number of
/// programs attached to the cgroup itself (not the ones it inherits).
///
/// The kernel returns only the count when it is given no ID buffer, but the query also gets a
/// buffer (and its capacity in `prog_cnt`), so that it does not depend on that shortcut. When more
/// programs are attached than the buffer holds, the kernel fills it, sets `prog_cnt` to the total
/// and fails with `ENOSPC`: that is still an answer, and the total is the count.
#[allow(unsafe_code, reason = "bpf(2) has no wrapper in std or libc")]
fn query(target_fd: u32, attach_type: u32) -> io::Result<u32> {
    let mut ids = [0_u32; ID_CAPACITY as usize];
    let mut attr = ProgQuery {
        target_fd,
        attach_type,
        prog_ids: ids.as_mut_ptr() as u64,
        prog_cnt: ID_CAPACITY,
        ..ProgQuery::default()
    };
    // SAFETY: `attr` is a properly aligned, initialized `ProgQuery` and `ids` an array of
    // `ID_CAPACITY` u32s; both live across the call, the size passed is `attr`'s own, and the
    // kernel writes at most `prog_cnt` (the array's length) IDs into `ids` and only the query
    // fields of `attr`.
    let result = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_QUERY,
            &raw mut attr,
            size_of::<ProgQuery>(),
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOSPC) && attr.prog_cnt > ID_CAPACITY {
            return Ok(attr.prog_cnt);
        }
        return Err(error);
    }
    Ok(attr.prog_cnt)
}

fn describe(counts: &[(&str, u32)]) -> String {
    counts
        .iter()
        .map(|(direction, count)| format!("{count} {direction}"))
        .collect::<Vec<_>>()
        .join(", ")
        + " programs"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_unified_entry() {
        assert_eq!(
            unified("0::/system.slice/trustiot-agent.service\n"),
            Some("/system.slice/trustiot-agent.service")
        );
        assert_eq!(
            unified("12:pids:/x\n1:name=systemd:/y\n0::/system.slice/trustiot-agent.service\n"),
            Some("/system.slice/trustiot-agent.service")
        );
        assert_eq!(unified("12:pids:/x\n1:name=systemd:/y\n"), None);
        assert_eq!(unified("0::relative\n"), None);
        assert_eq!(unified(""), None);
    }

    #[test]
    fn checks_only_the_agents_cgroup() {
        assert!(is_agents("/system.slice/trustiot-agent.service"));
        assert!(is_agents("/trustiot-agent.service"));
        assert!(!is_agents("/system.slice/ssh.service"));
        assert!(!is_agents("/system.slice/trustiot-agent.service/sub"));
        assert!(!is_agents("/user.slice/x/../trustiot-agent.service"));
        assert!(!is_agents("/system.slice//trustiot-agent.service"));
        assert!(!is_agents("/"));
    }

    #[test]
    fn the_query_has_the_kernels_layout() {
        assert_eq!(size_of::<ProgQuery>(), 64);
        assert_eq!(std::mem::offset_of!(ProgQuery, prog_ids), 16);
        assert_eq!(std::mem::offset_of!(ProgQuery, prog_cnt), 24);
        assert_eq!(std::mem::offset_of!(ProgQuery, prog_attach_flags), 32);
        assert_eq!(std::mem::offset_of!(ProgQuery, revision), 56);
    }

    #[test]
    fn describes_both_directions() {
        assert_eq!(
            describe(&[("ingress", 1), ("egress", 0)]),
            "1 ingress, 0 egress programs"
        );
    }
}
