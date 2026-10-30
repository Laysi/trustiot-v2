//! The `lab-dataset` command: checks a dataset, loads it into a ClickHouse given by the
//! environment, exports one out of it, or records one from Modbus devices. Credentials come only
//! from the environment, never from arguments (docs/architecture/secrets.md, rule 3).

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::export::{self, Exporter};
use crate::record::{Config, Recorder};
use crate::{ClickHouse, Dataset, Target, TimeShift, time};

pub const USAGE: &str = "usage:
  lab-dataset check <index.toml>
  lab-dataset load <index.toml> --tenant <uuid> --gateway <uuid> [--device <key>=<uuid>]...
                   [--shift-ms <ms> | --end-before <now|time> [--align-s <seconds>]]
  lab-dataset export <config.toml> --tenant <uuid> --gateway <id> --from <time> --to <time>
                     --out <dir>
  lab-dataset record <config.toml> --out <dir> [--duration-s <seconds>]

load and export use the ClickHouse named by the environment:
  CLICKHOUSE_URL       http://host:port or https://host:port (no credentials in it)
  CLICKHOUSE_USER      load: an account that may INSERT into trustiot.telemetry and
                       trustiot.stream_commits; export: an account with SELECT on both tables
                       and a row policy that names it (see datasets/lab/README.md)
  CLICKHOUSE_PASSWORD  its password
  CLICKHOUSE_CA_FILE   with https:// only: the CA certificate (PEM) the connection trusts

--end-before moves the dataset by whole multiples of --align-s (default 86400, one day) so that
its last time falls at or just before the given time; --shift-ms moves it by exactly that much.
Without either, the dataset's own times are written.

export reads the batches of the gateway whose rows were received from --from up to, not
including, --to (times like 2026-10-30T16:08:00.000Z, UTC) and writes the dataset to
<dir>/<name>/, which must not exist yet. Times are written as stored. The configuration lists
the devices and their IDs (it is the recording configuration plus an `id` per device). It sends
only SELECT statements, and fails when the account reads no rows.

record polls the devices of the configuration (it names converter addresses: keep it outside
the repository) and writes the dataset to <dir>/<name>/, which must not exist yet, until
--duration-s has passed or Ctrl-C. Stop the gateway agent's polling of the same converters
first: a converter serves one master at a time.";

/// The environment variables of `load`.
pub const URL_ENV: &str = "CLICKHOUSE_URL";
pub const USER_ENV: &str = "CLICKHOUSE_USER";
pub const PASSWORD_ENV: &str = "CLICKHOUSE_PASSWORD";
pub const CA_FILE_ENV: &str = "CLICKHOUSE_CA_FILE";

/// How `load` moves the dataset's times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    By(TimeShift),
    /// `None` is the current time when the command runs.
    EndBefore {
        at: Option<DateTime<Utc>>,
        align: TimeDelta,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `--help`: prints [`USAGE`].
    Help,
    Check {
        index: PathBuf,
    },
    Load {
        index: PathBuf,
        tenant: Uuid,
        gateway: Uuid,
        devices: Vec<(String, Uuid)>,
        shift: Shift,
    },
    Export {
        config: PathBuf,
        tenant: Uuid,
        /// The gateway's ID as the protocol defines it (text).
        gateway: String,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        out: PathBuf,
    },
    Record {
        config: PathBuf,
        out: PathBuf,
        /// `None`: until Ctrl-C.
        duration: Option<Duration>,
    },
}

fn uuid(flag: &str, text: &str) -> Result<Uuid, String> {
    Uuid::parse_str(text).map_err(|_| format!("{flag}: `{text}` is not a UUID"))
}

/// Parses the arguments after the program name.
///
/// # Errors
///
/// A message for the user, followed by [`USAGE`] by the caller.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let (command, rest) = args.split_first().ok_or("no command")?;
    if matches!(command.as_str(), "--help" | "-h" | "help") {
        return Ok(Command::Help);
    }
    let (index, rest) = rest.split_first().ok_or("no index file")?;
    if index.starts_with("--") {
        return Err("the index file comes before the options".to_owned());
    }
    let index = PathBuf::from(index);
    match command.as_str() {
        "check" if rest.is_empty() => Ok(Command::Check { index }),
        "check" => Err("check takes only the index file".to_owned()),
        "load" => parse_load(index, rest),
        "export" => parse_export(index, rest),
        "record" => parse_record(index, rest),
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_export(config: PathBuf, rest: &[String]) -> Result<Command, String> {
    let (mut tenant, mut gateway, mut from, mut to, mut out) = (None, None, None, None, None);
    let mut options = rest.iter();
    while let Some(flag) = options.next() {
        let value = options
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let given = |set: bool| {
            if set {
                Err(format!("{flag} is given twice"))
            } else {
                Ok(())
            }
        };
        match flag.as_str() {
            "--tenant" => {
                given(tenant.is_some())?;
                tenant = Some(uuid(flag, value)?);
            }
            "--gateway" => {
                given(gateway.is_some())?;
                protocol::GatewayId::new(value.as_str())
                    .map_err(|error| format!("--gateway: {error}"))?;
                gateway = Some(value.clone());
            }
            "--from" => {
                given(from.is_some())?;
                from = Some(time::parse(value).map_err(|error| format!("--from: {error}"))?);
            }
            "--to" => {
                given(to.is_some())?;
                to = Some(time::parse(value).map_err(|error| format!("--to: {error}"))?);
            }
            "--out" => {
                given(out.is_some())?;
                out = Some(PathBuf::from(value));
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    let from = from.ok_or("--from is required")?;
    let to = to.ok_or("--to is required")?;
    if from >= to {
        return Err("--from must be before --to".to_owned());
    }
    Ok(Command::Export {
        config,
        tenant: tenant.ok_or("--tenant is required")?,
        gateway: gateway.ok_or("--gateway is required")?,
        from,
        to,
        out: out.ok_or("--out is required")?,
    })
}

fn parse_record(config: PathBuf, rest: &[String]) -> Result<Command, String> {
    let (mut out, mut duration) = (None, None);
    let mut options = rest.iter();
    while let Some(flag) = options.next() {
        let value = options
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--out" if out.is_none() => out = Some(PathBuf::from(value)),
            "--duration-s" if duration.is_none() => {
                let seconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| format!("--duration-s: `{value}` is not a positive number"))?;
                duration = Some(Duration::from_secs(seconds));
            }
            "--out" | "--duration-s" => return Err(format!("{flag} is given twice")),
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    Ok(Command::Record {
        config,
        out: out.ok_or("--out is required")?,
        duration,
    })
}

/// Completes at Ctrl-C, or after `duration` when one is given.
async fn stop_after(duration: Option<Duration>) {
    let interrupted = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // Without signal handling, only the duration ends the recording.
            std::future::pending::<()>().await;
        }
    };
    match duration {
        Some(duration) => {
            tokio::select! {
                () = tokio::time::sleep(duration) => {}
                () = interrupted => {}
            }
        }
        None => interrupted.await,
    }
}

fn parse_load(index: PathBuf, rest: &[String]) -> Result<Command, String> {
    let (mut tenant, mut gateway, mut devices) = (None, None, Vec::new());
    let (mut shift_ms, mut end_before, mut align) = (None, None, None);
    let mut options = rest.iter();
    while let Some(flag) = options.next() {
        let value = options
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let repeated = |slot_set: bool| {
            if slot_set {
                Err(format!("{flag} is given twice"))
            } else {
                Ok(())
            }
        };
        match flag.as_str() {
            "--tenant" => {
                repeated(tenant.is_some())?;
                tenant = Some(uuid(flag, value)?);
            }
            "--gateway" => {
                repeated(gateway.is_some())?;
                gateway = Some(uuid(flag, value)?);
            }
            "--device" => {
                let (key, id) = value
                    .split_once('=')
                    .ok_or_else(|| format!("--device: `{value}` is not <key>=<uuid>"))?;
                if devices.iter().any(|(seen, _)| seen == key) {
                    return Err(format!("--device: `{key}` is given twice"));
                }
                devices.push((key.to_owned(), uuid(flag, id)?));
            }
            "--shift-ms" => {
                repeated(shift_ms.is_some())?;
                shift_ms = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| format!("--shift-ms: `{value}` is not a whole number"))?,
                );
            }
            "--end-before" => {
                repeated(end_before.is_some())?;
                end_before = Some(if value == "now" {
                    None
                } else {
                    Some(time::parse(value).map_err(|error| format!("--end-before: {error}"))?)
                });
            }
            "--align-s" => {
                repeated(align.is_some())?;
                let seconds = value
                    .parse::<i64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .and_then(TimeDelta::try_seconds)
                    .ok_or_else(|| format!("--align-s: `{value}` is not a positive number"))?;
                align = Some(seconds);
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    let tenant = tenant.ok_or("--tenant is required")?;
    let gateway = gateway.ok_or("--gateway is required")?;
    let shift = match (shift_ms, end_before, align) {
        (Some(_), Some(_), _) => {
            return Err("--shift-ms and --end-before exclude each other".to_owned());
        }
        (Some(_), None, Some(_)) | (None, None, Some(_)) => {
            return Err("--align-s goes with --end-before".to_owned());
        }
        (Some(millis), None, None) => Shift::By(TimeShift::from_millis(millis)),
        (None, Some(at), align) => Shift::EndBefore {
            at,
            align: align.unwrap_or_else(|| TimeDelta::days(1)),
        },
        (None, None, None) => Shift::By(TimeShift::NONE),
    };
    Ok(Command::Load {
        index,
        tenant,
        gateway,
        devices,
        shift,
    })
}

/// The ClickHouse the environment names: the URL, the account and its password, and for
/// `https://` the one CA it trusts. Nothing here comes from the command line.
fn clickhouse_from(env: &impl Fn(&str) -> Option<String>) -> Result<ClickHouse, String> {
    let required = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{name} is not set"))
    };
    let url = required(URL_ENV)?;
    let user = required(USER_ENV)?;
    let password = required(PASSWORD_ENV)?;
    let ca_file = env(CA_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    ClickHouse::new(&url, &user, &password, ca_file.as_deref()).map_err(|error| error.to_string())
}

/// What `check` prints.
#[derive(Serialize)]
struct Summary<'a> {
    dataset: &'a str,
    version: u32,
    segments: usize,
    rows: u64,
    start: String,
    end: String,
    anomalies: usize,
}

/// Runs a command; `env` reads an environment variable. Returns the JSON the command prints.
///
/// # Errors
///
/// A message for the user.
pub async fn run(command: Command, env: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    match command {
        Command::Help => Ok(USAGE.to_owned()),
        Command::Check { index } => {
            let dataset = Dataset::open(&index).map_err(|error| error.to_string())?;
            let (start, end) = dataset.span();
            let summary = Summary {
                dataset: &dataset.index().name,
                version: dataset.index().version,
                segments: dataset.index().segments.len(),
                rows: dataset.index().segments.iter().map(|s| s.rows).sum(),
                start: time::format(start),
                end: time::format(end),
                anomalies: dataset.anomalies().len(),
            };
            serde_json::to_string_pretty(&summary).map_err(|error| error.to_string())
        }
        Command::Load {
            index,
            tenant,
            gateway,
            devices,
            shift,
        } => {
            let clickhouse = clickhouse_from(&env)?;
            let dataset = Dataset::open(&index).map_err(|error| error.to_string())?;
            let shift = match shift {
                Shift::By(shift) => shift,
                Shift::EndBefore { at, align } => dataset
                    .shift_ending_before(at.unwrap_or_else(Utc::now), align)
                    .map_err(|error| error.to_string())?,
            };
            let target = devices
                .into_iter()
                .fold(Target::new(tenant, gateway), |target, (key, id)| {
                    target.with_device(key, id)
                });
            let loaded = dataset
                .load(&clickhouse, &target, shift)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string_pretty(&loaded).map_err(|error| error.to_string())
        }
        Command::Export {
            config,
            tenant,
            gateway,
            from,
            to,
            out,
        } => {
            // The configuration and the arguments are checked before the environment is read.
            let config = export::Config::read(&config).map_err(|error| error.to_string())?;
            let exporter = Exporter::new(&config).map_err(|error| error.to_string())?;
            let clickhouse = clickhouse_from(&env)?;
            let exported = exporter
                .run(&clickhouse, tenant, &gateway, from, to, &out)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string_pretty(&exported).map_err(|error| error.to_string())
        }
        Command::Record {
            config,
            out,
            duration,
        } => {
            let config = Config::read(&config).map_err(|error| error.to_string())?;
            let recorder = Recorder::new(config).map_err(|error| error.to_string())?;
            let recorded = recorder
                .run(
                    Recorder::endpoints(),
                    &out,
                    Utc::now(),
                    stop_after(duration),
                )
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_string_pretty(&recorded).map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    const T: &str = "00000000-0000-0000-0000-00000000000a";
    const G: &str = "00000000-0000-0000-0000-00000000000b";
    const F: &str = "2026-10-30T16:08:00.000Z";
    const U: &str = "2026-10-31T16:08:00.000Z";

    #[test]
    fn load_options() {
        let command = parse(&args(&format!(
            "load d/index.toml --tenant {T} --gateway {G} --device meter-1={G} --end-before now"
        )))
        .unwrap();
        let Command::Load {
            tenant,
            devices,
            shift,
            ..
        } = command
        else {
            panic!("not a load");
        };
        assert_eq!(tenant, Uuid::parse_str(T).unwrap());
        assert_eq!(
            devices,
            vec![("meter-1".to_owned(), Uuid::parse_str(G).unwrap())]
        );
        assert_eq!(
            shift,
            Shift::EndBefore {
                at: None,
                align: TimeDelta::days(1)
            }
        );
        let Command::Load { shift, .. } = parse(&args(&format!(
            "load d/index.toml --tenant {T} --gateway {G} --shift-ms -5"
        )))
        .unwrap() else {
            panic!("not a load");
        };
        assert_eq!(shift, Shift::By(TimeShift::from_millis(-5)));
        let Command::Load { shift, .. } = parse(&args(&format!(
            "load d/index.toml --tenant {T} --gateway {G} --end-before 2026-10-25T00:00:00.000Z --align-s 3600"
        )))
        .unwrap() else {
            panic!("not a load");
        };
        assert_eq!(
            shift,
            Shift::EndBefore {
                at: Some(time::parse("2026-10-25T00:00:00.000Z").unwrap()),
                align: TimeDelta::hours(1)
            }
        );
    }

    #[test]
    fn record_options() {
        assert_eq!(
            parse(&args("record lab.toml --out recordings --duration-s 60")).unwrap(),
            Command::Record {
                config: PathBuf::from("lab.toml"),
                out: PathBuf::from("recordings"),
                duration: Some(Duration::from_secs(60)),
            }
        );
        assert_eq!(
            parse(&args("record lab.toml --out recordings")).unwrap(),
            Command::Record {
                config: PathBuf::from("lab.toml"),
                out: PathBuf::from("recordings"),
                duration: None,
            }
        );
    }

    #[test]
    fn export_options() {
        assert_eq!(
            parse(&args(&format!(
                "export lab.toml --tenant {T} --gateway gw-1 --from 2026-10-30T16:08:00.000Z \
                 --to 2026-11-13T16:08:00.000Z --out exports"
            )))
            .unwrap(),
            Command::Export {
                config: PathBuf::from("lab.toml"),
                tenant: Uuid::parse_str(T).unwrap(),
                gateway: "gw-1".to_owned(),
                from: time::parse("2026-10-30T16:08:00.000Z").unwrap(),
                to: time::parse("2026-11-13T16:08:00.000Z").unwrap(),
                out: PathBuf::from("exports"),
            }
        );
    }

    #[test]
    fn help() {
        assert_eq!(parse(&args("--help")).unwrap(), Command::Help);
        assert_eq!(parse(&args("help")).unwrap(), Command::Help);
    }

    #[test]
    fn bad_arguments_are_refused() {
        for (text, expected) in [
            ("", "no command"),
            ("check", "no index"),
            ("check i extra", "only the index"),
            ("frobnicate i", "unknown command"),
            ("load --tenant x", "before the options"),
            (&format!("load i --gateway {G}"), "--tenant is required"),
            (&format!("load i --tenant {T}"), "--gateway is required"),
            (&format!("load i --tenant {T} --tenant {T}"), "twice"),
            ("load i --tenant nope", "not a UUID"),
            (
                &format!("load i --tenant {T} --gateway {G} --device x"),
                "<key>=<uuid>",
            ),
            (
                &format!("load i --tenant {T} --gateway {G} --shift-ms 1 --end-before now"),
                "exclude",
            ),
            (
                &format!("load i --tenant {T} --gateway {G} --align-s 60"),
                "goes with",
            ),
            (
                &format!("load i --tenant {T} --gateway {G} --end-before now --align-s 0"),
                "positive",
            ),
            (
                &format!("load i --tenant {T} --gateway {G} --end-before yesterday"),
                "--end-before",
            ),
            (
                &format!("load i --tenant {T} --gateway {G} --password x"),
                "unknown option",
            ),
            (&format!("load i --tenant {T} --gateway"), "needs a value"),
            (
                &format!("export c.toml --gateway g --from {F} --to {U} --out o"),
                "--tenant is required",
            ),
            (
                &format!("export c.toml --tenant {T} --from {F} --to {U} --out o"),
                "--gateway is required",
            ),
            (
                &format!("export c.toml --tenant {T} --gateway g --to {U} --out o"),
                "--from is required",
            ),
            (
                &format!("export c.toml --tenant {T} --gateway g --from {F} --out o"),
                "--to is required",
            ),
            (
                &format!("export c.toml --tenant {T} --gateway g --from {F} --to {U}"),
                "--out is required",
            ),
            (
                &format!("export c.toml --tenant {T} --gateway g --from {U} --to {F} --out o"),
                "before --to",
            ),
            (
                &format!("export c.toml --tenant {T} --gateway g --from {F} --to {F} --out o"),
                "before --to",
            ),
            (
                &format!(
                    "export c.toml --tenant {T} --gateway g --from yesterday --to {U} --out o"
                ),
                "--from",
            ),
            (
                &format!("export c.toml --tenant {T} --gateway bad/id --from {F} --to {U} --out o"),
                "--gateway",
            ),
            (
                &format!(
                    "export c.toml --tenant {T} --gateway g --from {F} --to {U} --out o --out p"
                ),
                "twice",
            ),
            (
                &format!(
                    "export c.toml --tenant {T} --gateway g --from {F} --to {U} --out o --password x"
                ),
                "unknown option",
            ),
            ("record c.toml", "--out is required"),
            ("record c.toml --out a --out b", "twice"),
            ("record c.toml --out a --duration-s 0", "positive"),
            ("record c.toml --out a --host x", "unknown option"),
        ] {
            let error = parse(&args(text)).expect_err(text);
            assert!(
                error.contains(expected),
                "{text}: `{error}` lacks `{expected}`"
            );
        }
    }

    #[tokio::test]
    async fn load_needs_its_environment() {
        let command = parse(&args(&format!(
            "load ../../datasets/lab/synthetic/index.toml --tenant {T} --gateway {G}"
        )))
        .unwrap();
        let error = run(command.clone(), |_| None).await.unwrap_err();
        assert!(error.contains("CLICKHOUSE_URL is not set"), "{error}");
        let error = run(command, |name| {
            (name != PASSWORD_ENV).then(|| "http://127.0.0.1:1".to_owned())
        })
        .await
        .unwrap_err();
        assert!(error.contains("CLICKHOUSE_PASSWORD is not set"), "{error}");
    }
}
