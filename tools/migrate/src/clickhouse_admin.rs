//! The bundled ClickHouse's bootstrap admin password (Laysi/trustiot-v2#403). Nobody creates,
//! copies or rotates it (docs/architecture/secrets.md):
//!
//! - the chart declares the Secret `<release>-clickhouse-admin-password` without any data, like
//!   the password Secrets of src/passwords.rs;
//! - [`generate`], run as `migrate clickhouse-admin-password` by the init container of the
//!   ClickHouse pod, fills it with the same rules as the migration Job fills its Secrets
//!   (`passwords::fill`: a 256-bit password from the operating system's CSPRNG, bound to the
//!   Secret's uid, renewed when `clickhouse.bundled.adminPasswordGeneration` goes up, kept when it
//!   goes down) and writes the password to a file on a volume in memory, which the server's
//!   entrypoint reads at start (`CLICKHOUSE_PASSWORD_FILE`). The server reads it only at start,
//!   and the init container runs before every start of the pod, so the server always starts with
//!   the password the Secret holds; a new generation changes the pod template, which restarts it;
//! - the migration Job reads the password from the Secret through the API ([`wait_for_password`])
//!   when it is about to use ClickHouse, and reads it again while ClickHouse refuses it
//!   (`clickhouse::connect`): during a rotation the Secret changes just before the server
//!   restarts with it.
//!
//! The ClickHouse pod's ServiceAccount may `get` and `patch` this one Secret and the Job's may
//! `get` it (the chart's Roles name it); only the init container mounts the pod's token. Nothing
//! here logs the password: reports and errors name the account and the Secret only.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::AdminSettings;
use crate::passwords::{self, Access, Api, Hints};
use crate::secret::Password;
use crate::{Error, Report};

/// The init container's hints: it fills the Secret with the ClickHouse pod's ServiceAccount.
const INIT_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data and the bundled ClickHouse's \
              init container fills it: run helm upgrade, which creates it again \
              (docs/runbooks/secrets.md)",
    refused: "the chart's Role lets the bundled ClickHouse's ServiceAccount get and patch exactly \
              this Secret",
};

/// The migration Job's hints: it only reads the Secret.
pub(crate) const JOB_HINTS: Hints = Hints {
    missing: "does not exist. The chart declares it without data and the bundled ClickHouse's \
              init container fills it: increase clickhouse.bundled.adminPasswordGeneration and \
              run helm upgrade, which creates it again and restarts ClickHouse so it generates a \
              new password (docs/runbooks/secrets.md)",
    refused: "the chart's Role lets the Job's ServiceAccount get the bundled ClickHouse's admin \
              Secret",
};

const RETRY_EVERY: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);

/// The init container: makes sure the Secret holds a password of the requested generation, bound
/// to it, and writes that password to `settings.password_file` (mode 0400, replaced atomically).
///
/// # Errors
///
/// When the Secret is missing or being deleted, the API refuses access or does not answer within
/// `settings.wait`, another writer keeps changing the Secret, there is no randomness, or the file
/// cannot be written. The pod then restarts the init container, and the server does not start.
pub async fn generate(settings: &AdminSettings, report: &mut Report) -> Result<(), Error> {
    let requested = passwords::parse_generation(&settings.generation).ok_or_else(|| {
        Error::Kube(format!(
            "TRUSTIOT__ADMIN__GENERATION: {:?} is not a password generation (1 to 9 decimal digits)",
            settings.generation
        ))
    })?;
    let access = Access {
        api_url: &settings.api_url,
        namespace: &settings.namespace,
        token_file: &settings.token_file,
        ca_file: &settings.ca_file,
    };
    let api = Api::new(access, settings.wait, INIT_HINTS)?;
    let account = settings.account.as_str();
    let mut filled = passwords::fill(&api, &settings.secret, &[account], requested, report).await?;
    let password = filled.remove(account).ok_or_else(|| Error::Secret {
        secret: settings.secret.clone(),
        problem: format!("no password for {account} after filling it"),
    })?;
    write_password_file(&settings.password_file, &password).map_err(|error| Error::Secret {
        secret: settings.secret.clone(),
        problem: format!(
            "cannot write the password of {account} to {}: {error}",
            settings.password_file.display()
        ),
    })?;
    tracing::info!(
        secret = %settings.secret,
        account,
        file = %settings.password_file.display(),
        "the bootstrap admin password is in place"
    );
    Ok(())
}

/// Writes `password` to `path` readable by its owner only: into a new file next to it, then
/// renamed over it, so the server never reads a half-written file.
fn write_password_file(path: &Path, password: &Password) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("the path names no file"))?;
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(".new");
    let temporary = path.with_file_name(temporary_name);
    // Left by an earlier start that stopped halfway.
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(&temporary)?;
    file.write_all(password.expose().as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// The migration Job: the password the Secret holds for `account`, once the bundled ClickHouse's
/// init container has filled it. Waits for that until `deadline`: on a fresh install the Job can
/// get here before the ClickHouse pod has started.
///
/// # Errors
///
/// When the Secret is missing, being deleted or unreadable, or holds no password filled by the
/// init container at `deadline`.
pub(crate) async fn wait_for_password(
    api: &Api,
    secret: &str,
    account: &str,
    deadline: Instant,
) -> Result<Password, Error> {
    let mut reported = None::<Instant>;
    loop {
        if let Some(password) = passwords::read_bound(api, secret, account).await? {
            return Ok(password);
        }
        if Instant::now() >= deadline {
            return Err(Error::Secret {
                secret: secret.to_owned(),
                problem: format!(
                    "holds no password of {account} that the bundled ClickHouse generated: its \
                     init container fills the Secret before the server starts, so check that the \
                     ClickHouse pod is running; when it is, the password was deleted from the \
                     Secret: increase clickhouse.bundled.adminPasswordGeneration and run helm \
                     upgrade (docs/runbooks/secrets.md)"
                ),
            });
        }
        if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
            tracing::info!(
                secret,
                account,
                "waiting for the bundled ClickHouse to generate its admin password"
            );
            reported = Some(Instant::now());
        }
        tokio::time::sleep(RETRY_EVERY).await;
    }
}
