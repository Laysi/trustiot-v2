//! The root CRL while the agent runs (Laysi/trustiot-v2#563; README.md, 「根 CRL 的更新」).
//!
//! The certs service hands out the root CRL it adopted at `GET`
//! [`protocol::root_crl::ROOT_CRL_PATH`] on the gateway host ([`Source`], in production the
//! renewal's [`crate::renewal::HttpsEndpoint`]: the host and port of `cloud.url`, the outer TLS
//! settings, nothing else). [`Updater::run`] asks a random time within
//! [`Schedule::first_within`] after start (at once while the root CRL is withheld), then every
//! [`Schedule::interval`] with jitter (between half of it and all of it), and after a failure
//! sooner, with backoff ([`Schedule::retry`]). Nothing here stops the agent or the MQTT
//! connection; every failure is logged with its reason and the root CRL held stays in use.
//!
//! **Only forward.** A root CRL fetched is taken only when one of the trust anchors of `broker.ca`
//! signed it (the agent's own load, [`RootCrl::from_bytes`]), it has a CRL number, it is signed by
//! the same root as the root CRL held, and its number is larger than the one held (and than the
//! highest one accepted, see below). Anything else is refused, logged, and the one held stays.
//! The same number with the same CRL changes nothing; the same number with another CRL is
//! refused: a CRL number names one CRL.
//!
//! **Kept across restarts.** A root CRL taken is written over `broker.crl` atomically, as
//! `provision install` writes it ([`crate::provision::install`]: a new file in the same open
//! directory, flushed, renamed over the old one, the directory flushed), so a power cut leaves
//! the old file or the new one, never a broken one; the next start loads it. When it cannot be
//! written it is used anyway until the agent restarts, the error is logged, and the next update
//! tries again, whatever the source answers then (every update brings memory and disk in line,
//! [`Updater::update`]).
//!
//! **The highest number accepted.** Next to `broker.crl` the file `<broker.crl>.highest`
//! ([`MARKER_SUFFIX`]) holds the root ([`RootKey`]) and the highest CRL number this gateway
//! accepted, written after `broker.crl` in the same atomic way. At start ([`load`]), a
//! `broker.crl` that is gone, has no number, or has a lower number than that file says, for a root
//! still in `broker.ca`, does not turn revocation checking off: the root CRL is
//! [`RootCrl::Withheld`], no inner TLS connection opens, and the updater asks at once until a root
//! CRL with at least that number is in place. A damaged marker counts as "a root CRL was accepted,
//! number unknown". A marker of a root that is no longer in `broker.ca` (the root changed), or of
//! another root than a usable root CRL in `broker.crl`, is ignored, and replaced together with
//! `broker.crl` as soon as the agent holds a root CRL of a root in `broker.ca`: a power cut
//! between the two renames of a root change leaves the new file with the old marker, and
//! `provision install` without a root CRL leaves the old marker when it changes the root.
//!
//! **One lock with `provision install`.** Every read of `broker.crl` and the marker for a
//! decision, and every write, happens under the `flock` of their directory, the lock `provision
//! install` takes before it plans the root CRL ([`crate::directory::Directory::lock`]), on the
//! blocking pool. So an install and an update never both decide on the same old files: whichever
//! goes second sees what the first wrote, and the forward-only rule applies to it. A newer root
//! CRL an install wrote is taken over from the file. Under the same lock `broker.ca` is read
//! again (Laysi/trustiot-v2#926): an install holds the lock while it renames `broker.ca`, so the
//! decision sees the trust anchors the install left. While `broker.ca` holds other roots than the
//! ones in use (an install changed the root and the agent has not restarted yet), or cannot be
//! read, nothing is written: a root CRL of the old root would keep the agent from starting.
//!
//! **The connection after a new root CRL.** Taking a root CRL builds new inner TLS settings
//! with a new, empty resumption cache and raises their version ([`InnerConfig::use_root_crl`]).
//! The connection in use checks the chain its broker presented again under them
//! ([`InnerConfig::check_broker`], through [`crate::link::Connector::trust_check`]): a broker
//! that fails (an issuing CA the new root CRL revokes) is dropped at once, with nothing more
//! written to it and every publish without its PUBACK ending as lost, so the offline buffer sends
//! it again; one that passes finishes what it accepted first. Either way the link reconnects at
//! once, and the inner handshake is a full one, which checks the broker's chain against the new
//! root CRL.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::future::Future;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::health::MAX_CRL_NUMBER_DIGITS;
use rustls_pki_types::UnixTime;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::backoff::{Backoff, BackoffSettings};
use crate::directory::Directory;
use crate::link::printable;
use crate::provision::install::{self, Owner, Target};
use crate::tls::{self, BrokerTrust, InnerConfig, RootCrl, RootKey, TlsError, crl_number_cmp};

/// The suffix of the file next to `broker.crl` that holds the highest root CRL number accepted.
pub const MARKER_SUFFIX: &str = ".highest";
/// The largest marker file read; it holds two short lines.
pub const MAX_MARKER_BYTES: u64 = 1024;

/// When the updater asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// The first request comes a random time up to this after start, so gateways restarted
    /// together do not ask together; at once while the root CRL is withheld.
    pub first_within: Duration,
    /// The time between requests: a random time between half of it and all of it.
    pub interval: Duration,
    /// After a failure, or while the root CRL is withheld: the next request comes sooner, with
    /// this backoff.
    pub retry: BackoffSettings,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            first_within: Duration::from_secs(5 * 60),
            // The root CA signs a root CRL in an offline ceremony, rarely; a revocation reaches
            // every gateway within 6 hours of the certs service adopting it.
            interval: Duration::from_secs(6 * 60 * 60),
            retry: BackoffSettings {
                base: Duration::from_secs(30),
                cap: Duration::from_secs(30 * 60),
            },
        }
    }
}

/// The answer to a root CRL request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched {
    /// 200: the body, at most [`root_crl::MAX_BYTES`].
    Crl(Vec<u8>),
    /// 404: the certs service has no root CRL to hand out.
    NotPublished,
    /// Any other status.
    Status(u16),
}

/// Where root CRLs come from: [`crate::renewal::HttpsEndpoint`], or a stand-in in tests.
pub trait Source: Send + Sync {
    /// Asks for the root CRL.
    ///
    /// # Errors
    ///
    /// The endpoint cannot be reached, or does not answer in time or within the size limit.
    fn fetch(&self) -> impl Future<Output = io::Result<Fetched>> + Send;

    /// Where it asks, for log lines.
    fn describe(&self) -> String;
}

/// The marker: the highest root CRL number this gateway accepted, and the root that signed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marker {
    /// No marker: no root CRL accepted since the agent knows about markers.
    Absent,
    /// A marker that cannot be read: a root CRL was accepted, its root and number are unknown.
    Damaged(String),
    /// A root CRL of `root` with `number` was accepted.
    At {
        /// The root.
        root: RootKey,
        /// The number, decimal.
        number: String,
    },
}

/// The marker file, as TOML.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkerFile {
    root_key_sha256: String,
    number: String,
}

impl Marker {
    /// The marker in `bytes` (a file that exists): [`Marker::At`] or [`Marker::Damaged`].
    pub fn parse(bytes: &[u8]) -> Self {
        if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_MARKER_BYTES) {
            return Self::Damaged(format!("larger than {MAX_MARKER_BYTES} bytes"));
        }
        let parsed = std::str::from_utf8(bytes)
            .map_err(|_| "not UTF-8".to_owned())
            .and_then(|text| {
                toml::from_str::<MarkerFile>(text).map_err(|error| error.message().to_owned())
            })
            .and_then(|file| {
                let root = RootKey::from_hex(&file.root_key_sha256)
                    .ok_or_else(|| "root_key_sha256 is not 64 lowercase hex digits".to_owned())?;
                if !is_crl_number(&file.number) {
                    return Err(format!(
                        "number is not a decimal number of 1 to {MAX_CRL_NUMBER_DIGITS} digits"
                    ));
                }
                Ok(Self::At {
                    root,
                    number: file.number,
                })
            });
        parsed.unwrap_or_else(|reason| Self::Damaged(printable(&reason, 200)))
    }

    /// The file that holds a root CRL of `root` with `number`.
    pub fn text(root: &RootKey, number: &str) -> String {
        format!(
            "# The highest root CRL number this gateway accepted, and the root that signed it\n\
             # (the SHA-256 of its SubjectPublicKeyInfo). Written by trustiot-agent and\n\
             # `trustiot-agent provision install` only: see the agent's README, 「根 CRL 的更新」.\n\
             root_key_sha256 = \"{}\"\nnumber = \"{number}\"\n",
            root.to_hex()
        )
    }

    /// The number a marker for a root CRL of `root` with `number` holds after this one: never
    /// lower than this one's for the same root.
    pub fn raised(&self, root: &RootKey, number: &str) -> String {
        match self {
            Self::At {
                root: held,
                number: highest,
            } if held == root && crl_number_cmp(highest, number).is_gt() => highest.clone(),
            Self::Absent | Self::Damaged(_) | Self::At { .. } => number.to_owned(),
        }
    }
}

/// Whether `text` is a CRL number as the heartbeat carries it.
fn is_crl_number(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_CRL_NUMBER_DIGITS
        && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The name of the marker of the root CRL file `name`.
pub(crate) fn marker_name(name: &OsStr) -> OsString {
    let mut marker = name.to_owned();
    marker.push(MARKER_SUFFIX);
    marker
}

/// The marker's path for `broker.crl` at `path`.
pub fn marker_path(path: &Path) -> PathBuf {
    let mut marker = path.as_os_str().to_owned();
    marker.push(MARKER_SUFFIX);
    PathBuf::from(marker)
}

/// What `broker.crl` holds.
#[derive(Clone, Debug)]
pub(crate) enum OnDisk {
    /// Nothing.
    Absent,
    /// A root CRL that passes under `broker.ca` ([`RootCrl::Loaded`]).
    Usable(RootCrl),
    /// Something else, and why.
    Unusable(String),
}

/// What [`decide`] makes of a candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// There was none.
    None,
    /// It is the root CRL held.
    Same,
    /// It is newer: taken.
    Taken,
    /// Refused, and why.
    Refused(String),
}

/// What to hold and write, by the forward-only rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    /// The root CRL to hold from now on.
    pub(crate) held: RootCrl,
    /// Whether to write `held` (then [`RootCrl::Loaded`]) over `broker.crl`.
    pub(crate) write_file: bool,
    /// The marker to write, if it changes.
    pub(crate) write_marker: Option<String>,
    /// What became of the candidate.
    pub(crate) verdict: Verdict,
}

fn number_of(crl: &RootCrl) -> Option<&str> {
    match crl {
        RootCrl::Loaded { number, .. } => number.as_deref(),
        RootCrl::NotConfigured | RootCrl::Missing(_) | RootCrl::Withheld { .. } => None,
    }
}

/// `a` against `b`; no number is lower than every number.
fn cmp_numbers(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => crl_number_cmp(a, b),
        (a, b) => a.is_some().cmp(&b.is_some()),
    }
}

/// The forward-only rule (the module documentation), as one decision: with `held` in memory,
/// `disk` in `broker.crl` (at `path`), `marker` next to it and `candidate` (a root CRL that passed
/// [`RootCrl::from_bytes`] under `trust`, or none), what to hold from now on and what to write.
/// Writes only move forward: never a lower number for the same root. `trust` must be what
/// `broker.ca` holds when the writes happen ([`adopt`] makes sure of that).
pub(crate) fn decide(
    path: &Path,
    trust: &BrokerTrust,
    held: &RootCrl,
    disk: &OnDisk,
    marker: &Marker,
    candidate: Option<&RootCrl>,
) -> Decision {
    // A root CRL on disk passed `RootCrl::from_bytes` under `trust`: its root is a trust anchor.
    let disk_root = match disk {
        OnDisk::Usable(RootCrl::Loaded { root, .. }) => Some(*root),
        OnDisk::Absent | OnDisk::Unusable(_) | OnDisk::Usable(_) => None,
    };
    // A marker that does not count: of a root no longer in broker.ca (the root changed under this
    // process), or of another root than the root CRL of a trust anchor in broker.crl (left from
    // before a root change: `provision install` renames broker.crl first, and a power cut can
    // come before the marker's rename). The marker guards one root's numbers; changing the root
    // takes writing broker.crl, as `provision install` does.
    let foreign = matches!(
        marker,
        Marker::At { root, .. }
            if !trust.holds(root) || disk_root.is_some_and(|disk_root| disk_root != *root)
    );
    let marker_root = match marker {
        Marker::At { root, .. } if !foreign => Some(*root),
        Marker::Absent | Marker::Damaged(_) | Marker::At { .. } => None,
    };
    // The root whose root CRLs count: the one held, else the marker's, else the file's (one from
    // before markers existed), so a candidate never switches roots.
    let reference = match held {
        RootCrl::Loaded { root, .. } => Some(*root),
        RootCrl::Withheld { root, .. } => root.or(marker_root).or(disk_root),
        RootCrl::NotConfigured | RootCrl::Missing(_) => marker_root.or(disk_root),
    };
    let same_root = |crl: &RootCrl| match crl {
        RootCrl::Loaded { root, .. } => reference.is_none_or(|reference| *root == reference),
        RootCrl::NotConfigured | RootCrl::Missing(_) | RootCrl::Withheld { .. } => false,
    };
    // Whether a root CRL was accepted, and the highest number accepted.
    let mut accepted = matches!(held, RootCrl::Loaded { .. } | RootCrl::Withheld { .. })
        || matches!(marker, Marker::Damaged(_));
    let mut floor: Option<String> = None;
    let mut raise = |number: Option<&str>| {
        if let Some(number) = number
            && cmp_numbers(Some(number), floor.as_deref()).is_gt()
        {
            floor = Some(number.to_owned());
        }
    };
    match held {
        RootCrl::Loaded { number, .. } | RootCrl::Withheld { number, .. } => {
            raise(number.as_deref());
        }
        RootCrl::NotConfigured | RootCrl::Missing(_) => {}
    }
    if let Marker::At { root, number } = marker
        && !foreign
        && Some(*root) == reference
    {
        accepted = true;
        raise(Some(number));
    }

    // The newest root CRL of the reference root among the one held and the one on disk.
    let mut best: Option<&RootCrl> = matches!(held, RootCrl::Loaded { .. }).then_some(held);
    if let OnDisk::Usable(on_disk) = disk
        && same_root(on_disk)
        && best.is_none_or(|best| cmp_numbers(number_of(on_disk), number_of(best)).is_gt())
    {
        best = Some(on_disk);
    }
    let mut verdict = Verdict::None;
    if let Some(candidate) = candidate {
        verdict = match (candidate, number_of(candidate)) {
            (RootCrl::Loaded { .. }, None) => {
                Verdict::Refused("it has no CRL number, by which the gateway keeps the newest one".to_owned())
            }
            (RootCrl::Loaded { .. }, Some(number)) if !is_crl_number(number) => Verdict::Refused(
                format!("its CRL number has more than {MAX_CRL_NUMBER_DIGITS} digits"),
            ),
            (RootCrl::Loaded { .. }, Some(_)) if !same_root(candidate) => Verdict::Refused(
                "it is signed by another root than the root CRL this gateway accepted; a new root comes with `provision install`".to_owned(),
            ),
            (RootCrl::Loaded { .. }, Some(number))
                if cmp_numbers(Some(number), floor.as_deref()).is_lt() =>
            {
                Verdict::Refused(format!(
                    "its number {number} is lower than {}, the highest this gateway accepted",
                    floor.as_deref().unwrap_or("none")
                ))
            }
            (RootCrl::Loaded { der, .. }, Some(number)) => {
                let against_best =
                    best.map(|best| (best, cmp_numbers(Some(number), number_of(best))));
                match against_best {
                    Some((RootCrl::Loaded { der: best_der, .. }, std::cmp::Ordering::Equal))
                        if best_der == der =>
                    {
                        Verdict::Same
                    }
                    Some((_, std::cmp::Ordering::Equal)) => Verdict::Refused(format!(
                        "its number {number} is the number of the root CRL held, but the CRL is another one; a CRL number names one CRL"
                    )),
                    Some((best, std::cmp::Ordering::Less)) => Verdict::Refused(format!(
                        "its number {number} is lower than {}, the number of the root CRL held",
                        number_of(best).unwrap_or("none")
                    )),
                    _ => {
                        best = Some(candidate);
                        Verdict::Taken
                    }
                }
            }
            (RootCrl::NotConfigured | RootCrl::Missing(_) | RootCrl::Withheld { .. }, _) => {
                Verdict::Refused("it is not a root CRL".to_owned())
            }
        };
    }

    let new_held = match best {
        Some(best) if cmp_numbers(number_of(best), floor.as_deref()).is_ge() => best.clone(),
        _ if accepted => RootCrl::Withheld {
            path: path.to_owned(),
            root: reference,
            number: floor,
        },
        _ => RootCrl::Missing(path.to_owned()),
    };
    let mut write_file = false;
    let mut write_marker = None;
    // A foreign marker is stale here: `trust` is what broker.ca holds ([`adopt`] reads broker.ca
    // again under the lock and writes nothing when it holds other roots, which is how an install
    // that changed the root under this process shows), so the marker's root is gone from
    // broker.ca, or broker.crl holds another trust anchor's root CRL (a power cut between the two
    // renames of a root change). It is replaced, or a later deletion of broker.crl would look like
    // "never accepted"; until then a fetched root CRL could never be saved (Laysi/trustiot-v2#926).
    // Only with broker.crl gone or a root CRL of these trust anchors, though: an unusable
    // broker.crl under a foreign marker may be the marker root's own root CRL, written by
    // something that changed broker.ca without the lock, and is left alone.
    let may_write = !foreign || !matches!(disk, OnDisk::Unusable(_));
    if may_write
        && let RootCrl::Loaded {
            der, root, number, ..
        } = &new_held
    {
        write_file = match disk {
            OnDisk::Absent | OnDisk::Unusable(_) => true,
            OnDisk::Usable(RootCrl::Loaded {
                der: on_disk,
                root: disk_root,
                number: disk_number,
                ..
            }) => {
                on_disk != der
                    && disk_root == root
                    && cmp_numbers(disk_number.as_deref(), number.as_deref()).is_lt()
            }
            OnDisk::Usable(_) => false,
        };
        if let Some(number) = number {
            let wanted = Marker::At {
                root: *root,
                number: marker.raised(root, number),
            };
            if *marker != wanted
                && let Marker::At { root, number } = &wanted
            {
                write_marker = Some(Marker::text(root, number));
            }
        }
    }
    Decision {
        held: new_held,
        write_file,
        write_marker,
        verdict,
    }
}

/// Reads the regular file `name` in `directory`, at most `limit` bytes and one more; `None` when
/// nothing is there. A link (not followed), a directory, a FIFO or a device is an error.
fn read_in(directory: &Directory, name: &OsStr, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let not_regular = || io::Error::other("not a regular file (a symbolic link is not followed)");
    match directory.is_file(name)? {
        None => return Ok(None),
        Some(false) => return Err(not_regular()),
        Some(true) => {}
    }
    let file = match directory.open_read(name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Err(not_regular()),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// The marker for `bytes` read (`None`: no file).
fn marker_of(read: io::Result<Option<Vec<u8>>>) -> Marker {
    match read {
        Ok(None) => Marker::Absent,
        Ok(Some(bytes)) => Marker::parse(&bytes),
        Err(error) => Marker::Damaged(error.to_string()),
    }
}

/// The root CRL the agent starts with (`path`: `broker.crl`, `None` when not set): the file, as
/// [`RootCrl::load`] loads it, under the marker's rule (the module documentation): a file that is
/// gone, or older than the highest number accepted, is [`RootCrl::Withheld`]. Nothing is written
/// here (`trustiot-agent --check` runs this too); the updater writes the marker of a root CRL
/// that has none.
///
/// # Errors
///
/// The file exists but cannot be read or used ([`RootCrl::load`]).
pub fn load(path: Option<&Path>, trust: &BrokerTrust, now: UnixTime) -> Result<RootCrl, TlsError> {
    let Some(path) = path else {
        return Ok(RootCrl::NotConfigured);
    };
    let file = RootCrl::load(Some(path), trust, now)?;
    let disk = match &file {
        RootCrl::Loaded { .. } => OnDisk::Usable(file.clone()),
        RootCrl::NotConfigured | RootCrl::Missing(_) | RootCrl::Withheld { .. } => OnDisk::Absent,
    };
    let marker = marker_of(read_marker(&marker_path(path)));
    let decision = decide(
        path,
        trust,
        &RootCrl::Missing(path.to_owned()),
        &disk,
        &marker,
        None,
    );
    Ok(decision.held)
}

/// The marker file at `path`, read as the agent's own file: without following a link or
/// blocking.
fn read_marker(path: &Path) -> io::Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MARKER_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Writes a new file's contents; tests pass one that fails half way.
pub(crate) type Writer = fn(&mut File, &[u8]) -> io::Result<()>;

fn write_all(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)
}

/// What [`adopt`] did.
#[derive(Debug)]
pub(crate) struct Adoption {
    /// The decision.
    pub(crate) decision: Decision,
    /// Whether the files on disk now hold the decision: `Ok`, or why not.
    pub(crate) saved: Result<(), String>,
    /// Why the writes the decision asked for were left out on purpose (`saved` is then `Ok`):
    /// `broker.ca` holds other roots than the ones in use, or cannot be read. Unlike a failed
    /// save, asking again sooner does not help; only a restart with the new roots does.
    pub(crate) held_back: Option<String>,
}

/// [`decide`] on what `broker.crl` (at `path`) and its marker hold, read under their directory's
/// lock, and the writes the decision asks for, under the same lock (the module documentation).
/// When the directory cannot be opened or locked, nothing on disk is read or written, and the
/// decision is made on `held` alone (which never goes back either).
pub(crate) fn adopt(
    path: &Path,
    trust: &BrokerTrust,
    held: &RootCrl,
    candidate: Option<&RootCrl>,
    now: UnixTime,
    writer: Writer,
) -> Adoption {
    let locked = (|| {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(format!("{} is not a file's path", path.display()));
        };
        let directory = Directory::open(parent).map_err(|error| {
            format!(
                "the directory {} cannot be opened without following symbolic links: {error}",
                parent.display()
            )
        })?;
        directory
            .lock()
            .map_err(|error| format!("cannot lock {}: {error}", parent.display()))?;
        Ok((directory, name.to_owned()))
    })();
    let (directory, name) = match locked {
        Ok(locked) => locked,
        Err(error) => {
            let decision = decide(
                path,
                trust,
                held,
                &OnDisk::Absent,
                &Marker::Absent,
                candidate,
            );
            return Adoption {
                decision,
                saved: Err(error),
                held_back: None,
            };
        }
    };
    let disk = match read_in(&directory, &name, tls::MAX_ROOT_CRL_BYTES) {
        Ok(None) => OnDisk::Absent,
        Ok(Some(bytes)) => match RootCrl::from_bytes(path, &bytes, trust, now) {
            Ok(crl) => OnDisk::Usable(crl),
            Err(error) => OnDisk::Unusable(error.to_string()),
        },
        Err(error) => OnDisk::Unusable(error.to_string()),
    };
    let marker_name = marker_name(&name);
    let marker = marker_of(read_in(&directory, &marker_name, MAX_MARKER_BYTES));
    let mut decision = decide(path, trust, held, &disk, &marker, candidate);
    // broker.ca again, under the same lock: `provision install` holds it while it renames
    // broker.ca, so the decision and an install see the same trust anchors. While broker.ca holds
    // other roots than the ones in use (an install changed the root under this process), nothing
    // is written: a root CRL of the old root in broker.crl would keep the agent from starting.
    let anchors = match BrokerTrust::load(trust.path()) {
        Ok(on_disk) if on_disk.same_anchors(trust) => Ok(()),
        Ok(_) => Err(format!(
            "{} holds other roots than the ones this agent started with (`provision install` changed them); nothing is written to {} until the agent restarts",
            trust.path().display(),
            parent_of(path).display()
        )),
        Err(error) => Err(format!(
            "the roots in use cannot be confirmed, so nothing is written to {}: {error}",
            parent_of(path).display()
        )),
    };
    let blocked = match anchors {
        Err(reason) if decision.write_file || decision.write_marker.is_some() => {
            decision.write_file = false;
            decision.write_marker = None;
            Some(reason)
        }
        Ok(()) | Err(_) => None,
    };
    if let OnDisk::Unusable(reason) = &disk {
        tracing::warn!(
            path = %path.display(),
            reason = %printable(reason, 1024),
            replaced = decision.write_file,
            "the root CRL file cannot be used; it never lowers the root CRL in use"
        );
    }
    let saved = match blocked {
        Some(_) => Ok(()),
        None => save(&directory, name, marker_name, &decision, writer),
    };
    // The lock goes with the last descriptor of the directory, here.
    drop(directory);
    Adoption {
        decision,
        saved,
        held_back: blocked,
    }
}

/// The directory of `path`, for messages.
fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or(path)
}

/// Checks that the agent can save root CRLs next to `broker.crl` (at `path`): its directory
/// opens without following a symbolic link and this process may create, rename and remove
/// files in it (write and search permission, by the effective user and groups, and not a
/// read-only file system). Nothing is created or changed. The agent warns at start and
/// `trustiot-agent --check` fails when it cannot (Laysi/trustiot-v2#926): the updater would
/// otherwise keep failing to save, retrying with backoff, and a restart would lose each root CRL
/// fetched since.
///
/// # Errors
///
/// Why the agent cannot write there.
pub fn check_directory(path: &Path) -> Result<(), String> {
    let (Some(parent), Some(_)) = (path.parent(), path.file_name()) else {
        return Err(format!("{} is not a file's path", path.display()));
    };
    let directory = Directory::open(parent).map_err(|error| {
        format!(
            "the directory {} cannot be opened without following symbolic links: {error}",
            parent.display()
        )
    })?;
    directory.writable().map_err(|error| {
        format!(
            "the directory {} is not writable by this account: {error}",
            parent.display()
        )
    })
}

/// The writes of `decision` in `directory`: `broker.crl` (`name`) first, then the marker, each
/// replaced atomically, both with the directory's owner and [`install::FILE_MODE`].
fn save(
    directory: &Directory,
    name: OsString,
    marker_name: OsString,
    decision: &Decision,
    writer: Writer,
) -> Result<(), String> {
    let crl_text = match (&decision.held, decision.write_file) {
        (RootCrl::Loaded { der, .. }, true) => Some(install::pem(install::CRL, der)),
        _ => None,
    };
    if crl_text.is_none() && decision.write_marker.is_none() {
        return Ok(());
    }
    let shown = |error: io::Error| format!("{}: {error}", directory.path().display());
    let metadata = directory.metadata().map_err(shown)?;
    // The owner a root CRL installed there gets too (`provision install`).
    let owner = Owner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    let crl_target = Target {
        directory: directory.try_clone().map_err(shown)?,
        name,
    };
    let marker_target = Target {
        directory: directory.try_clone().map_err(shown)?,
        name: marker_name,
    };
    let mut files = Vec::with_capacity(2);
    if let Some(text) = crl_text {
        files.push((&crl_target, text, owner));
    }
    if let Some(text) = &decision.write_marker {
        files.push((&marker_target, text.clone(), owner));
    }
    let mut write = |file: &mut File, bytes: &[u8]| writer(file, bytes);
    install::replace_all(&files, &mut write).map(|_outcomes| ())
}

/// What one update did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// A newer root CRL is in use now (fetched, or written by `provision install`).
    Adopted {
        /// Its number.
        number: Option<String>,
    },
    /// Nothing changed.
    Unchanged,
    /// The certs service has no root CRL to hand out.
    NotPublished,
    /// The root CRL fetched is refused, and why; the one held stays.
    Refused(String),
}

/// Fetches the root CRL and keeps the newest; see the module documentation.
pub struct Updater<S> {
    source: S,
    path: PathBuf,
    inner: InnerConfig,
    schedule: Schedule,
    jitter: fn() -> u64,
    writer: Writer,
    /// Whether the agent already warned that writes are held back until a restart
    /// ([`Adoption::held_back`]): once is enough.
    held_back_told: AtomicBool,
}

impl<S> std::fmt::Debug for Updater<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Updater")
            .field("path", &self.path)
            .field("schedule", &self.schedule)
            .finish_non_exhaustive()
    }
}

/// A uniformly random duration up to `max`.
fn up_to(max: Duration, random: u64) -> Duration {
    let nanos = max.as_nanos().saturating_mul(u128::from(random)) >> 64;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

impl<S: Source> Updater<S> {
    /// Updates the root CRL at `path` (`broker.crl`) in use in `inner` from `source`.
    pub fn new(source: S, path: PathBuf, inner: InnerConfig) -> Self {
        Self {
            source,
            path,
            inner,
            schedule: Schedule::default(),
            jitter: crate::link::random_u64,
            writer: write_all,
            held_back_told: AtomicBool::new(false),
        }
    }

    /// The same updater with `schedule`.
    #[must_use]
    pub fn with_schedule(mut self, schedule: Schedule) -> Self {
        self.schedule = schedule;
        self
    }

    /// The same updater asking `source` instead, for example a stand-in in tests.
    pub fn with_source<T: Source>(self, source: T) -> Updater<T> {
        Updater {
            source,
            path: self.path,
            inner: self.inner,
            schedule: self.schedule,
            jitter: self.jitter,
            writer: self.writer,
            held_back_told: self.held_back_told,
        }
    }

    /// The same updater writing files with `writer`: tests pass one that fails half way.
    #[cfg(test)]
    pub(crate) fn with_writer(mut self, writer: Writer) -> Self {
        self.writer = writer;
        self
    }

    /// `broker.crl`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The inner TLS settings whose root CRL this updates.
    pub fn inner(&self) -> &InnerConfig {
        &self.inner
    }

    /// Brings memory and disk in line without asking: takes over a newer root CRL that
    /// `provision install` wrote, writes the marker of a root CRL held that has none.
    ///
    /// # Errors
    ///
    /// The files cannot be read or written, or the inner TLS settings cannot be built.
    pub async fn settle(&self) -> Result<Update, String> {
        self.adopt(None).await
    }

    /// Asks once and keeps the newest root CRL. Whatever the answer, memory and disk are brought
    /// in line too ([`Updater::settle`]): a newer root CRL `provision install` wrote is taken
    /// over, and a root CRL whose save failed earlier is saved again, even while the source fails
    /// or has nothing to hand out.
    ///
    /// # Errors
    ///
    /// The source cannot be reached or answers anything but 200 or 404, the files cannot be
    /// written (the root CRL is used anyway), or the inner TLS settings cannot be built.
    pub async fn update(&self) -> Result<Update, String> {
        let fetched = self
            .source
            .fetch()
            .await
            .map_err(|error| format!("asking {}: {error}", self.source.describe()));
        let answer = match fetched {
            Ok(Fetched::Crl(bytes)) => {
                let trust = self.inner.trust();
                match RootCrl::from_bytes(&self.path, &bytes, &trust, crate::setup::clock_now()) {
                    // Reads and writes the files under their lock, as `settle` does.
                    Ok(candidate) => return self.adopt(Some(candidate)).await,
                    Err(TlsError::BadCrl { reason, .. }) => Ok(Update::Refused(reason)),
                    Err(error) => Ok(Update::Refused(error.to_string())),
                }
            }
            Ok(Fetched::NotPublished) => Ok(Update::NotPublished),
            Ok(Fetched::Status(status)) => {
                Err(format!("{} answered HTTP {status}", self.source.describe()))
            }
            Err(error) => Err(error),
        };
        match self.settle().await {
            // The answer still decides the schedule; a change is logged where it happens.
            Ok(_) => answer,
            Err(error) => match answer {
                Ok(_) => Err(error),
                Err(asking) => Err(format!("{asking}; and {error}")),
            },
        }
    }

    async fn adopt(&self, candidate: Option<RootCrl>) -> Result<Update, String> {
        let held = self.inner.root_crl();
        let adoption = {
            let path = self.path.clone();
            let trust = self.inner.trust();
            let held = held.clone();
            let writer = self.writer;
            // The directory's flock waits for `provision install`: off the async threads.
            tokio::task::spawn_blocking(move || {
                adopt(
                    &path,
                    &trust,
                    &held,
                    candidate.as_ref(),
                    crate::setup::clock_now(),
                    writer,
                )
            })
            .await
            .map_err(|error| format!("the root CRL update stopped: {error}"))?
        };
        let Adoption {
            decision,
            saved,
            held_back,
        } = adoption;
        if let Some(reason) = held_back
            && !self.held_back_told.swap(true, Ordering::Relaxed)
        {
            // Not a failed save: asking sooner does not help, so the schedule stays as it is.
            tracing::warn!(
                reason = %printable(&reason, 1024),
                "root CRLs are used but not saved until the agent restarts"
            );
        }
        let changed = decision.held != held;
        if changed {
            self.inner
                .use_root_crl(decision.held.clone())
                .map_err(|error| format!("the inner TLS settings cannot be built: {error}"))?;
            log_change(&held, &decision.held);
        }
        if let Err(error) = saved {
            return Err(format!(
                "the root CRL cannot be saved to {}: {error}; the one in use stays in use until the agent restarts, and the next update saves it again",
                self.path.display()
            ));
        }
        Ok(match decision.verdict {
            Verdict::Refused(reason) => Update::Refused(reason),
            Verdict::None | Verdict::Same | Verdict::Taken => match &decision.held {
                RootCrl::Loaded { number, .. } if changed => Update::Adopted {
                    number: number.clone(),
                },
                _ => Update::Unchanged,
            },
        })
    }

    /// Updates on schedule until `shutdown`; see the module documentation.
    pub async fn run(self, shutdown: CancellationToken) {
        let settled = tokio::select! {
            () = shutdown.cancelled() => return,
            settled = self.settle() => settled,
        };
        if let Err(error) = settled {
            tracing::error!(error = %printable(&error, 1024), "root CRL");
        }
        let mut retry = Backoff::new(self.schedule.retry);
        let mut delay = if self.withheld() {
            Duration::ZERO
        } else {
            up_to(self.schedule.first_within, (self.jitter)())
        };
        loop {
            let updated = tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(delay) => tokio::select! {
                    () = shutdown.cancelled() => return,
                    updated = self.update() => updated,
                },
            };
            let succeeded = match &updated {
                Ok(Update::Adopted { .. } | Update::Unchanged | Update::NotPublished) => true,
                Ok(Update::Refused(_)) | Err(_) => false,
            };
            match updated {
                Ok(Update::Adopted { .. } | Update::Unchanged) => {}
                // Expected before the first root CRL; with one held, the cloud went back (a
                // rollback) or something on the way holds newer ones back.
                Ok(Update::NotPublished)
                    if matches!(self.inner.root_crl(), RootCrl::Loaded { .. }) =>
                {
                    tracing::warn!(
                        url = %self.source.describe(),
                        "the cloud answers that it has no root CRL, but this gateway holds one; keeping it"
                    );
                }
                Ok(Update::NotPublished) => tracing::info!(
                    url = %self.source.describe(),
                    "the cloud has no root CRL to hand out yet"
                ),
                Ok(Update::Refused(reason)) => tracing::warn!(
                    url = %self.source.describe(),
                    reason = %printable(&reason, 1024),
                    "the root CRL fetched is refused; keeping the one held"
                ),
                Err(error) => tracing::warn!(
                    error = %printable(&error, 1024),
                    "the root CRL update failed; keeping the one held"
                ),
            }
            delay = if succeeded && !self.withheld() {
                retry.reset();
                let half = self.schedule.interval / 2;
                half + up_to(half, (self.jitter)())
            } else {
                retry.next_delay((self.jitter)())
            };
        }
    }

    fn withheld(&self) -> bool {
        matches!(self.inner.root_crl(), RootCrl::Withheld { .. })
    }
}

/// Logs a change of the root CRL in use.
fn log_change(before: &RootCrl, after: &RootCrl) {
    let number_of = |crl: &RootCrl| match crl {
        RootCrl::Loaded { number, .. } => number.clone().unwrap_or_else(|| "none".to_owned()),
        RootCrl::NotConfigured | RootCrl::Missing(_) | RootCrl::Withheld { .. } => {
            "none".to_owned()
        }
    };
    match after {
        RootCrl::Loaded {
            path, next_update, ..
        } => tracing::info!(
            path = %path.display(),
            number = %number_of(after),
            previous = %number_of(before),
            next_update = %next_update.map_or_else(|| "none".to_owned(), crate::identity::utc),
            "a newer root CRL is in use; the connection starts over with a full handshake checked against it"
        ),
        RootCrl::Withheld { path, number, .. } => tracing::warn!(
            path = %path.display(),
            highest = number.as_deref().unwrap_or("unknown"),
            "the root CRL file is gone or older than the highest one accepted; no connection until a newer one is fetched"
        ),
        RootCrl::NotConfigured | RootCrl::Missing(_) => {}
    }
}

#[cfg(test)]
mod tests;
