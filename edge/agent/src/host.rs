//! The gateway's own operating metrics for the heartbeat (rebuild checklist H1,
//! Laysi/trustiot-v2#117): CPU use, the data partition's size and free space, the SoC
//! temperature, the power and throttling flags, and how often the OS booted and the agent
//! started.
//!
//! Everything is read from files the agent's sandbox leaves readable (`/proc`, `/sys`, and
//! `statvfs` on the state directory), never by running a program: the unit allows no
//! `vcgencmd`. A source that is missing or unreadable (a gateway without the sensor, a kernel
//! without the firmware attribute, an unmounted partition) is reported as what the protocol
//! allows for it, never as a made-up number: `None` for the temperature and the throttling
//! flags, zero for sizes and counts. The text sources are parsed by pure functions, so the
//! parsing is tested without the files.
//!
//! - **CPU** ([`CpuMeter`]): the busy share of all CPUs between two heartbeats, from the
//!   aggregate line of `/proc/stat` (idle and iowait are not busy; guest time is already in user
//!   time). The first heartbeat of a run measures from the OS boot.
//! - **Temperature**: `thermal_zone0`, which is the SoC sensor on the gateway's board.
//! - **Throttling** ([`ThrottleTracker`]): the firmware's `get_throttled` word, which is
//!   what `vcgencmd get_throttled` prints: bits 0 to 3 are the flags now, bits 16 to 19 the ones
//!   set at any time since boot. The agent also keeps what it has seen itself since it started,
//!   so a flag that was set between two readings and cleared again in the firmware's word is
//!   not lost for the rest of the agent's run.
//! - **Boot count** ([`register_start`]): the kernel's boot ID tells one boot from the next; a
//!   small file on the data partition counts the boots it saw and the agent starts in the
//!   current one, because the OS cannot count its own boots across the agent's life.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use protocol::health::{ThrottleFlags, Throttling};

/// The kernel's boot ID: a UUID that is new at every boot.
pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
/// The file in the state directory that counts boots and agent starts.
pub const BOOT_RECORD_FILE: &str = "boot-count";
/// The temporary file a save writes before renaming it to [`BOOT_RECORD_FILE`].
const BOOT_RECORD_TEMPORARY: &str = "boot-count.tmp";
/// The most the boot record file is read as; it is a few dozen bytes.
const BOOT_RECORD_MAX_BYTES: u64 = 4096;
/// The SoC temperature sensor: millidegrees Celsius.
const THERMAL_ZONE_PATH: &str = "/sys/class/thermal/thermal_zone0/temp";
/// The firmware's throttling word, as the Raspberry Pi kernel exposes it.
const THROTTLED_PATH: &str = "/sys/devices/platform/soc/soc:firmware/get_throttled";
/// Temperatures outside this range, in degrees Celsius, are a broken sensor, not a measurement.
const TEMPERATURE_RANGE: std::ops::RangeInclusive<f64> = -100.0..=250.0;

/// Where the metrics are read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPaths {
    /// A directory laid out like `/proc` (`meminfo`, `uptime`, `stat`).
    pub proc_directory: PathBuf,
    /// The SoC temperature file.
    pub temperature: PathBuf,
    /// The firmware's throttling word.
    pub throttled: PathBuf,
    /// A path on the data partition, whose size and free space are reported.
    pub data: PathBuf,
}

impl HostPaths {
    /// The paths of a gateway, with the data partition that holds `data` (the state directory).
    pub fn new(data: impl Into<PathBuf>) -> Self {
        Self {
            proc_directory: PathBuf::from("/proc"),
            temperature: PathBuf::from(THERMAL_ZONE_PATH),
            throttled: PathBuf::from(THROTTLED_PATH),
            data: data.into(),
        }
    }

    /// Paths that exist nowhere, so that every metric is missing; for tests.
    pub fn nowhere() -> Self {
        let root = Path::new("/nonexistent/trustiot-agent-test");
        Self {
            proc_directory: root.join("proc"),
            temperature: root.join("temperature"),
            throttled: root.join("throttled"),
            data: root.join("data"),
        }
    }
}

/// Size and free space of a file system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSpace {
    /// The size, in bytes.
    pub total_bytes: u64,
    /// The space available to the agent's account, in bytes.
    pub available_bytes: u64,
}

/// What one reading of the host found; every source that could not be read is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostSample {
    /// `/proc/meminfo`.
    pub meminfo: String,
    /// `/proc/uptime`.
    pub uptime: String,
    /// `/proc/stat`.
    pub stat: String,
    /// The temperature file's contents.
    pub temperature: Option<String>,
    /// The throttling word's contents.
    pub throttled: Option<String>,
    /// The data partition.
    pub data: Option<DiskSpace>,
}

impl HostSample {
    /// Reads every source. Blocking: call it from a thread that may wait on the file system.
    pub fn read(paths: &HostPaths) -> Self {
        let text = |path: &Path| fs::read_to_string(path).ok();
        Self {
            meminfo: text(&paths.proc_directory.join("meminfo")).unwrap_or_default(),
            uptime: text(&paths.proc_directory.join("uptime")).unwrap_or_default(),
            stat: text(&paths.proc_directory.join("stat")).unwrap_or_default(),
            temperature: text(&paths.temperature),
            throttled: text(&paths.throttled),
            data: disk_space(&paths.data),
        }
    }
}

/// The size and free space of the file system that holds `path`; `None` when it cannot be asked.
#[allow(
    unsafe_code,
    reason = "statvfs(3) has no safe wrapper in std; the path is a CStr and the result is plain data"
)]
#[allow(
    clippy::useless_conversion,
    reason = "the block counts are u32 on 32-bit targets and u64 on the 64-bit ones"
)]
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is plain C data (integers); all zeroes is a valid value, and the call
    // overwrites it when it succeeds, which is the only case in which it is read.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a NUL-terminated string that outlives the call and `stat` is a valid,
    // writable `statvfs`.
    if unsafe { libc::statvfs(path.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    let unit = u64::from(stat.f_frsize);
    Some(DiskSpace {
        total_bytes: u64::from(stat.f_blocks).saturating_mul(unit),
        available_bytes: u64::from(stat.f_bavail).saturating_mul(unit),
    })
}

/// The busy share of the CPUs between two readings.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuMeter {
    previous: Option<CpuTimes>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

impl CpuMeter {
    /// The average CPU use, 0 to 100, since the previous call (since boot for the first), from
    /// the contents of `/proc/stat`; 0 when it cannot be read or no time has passed.
    pub fn percent(&mut self, stat: &str) -> f64 {
        let Some(now) = cpu_times(stat) else {
            return 0.0;
        };
        let before = self.previous.replace(now).unwrap_or_default();
        let total = now.total.saturating_sub(before.total);
        let busy = now.busy.saturating_sub(before.busy);
        if total == 0 {
            return 0.0;
        }
        // Counters reach 2^53 only after centuries of CPU time; the loss of precision is
        // irrelevant for a percentage.
        let share = busy as f64 / total as f64 * 100.0;
        share.clamp(0.0, 100.0)
    }
}

/// The aggregate `cpu` line of `/proc/stat`: user nice system idle iowait irq softirq steal
/// (guest and guest_nice are already part of user and nice).
fn cpu_times(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    // Old kernels have fewer than eight; user, nice, system and idle are the least.
    let [user, nice, system, idle, rest @ ..] = fields.as_slice() else {
        return None;
    };
    let iowait = rest.first().copied().unwrap_or(0);
    let others = rest
        .iter()
        .skip(1)
        .fold(0_u64, |sum, time| sum.saturating_add(*time));
    let busy = user
        .saturating_add(*nice)
        .saturating_add(*system)
        .saturating_add(others);
    Some(CpuTimes {
        busy,
        total: busy.saturating_add(*idle).saturating_add(iowait),
    })
}

/// The SoC temperature in degrees Celsius from the contents of the thermal zone's `temp` file
/// (millidegrees); `None` when it is not a number or outside [`TEMPERATURE_RANGE`].
pub fn temperature_celsius(text: &str) -> Option<f64> {
    let millidegrees: i32 = text.trim().parse().ok()?;
    let celsius = f64::from(millidegrees) / 1000.0;
    TEMPERATURE_RANGE.contains(&celsius).then_some(celsius)
}

/// The firmware's throttling word from its file: hexadecimal, with or without `0x`.
fn throttled_word(text: &str) -> Option<u32> {
    let text = text.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u32::from_str_radix(digits, 16).ok()
}

/// The four flags of `word` starting at bit `shift` (`vcgencmd get_throttled`: under-voltage,
/// ARM frequency capped, throttled, soft temperature limit).
fn flags(word: u32, shift: u32) -> ThrottleFlags {
    let set = |bit: u32| word & (1 << (shift + bit)) != 0;
    ThrottleFlags {
        under_voltage: set(0),
        frequency_capped: set(1),
        throttled: set(2),
        soft_temp_limit: set(3),
    }
}

fn union(a: ThrottleFlags, b: ThrottleFlags) -> ThrottleFlags {
    ThrottleFlags {
        under_voltage: a.under_voltage || b.under_voltage,
        frequency_capped: a.frequency_capped || b.frequency_capped,
        throttled: a.throttled || b.throttled,
        soft_temp_limit: a.soft_temp_limit || b.soft_temp_limit,
    }
}

/// The power and throttling flags, with the ones seen since the agent started kept in
/// `since_boot` (see the module documentation).
#[derive(Clone, Copy, Debug, Default)]
pub struct ThrottleTracker {
    seen: ThrottleFlags,
}

impl ThrottleTracker {
    /// The flags for the contents of the throttling file; `None` when the file is missing or is
    /// not a hexadecimal number, as on hardware that does not report them.
    pub fn observe(&mut self, text: Option<&str>) -> Option<Throttling> {
        let word = throttled_word(text?)?;
        let now = flags(word, 0);
        self.seen = union(self.seen, union(now, flags(word, 16)));
        Some(Throttling {
            now,
            since_boot: self.seen,
        })
    }
}

/// How many times the OS booted and how many times the agent started in the current boot, for
/// the heartbeat's `boot_count` and `agent_starts_since_boot`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootCounts {
    /// Boots counted on the data partition; this one included.
    pub boot_count: u64,
    /// Agent starts in the current boot; this one included, so 1 means it has not restarted.
    pub agent_starts_since_boot: u32,
}

/// What the boot record file holds.
#[derive(Debug, PartialEq, Eq)]
struct BootRecord {
    boot_id: String,
    boot_count: u64,
    agent_starts: u32,
}

impl BootRecord {
    fn parse(text: &str) -> Option<Self> {
        let (mut boot_id, mut boot_count, mut agent_starts) = (None, None, None);
        for line in text.lines() {
            match line.split_once('=')? {
                ("boot_id", value) => boot_id = Some(value.to_owned()),
                ("boot_count", value) => boot_count = value.parse().ok(),
                ("agent_starts", value) => agent_starts = value.parse().ok(),
                _ => {}
            }
        }
        Some(Self {
            boot_id: boot_id.filter(|id| !id.is_empty())?,
            boot_count: boot_count?,
            agent_starts: agent_starts?,
        })
    }

    fn render(&self) -> String {
        format!(
            "boot_id={}\nboot_count={}\nagent_starts={}\n",
            self.boot_id, self.boot_count, self.agent_starts
        )
    }
}

/// Counts this start of the agent in the record in `directory` (the state directory) for the
/// boot `boot_id`, and returns the counts including it. A new boot ID is a new boot, whose
/// first agent start this is; the same boot ID is another start in the same boot. The record is
/// replaced atomically; when it cannot be read it starts over, and when it cannot be written
/// the counts are still returned (they are right for this run, and the next start counts
/// again from what is on the disk). With no boot ID nothing can be told, and the counts are
/// zero.
pub fn register_start(directory: &Path, boot_id: Option<&str>) -> BootCounts {
    let Some(boot_id) = boot_id.map(str::trim).filter(|id| !id.is_empty()) else {
        tracing::warn!(
            path = BOOT_ID_PATH,
            "the boot ID is unreadable; reporting no boot count"
        );
        return BootCounts::default();
    };
    let path = directory.join(BOOT_RECORD_FILE);
    let previous = match read_record(&path) {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "the boot record is unusable; counting from this boot");
            None
        }
    };
    let record = match previous {
        Some(previous) if previous.boot_id == boot_id => BootRecord {
            agent_starts: previous.agent_starts.saturating_add(1),
            ..previous
        },
        Some(previous) => BootRecord {
            boot_id: boot_id.to_owned(),
            boot_count: previous.boot_count.saturating_add(1),
            agent_starts: 1,
        },
        None => BootRecord {
            boot_id: boot_id.to_owned(),
            boot_count: 1,
            agent_starts: 1,
        },
    };
    if let Err(error) = write_record(directory, &record) {
        tracing::warn!(path = %path.display(), %error, "cannot save the boot record; the next start counts from what is on the disk");
    }
    BootCounts {
        boot_count: record.boot_count,
        agent_starts_since_boot: record.agent_starts,
    }
}

/// The current boot's ID; `None` when the kernel does not give it.
pub fn current_boot_id() -> Option<String> {
    fs::read_to_string(BOOT_ID_PATH).ok()
}

fn read_record(path: &Path) -> io::Result<Option<BootRecord>> {
    use std::io::Read as _;

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut text = String::new();
    file.take(BOOT_RECORD_MAX_BYTES).read_to_string(&mut text)?;
    BootRecord::parse(&text)
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a boot record"))
}

/// Replaces the record the way the applied configuration is replaced
/// ([`crate::desired::store::Store::save`]): written and flushed under another name, renamed
/// over the old file, and the directory flushed.
fn write_record(directory: &Path, record: &BootRecord) -> io::Result<()> {
    let temporary = directory.join(BOOT_RECORD_TEMPORARY);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(record.render().as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, directory.join(BOOT_RECORD_FILE))?;
    File::open(directory)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 20 30 800 40 5 5 0 7 0\ncpu0 50 10 15 400 20 2 3 0 0 0\nintr 1\n";

    #[test]
    fn cpu_use_is_the_busy_share_between_two_readings() {
        let mut meter = CpuMeter::default();
        // Since boot: busy = 100 + 20 + 30 + 5 + 5 + 0 = 160 of 160 + 800 + 40 = 1000 (guest
        // time, the ninth field, is already in user time).
        assert!((meter.percent(STAT) - 16.0).abs() < 1e-9);
        // 60 busy and 40 idle ticks later: 60 %.
        let later = "cpu  150 20 40 840 40 5 5 0 9 0\n";
        assert!((meter.percent(later) - 60.0).abs() < 1e-9);
        // Nothing elapsed: no division by zero.
        assert_eq!(meter.percent(later), 0.0);
    }

    #[test]
    fn cpu_use_survives_unreadable_and_odd_input() {
        let mut meter = CpuMeter::default();
        assert_eq!(meter.percent(""), 0.0);
        assert_eq!(meter.percent("cpu  a b c d\n"), 0.0);
        assert_eq!(meter.percent("cpu  1 2 3\n"), 0.0, "fewer than four fields");
        // An old kernel without iowait and the rest: 3 busy of 4.
        assert!((meter.percent("cpu  1 1 1 1\n") - 75.0).abs() < 1e-9);
        // A counter that went backwards (a reset) never gives a negative or a value above 100.
        let percent = meter.percent("cpu  0 0 0 1000\n");
        assert!((0.0..=100.0).contains(&percent), "{percent}");
        let percent = meter.percent("cpu  18446744073709551615 18446744073709551615 1 1\n");
        assert!((0.0..=100.0).contains(&percent), "{percent}");
    }

    #[test]
    fn the_temperature_is_in_degrees_and_sensor_garbage_is_left_out() {
        assert_eq!(temperature_celsius("48312\n"), Some(48.312));
        assert_eq!(temperature_celsius("-5000"), Some(-5.0));
        for garbage in ["", "hot", "999999999", "-999999999", "99999999999"] {
            assert_eq!(temperature_celsius(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn the_throttling_word_gives_the_flags_now_and_since_boot() {
        let mut tracker = ThrottleTracker::default();
        // 0x50005: under-voltage and throttled now and since boot.
        let flags = tracker.observe(Some("50005\n")).unwrap();
        let both = ThrottleFlags {
            under_voltage: true,
            throttled: true,
            ..ThrottleFlags::default()
        };
        assert_eq!((flags.now, flags.since_boot), (both, both));
        // 0x80008 with a prefix: soft temperature limit now and since boot; what was seen
        // earlier stays in since_boot even though the firmware's word no longer has it.
        let flags = tracker.observe(Some("0x80008")).unwrap();
        assert_eq!(
            flags.now,
            ThrottleFlags {
                soft_temp_limit: true,
                ..ThrottleFlags::default()
            }
        );
        assert_eq!(
            flags.since_boot,
            ThrottleFlags {
                soft_temp_limit: true,
                ..both
            }
        );
        // A flag only in the sticky half is in since_boot and not in now: 0x20000, frequency
        // capped some time ago.
        let mut tracker = ThrottleTracker::default();
        let flags = tracker.observe(Some("20000")).unwrap();
        assert_eq!(flags.now, ThrottleFlags::default());
        assert!(flags.since_boot.frequency_capped);
        assert!(!flags.since_boot.under_voltage);
    }

    #[test]
    fn missing_or_malformed_throttling_is_not_reported() {
        let mut tracker = ThrottleTracker::default();
        assert_eq!(tracker.observe(None), None);
        for garbage in ["", "zz", "0x", "123456789abcdef"] {
            assert_eq!(tracker.observe(Some(garbage)), None, "{garbage:?}");
        }
        // A reading of 0 means the hardware reports and nothing is wrong.
        assert_eq!(
            tracker.observe(Some("0\n")),
            Some(Throttling {
                now: ThrottleFlags::default(),
                since_boot: ThrottleFlags::default(),
            })
        );
    }

    #[test]
    fn the_data_partition_is_measured_and_a_missing_path_is_not() {
        let space = disk_space(&std::env::temp_dir()).unwrap();
        assert!(space.total_bytes > 0);
        assert!(space.available_bytes <= space.total_bytes);
        assert_eq!(
            disk_space(Path::new("/nonexistent/trustiot-agent-test")),
            None
        );
        assert_eq!(disk_space(Path::new("/with\0nul")), None);
    }

    #[test]
    fn a_sample_of_unreadable_sources_is_empty() {
        assert_eq!(
            HostSample::read(&HostPaths::nowhere()),
            HostSample::default()
        );
    }

    #[test]
    fn a_sample_reads_every_source_it_is_pointed_at() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-host");
        let proc = scratch.path().join("proc");
        fs::create_dir(&proc).unwrap();
        fs::write(proc.join("meminfo"), "MemTotal: 1 kB\n").unwrap();
        fs::write(proc.join("uptime"), "5.0 1.0\n").unwrap();
        fs::write(proc.join("stat"), STAT).unwrap();
        fs::write(scratch.path().join("temp"), "50000\n").unwrap();
        fs::write(scratch.path().join("throttled"), "0\n").unwrap();
        let sample = HostSample::read(&HostPaths {
            proc_directory: proc,
            temperature: scratch.path().join("temp"),
            throttled: scratch.path().join("throttled"),
            data: scratch.path().to_owned(),
        });
        assert_eq!(sample.meminfo, "MemTotal: 1 kB\n");
        assert_eq!(sample.uptime, "5.0 1.0\n");
        assert_eq!(sample.stat, STAT);
        assert_eq!(sample.temperature.as_deref(), Some("50000\n"));
        assert_eq!(sample.throttled.as_deref(), Some("0\n"));
        assert!(sample.data.is_some());
    }

    #[test]
    fn boots_and_agent_starts_are_counted_on_the_data_partition() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-boot");
        let directory = scratch.path();
        let counts = |boot: &str| register_start(directory, Some(boot));
        assert_eq!(
            counts("boot-a"),
            BootCounts {
                boot_count: 1,
                agent_starts_since_boot: 1
            }
        );
        // The agent restarts in the same boot.
        assert_eq!(
            counts("boot-a"),
            BootCounts {
                boot_count: 1,
                agent_starts_since_boot: 2
            }
        );
        assert_eq!(counts("boot-a").agent_starts_since_boot, 3);
        // The OS reboots.
        assert_eq!(
            counts("boot-b"),
            BootCounts {
                boot_count: 2,
                agent_starts_since_boot: 1
            }
        );
        assert_eq!(counts("boot-b").boot_count, 2);
        // The file is the agent's alone and has no leftovers.
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(directory.join(BOOT_RECORD_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode, 0o600);
        assert!(!directory.join(BOOT_RECORD_TEMPORARY).exists());
    }

    #[test]
    fn a_damaged_or_unwritable_record_does_not_stop_the_count() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-boot-damaged");
        let directory = scratch.path();
        for damaged in [
            "",
            "garbage",
            "boot_id=\nboot_count=1\nagent_starts=1\n",
            "boot_id=a\nboot_count=x\nagent_starts=1\n",
        ] {
            fs::write(directory.join(BOOT_RECORD_FILE), damaged).unwrap();
            assert_eq!(
                register_start(directory, Some("boot-a")),
                BootCounts {
                    boot_count: 1,
                    agent_starts_since_boot: 1
                },
                "{damaged:?}"
            );
        }
        // A directory that cannot be written: the counts of this run are still returned.
        assert_eq!(
            register_start(
                Path::new("/nonexistent/trustiot-agent-test"),
                Some("boot-a")
            ),
            BootCounts {
                boot_count: 1,
                agent_starts_since_boot: 1
            }
        );
        // Without a boot ID nothing can be told.
        assert_eq!(register_start(directory, None), BootCounts::default());
        assert_eq!(
            register_start(directory, Some("  \n")),
            BootCounts::default()
        );
    }

    #[test]
    fn the_boot_id_has_a_trailing_newline_that_does_not_count() {
        let scratch = testkit::mqtt::ScratchDir::new("agent-boot-newline");
        let first = register_start(scratch.path(), Some("0199d0a0-0000\n"));
        let second = register_start(scratch.path(), Some("0199d0a0-0000"));
        assert_eq!(first.boot_count, second.boot_count);
        assert_eq!(second.agent_starts_since_boot, 2);
    }
}
