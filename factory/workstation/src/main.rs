//! `trustiot-workstation`: the production workstation tool (factory/workstation/README.md).
//!
//! Exit codes: 0 done, 1 failed, 2 usage, `register`'s own (10 to 19 and 75,
//! [`workstation::register::StopKind`]; 20 to 23 with `--gateway`,
//! [`workstation::pipeline::StopKind`]) and `verify`'s 24 ([`workstation::NO_EVENT_EXIT_CODE`]). Results go to stdout, prompts and log lines to stderr
//! (`TRUSTIOT_WORKSTATION_LOG`: off, error, warn, info (default), debug or trace). Neither ever
//! shows the login code, the session cookie, the private key or a `--headers-file` value.

use std::ffi::OsString;
use std::io::{BufRead as _, Write as _};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::StatusCode;
use protocol::GatewayId;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use workstation::config::{self, Options, Origin};
use workstation::gateway::{self, Destination, SshGateway, UploadedAgent};
use workstation::http::{Client, ExtraHeaders, printable};
use workstation::identity::{self, KeyOrigin, WorkstationDirectory};
use workstation::journal::{Journal, State};
use workstation::pipeline::{self, Plan, Provisioned};
use workstation::register::{self, Backoff, Gateway, InputError, Registrar, StopKind, Workstation};
use workstation::secret::VerificationCode;
use workstation::session::{
    self, Accepted, LoggedOut, SendCodeError, Session, SessionStore, VerifyError,
};
use workstation::{Error, files};
use workstation::{retry, verify};
use zeroize::Zeroizing;

const USAGE: &str = "usage:
  trustiot-workstation init [--dir <dir>]
  trustiot-workstation login --url <platform URL> --email <address> [network options]
  trustiot-workstation enroll --name <workstation name> [--url <platform URL>] [--dir <dir>] \
[network options]
  trustiot-workstation install-certificate --certificate <file> [--dir <dir>]
  trustiot-workstation logout [--url <platform URL>] [network options]
  trustiot-workstation register --hardware-id <id> --gateway <ssh destination> --root-sha256 <hex> \
[--handle <hex>] [--upload-agent <file>] [--remote-sudo] [--allow-new-key] [registration options]
  trustiot-workstation register --hardware-id <id> --csr <file> --ek <file> --root-sha256 <hex> \
[registration options]
  trustiot-workstation verify --hardware-id <id> [--url <platform URL>] [--dir <dir>] \
[network options]
registration options: [--url <platform URL>] [--dir <dir>] [--attempts <n>] \
[--retry-for <seconds>] [network options]
network options: [--ca-file <file>] [--headers-file <file>] [--timeout <seconds>]
login reads the code from the email on stdin. The guide, with the exit codes, is
factory/workstation/README.md.";

/// The options every command that calls the platform takes.
const NETWORK: [&str; 3] = ["ca-file", "headers-file", "timeout"];

/// How often `login` asks for the code before giving up; the identity service allows five
/// attempts per code.
const CODE_ATTEMPTS: u32 = 3;

/// Largest certificate file `install-certificate` reads.
const MAX_CERTIFICATE_BYTES: usize = 64 * 1024;

/// Largest CSR or endorsement key file `register` reads.
const MAX_GATEWAY_FILE_BYTES: usize = 64 * 1024;

/// The log level variable.
const LOG_VARIABLE: &str = "TRUSTIOT_WORKSTATION_LOG";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let result = init_logging().and_then(|()| run(&arguments));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "trustiot-workstation: {error}");
            if matches!(error, Error::Usage(_)) {
                let _ = writeln!(stderr, "{USAGE}");
            }
            ExitCode::from(error.exit_code())
        }
    }
}

/// Log lines of this tool only, on stderr, at the level of `TRUSTIOT_WORKSTATION_LOG`.
fn init_logging() -> Result<(), Error> {
    let level = match std::env::var(LOG_VARIABLE).as_deref() {
        Err(_) | Ok("") | Ok("info") => LevelFilter::INFO,
        Ok("off") => LevelFilter::OFF,
        Ok("error") => LevelFilter::ERROR,
        Ok("warn") => LevelFilter::WARN,
        Ok("debug") => LevelFilter::DEBUG,
        Ok("trace") => LevelFilter::TRACE,
        Ok(_) => {
            return Err(Error::Usage(format!(
                "{LOG_VARIABLE} must be off, error, warn, info, debug or trace"
            )));
        }
    };
    let targets = Targets::new()
        .with_target("workstation", level)
        .with_target("trustiot_workstation", level);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false),
        )
        .with(targets)
        .try_init()
        .map_err(|_| Error::Failed("cannot set up logging".to_owned()))
}

/// A variable of the process environment, for the `config` functions that locate directories.
fn environment_variable(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// Writes a result line to stdout.
fn say(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{text}");
}

/// Writes a prompt or a notice to stderr.
fn tell(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{text}");
}

fn allowed<'a>(own: &[&'a str], network: bool) -> Vec<&'a str> {
    let mut names = own.to_vec();
    if network {
        names.extend(NETWORK);
    }
    names
}

fn run(arguments: &[String]) -> Result<(), Error> {
    let Some((command, rest)) = arguments.split_first() else {
        return Err(Error::Usage("a command is required".to_owned()));
    };
    match command.as_str() {
        "init" => init(&Options::parse(rest, &["dir"])?),
        "install-certificate" => install(&Options::parse(rest, &["certificate", "dir"])?),
        "login" => network(login(&Options::parse(
            rest,
            &allowed(&["url", "email"], true),
        )?)),
        "enroll" => network(enroll(&Options::parse(
            rest,
            &allowed(&["name", "url", "dir"], true),
        )?)),
        "logout" => network(logout(&Options::parse(rest, &allowed(&["url"], true))?)),
        "register" => network(register(&Options::parse_with_flags(
            rest,
            &allowed(
                &[
                    "hardware-id",
                    "csr",
                    "ek",
                    "gateway",
                    "handle",
                    "upload-agent",
                    "url",
                    "dir",
                    "root-sha256",
                    "attempts",
                    "retry-for",
                ],
                true,
            ),
            &["remote-sudo", "allow-new-key"],
        )?)),
        "verify" => network(verify(&Options::parse(
            rest,
            &allowed(&["hardware-id", "url", "dir"], true),
        )?)),
        "help" | "--help" | "-h" => {
            say(USAGE);
            Ok(())
        }
        other => Err(Error::Usage(format!("unknown command {other:?}"))),
    }
}

/// Runs a command that calls the platform on a single-threaded runtime.
fn network(work: impl Future<Output = Result<(), Error>>) -> Result<(), Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| Error::Failed(format!("cannot start the runtime: {error}")))?
        .block_on(work)
}

fn directory(options: &Options) -> Result<WorkstationDirectory, Error> {
    config::workstation_directory(options.optional("dir"), &environment_variable)
        .map(WorkstationDirectory::new)
}

fn init(options: &Options) -> Result<(), Error> {
    let directory = directory(options)?;
    let origin = identity::init(&directory)?;
    match origin {
        KeyOrigin::Created => say(&format!(
            "created the workstation key {}",
            directory.key().display()
        )),
        KeyOrigin::Existing => say(&format!(
            "kept the existing workstation key {}",
            directory.key().display()
        )),
    }
    say(&format!(
        "wrote the certificate signing request {}",
        directory.csr().display()
    ));
    Ok(())
}

fn install(options: &Options) -> Result<(), Error> {
    let directory = directory(options)?;
    let path = config::path_option(options, "certificate")?
        .ok_or_else(|| Error::Usage("--certificate is required".to_owned()))?;
    let pem = files::read_public(path, MAX_CERTIFICATE_BYTES)?;
    let installed = identity::install_certificate(&directory, &pem, now())?;
    say(&format!(
        "installed {} as {} (serial {}, valid until {})",
        installed.subject,
        directory.certificate().display(),
        installed.serial,
        installed.not_after.to_rfc3339()
    ));
    Ok(())
}

/// The client for `origin` with the network options.
fn client(options: &Options, origin: Origin) -> Result<Client, Error> {
    let headers = match config::path_option(options, "headers-file")? {
        Some(path) => ExtraHeaders::load(path)?,
        None => ExtraHeaders::none(),
    };
    Client::new(
        origin,
        config::path_option(options, "ca-file")?,
        headers,
        options.timeout()?,
    )
}

fn url_option(options: &Options) -> Result<Option<Origin>, Error> {
    options.optional("url").map(Origin::parse).transpose()
}

async fn login(options: &Options) -> Result<(), Error> {
    let origin = Origin::parse(options.required("url")?)?;
    let email = session::check_email(options.required("email")?)?;
    // Everything that can fail locally fails before a code is mailed.
    let store = SessionStore::new(config::session_directory(&environment_variable)?);
    store.prepare()?;
    // The workstation keeps one session. A session of another platform is not replaced: the
    // file is the only place its cookie lives, so overwriting it would leave that session active
    // and out of reach until it expires.
    let previous = match store.load()? {
        Some(session) if session.origin != origin => {
            return Err(Error::Failed(format!(
                "this workstation holds a session for {}: run logout first, then log in to \
                 {origin}",
                session.origin
            )));
        }
        stored => stored.map(|session| session.cookie),
    };
    let client = client(options, origin.clone())?;

    let accepted = session::send_code(&client, email)
        .await
        .map_err(|error| match error {
            SendCodeError::Failed(error) => error,
            unknown @ SendCodeError::Unknown(_) => Error::Failed(unknown.to_string()),
        })?;
    match accepted {
        Accepted::Answer(sent) => tell(&format!(
            "If the address belongs to an account, a login code of {} digits is on its way; it is \
             valid for {} minutes. If none arrives, wait at least {} seconds and run login again \
             (each address gets at most 5 codes an hour and 20 a day).",
            sent.code_length,
            sent.expires_in_secs / 60,
            sent.resend_after_secs
        )),
        Accepted::Unreadable => tell(
            "The platform accepted the request but its answer could not be read. If the address \
             belongs to an account, a login code is on its way all the same. If none arrives, \
             wait at least 60 seconds and run login again (each address gets at most 5 codes an \
             hour and 20 a day).",
        ),
    }
    let stdin = std::io::stdin();
    for attempt in 1..=CODE_ATTEMPTS {
        tell(&format!(
            "Enter the code from the email (attempt {attempt} of {CODE_ATTEMPTS}):"
        ));
        let mut line = Zeroizing::new(String::with_capacity(256));
        let read = stdin
            .lock()
            .read_line(&mut line)
            .map_err(|error| Error::Failed(format!("cannot read the code from stdin: {error}")))?;
        if read == 0 {
            return Err(Error::Failed(
                "no code on stdin: run login again and enter the code from the newest email"
                    .to_owned(),
            ));
        }
        let Some(code) = VerificationCode::parse(&line) else {
            tell("A login code is 8 digits.");
            continue;
        };
        match session::verify_code(&client, email, &code, previous.as_ref()).await {
            Ok(cookie) => {
                let session = Session {
                    origin: origin.clone(),
                    cookie,
                };
                if let Err(error) = store.save(&session) {
                    // The cookie now lives only in this process: revoke the session rather than
                    // leave it active with no way to reach it.
                    let revoked = match session::logout(&client, &session).await {
                        Ok(_) => "the new session was revoked",
                        Err(_) => {
                            "the new session could not be revoked and stays active until 12 \
                             hours without use"
                        }
                    };
                    return Err(Error::Failed(format!("{error}; {revoked}")));
                }
                say(&format!(
                    "logged in to {origin}; the session is kept in {} until logout, or until it \
                     ends (12 hours without use, 7 days at most)",
                    store.path().display()
                ));
                return Ok(());
            }
            Err(VerifyError::Refused) => tell(&VerifyError::Refused.to_string()),
            Err(VerifyError::Failed(error)) => return Err(error),
        }
    }
    Err(Error::Failed(
        "no code was accepted: run login again for a new code".to_owned(),
    ))
}

/// The stored session for `--url` (or its own origin), and a client for that origin.
fn session_client(options: &Options, store: &SessionStore) -> Result<(Session, Client), Error> {
    let session = session::session_for(store, url_option(options)?.as_ref())?;
    let client = client(options, session.origin.clone())?;
    Ok((session, client))
}

/// After `error` from a call made with `session`: when the platform rejected the session (its
/// own 401), revokes it and removes the file only once the identity service confirms it is
/// gone. The file is the only copy of the cookie, so on anything else it is kept and the
/// operator is told to run logout.
async fn forget_ended_session(
    client: &Client,
    store: &SessionStore,
    session: &Session,
    error: Error,
) -> Error {
    if !session::is_unauthenticated(&error) {
        if matches!(&error, Error::Api(api) if api.status == StatusCode::UNAUTHORIZED) {
            return Error::Failed(format!(
                "{error}. That is not the platform's own answer (a proxy in front of it?), so \
                 the session file is kept"
            ));
        }
        return error;
    }
    match session::logout(client, session).await {
        Ok(_) => match store.remove() {
            Ok(_) => Error::Failed(format!(
                "{error}: the session has ended or was revoked and its file is removed; log in \
                 again"
            )),
            Err(remove) => Error::Failed(format!("{error}; and the session file: {remove}")),
        },
        Err(logout) => Error::Failed(format!(
            "{error}. The session could not be confirmed as ended ({logout}), so its file is \
             kept: run logout, then login"
        )),
    }
}

async fn enroll(options: &Options) -> Result<(), Error> {
    let name = options.required("name")?;
    let directory = directory(options)?;
    let store = SessionStore::new(config::session_directory(&environment_variable)?);
    let (session, client) = session_client(options, &store)?;
    let enrolled = match identity::enroll(&client, &session.cookie, &directory, name, now).await {
        Ok(enrolled) => enrolled,
        Err(error) => return Err(forget_ended_session(&client, &store, &session, error).await),
    };
    say(&format!(
        "installed {} as {} (serial {}, valid until {}); keep the serial, it is how the \
         certificate is revoked",
        enrolled.installed.subject,
        directory.certificate().display(),
        enrolled.installed.serial,
        enrolled.installed.not_after.to_rfc3339()
    ));
    Ok(())
}

/// The retry limits of `register`: the defaults, with `--attempts` and `--retry-for`.
fn backoff(options: &Options) -> Result<Backoff, Error> {
    let mut backoff = Backoff::default();
    if let Some(text) = options.optional("attempts") {
        backoff.max_attempts = text
            .parse::<u32>()
            .ok()
            .filter(|attempts| (1..=retry::MAX_ATTEMPTS).contains(attempts))
            .ok_or_else(|| {
                Error::Usage(format!(
                    "--attempts must be a whole number from 1 to {}",
                    retry::MAX_ATTEMPTS
                ))
            })?;
    }
    if let Some(text) = options.optional("retry-for") {
        backoff.max_total = text
            .parse::<u64>()
            .ok()
            .filter(|seconds| (1..=retry::MAX_TOTAL_SECS).contains(seconds))
            .map(Duration::from_secs)
            .ok_or_else(|| {
                Error::Usage(format!(
                    "--retry-for must be a whole number of seconds from 1 to {}",
                    retry::MAX_TOTAL_SECS
                ))
            })?;
    }
    Ok(backoff)
}

/// Reads a CSR or endorsement key file named by `--<name>`.
fn gateway_file(options: &Options, name: &str) -> Result<(Vec<u8>, std::path::PathBuf), Error> {
    let path = config::path_option(options, name)?
        .ok_or_else(|| Error::Usage(format!("--{name} is required")))?;
    Ok((
        files::read_public(path, MAX_GATEWAY_FILE_BYTES)?,
        path.to_owned(),
    ))
}

/// The hardware ID of `--hardware-id`.
fn hardware_id(options: &Options) -> Result<GatewayId, Error> {
    let text = options.required("hardware-id")?;
    let invalid = || Error::from(InputError::HardwareId(format!("{:?}", printable(text))));
    if !register::is_hardware_id(text) {
        return Err(invalid());
    }
    GatewayId::new(text).map_err(|_| invalid())
}

/// Where the gateway's CSR and endorsement key come from.
enum Source {
    /// `--csr` and `--ek`.
    Files(Gateway),
    /// `--gateway`: `trustiot-agent provision` over ssh.
    Ssh { ssh: SshGateway, plan: Plan },
}

/// The options only `register --gateway` takes.
const SSH_ONLY: [&str; 4] = ["handle", "upload-agent", "remote-sudo", "allow-new-key"];

fn register_source(options: &Options) -> Result<Source, Error> {
    let Some(destination) = options.optional("gateway") else {
        if let Some(name) = SSH_ONLY.iter().find(|name| options.given(name)) {
            return Err(Error::Usage(format!("--{name} needs --gateway")));
        }
        let hardware_id = options.required("hardware-id")?;
        let (request, request_path) = gateway_file(options, "csr")?;
        let (endorsement_key, endorsement_path) = gateway_file(options, "ek")?;
        let gateway =
            Gateway::new(hardware_id, &request, &endorsement_key).map_err(|error| match error {
                InputError::CertificateRequest(reason) => Error::BadFile {
                    path: request_path.clone(),
                    reason,
                },
                InputError::EndorsementKey(reason) => Error::BadFile {
                    path: endorsement_path.clone(),
                    reason,
                },
                other @ InputError::HardwareId(_) => other.into(),
            })?;
        return Ok(Source::Files(gateway));
    };
    if let Some(name) = ["csr", "ek"].iter().find(|name| options.given(name)) {
        return Err(Error::Usage(format!(
            "--{name} is for files; with --gateway the CSR and the endorsement key come from the \
             gateway"
        )));
    }
    // Every value that reaches the remote command is checked here, before anything runs.
    let plan = Plan {
        hardware_id: hardware_id(options)?,
        handle: gateway::parse_handle(options.optional("handle"))?,
        allow_new_key: options.flag("allow-new-key"),
    };
    let destination = Destination::parse(destination)?;
    let upload = config::path_option(options, "upload-agent")?
        .map(UploadedAgent::load)
        .transpose()?;
    Ok(Source::Ssh {
        ssh: SshGateway::new(destination, upload, options.flag("remote-sudo")),
        plan,
    })
}

async fn register(options: &Options) -> Result<(), Error> {
    let source = register_source(options)?;
    // Required, with no alternative: the root certificate of the answer becomes the gateway's
    // only trust anchor, so it is checked against the fingerprint the operator was given.
    let root_sha256 = register::parse_root_sha256(options.required("root-sha256")?)?;
    let backoff = backoff(options)?;
    let directory = directory(options)?;
    let workstation = Workstation::load(&directory, now())?;
    let store = SessionStore::new(config::session_directory(&environment_variable)?);
    let (session, client) = session_client(options, &store)?;
    let journal = Journal::new(directory.path());
    let registrar = Registrar {
        client: &client,
        cookie: &session.cookie,
        workstation: &workstation,
        journal: &journal,
        backoff,
        root_sha256,
    };
    let result = match &source {
        Source::Files(gateway) => register::register(&registrar, gateway, now).await,
        Source::Ssh { ssh, plan } => pipeline::register_gateway(&registrar, ssh, plan, now)
            .await
            .map(|outcome| {
                report_provisioned(ssh, plan, &outcome.provisioned);
                outcome.registered
            }),
    };
    let registered = match result {
        Ok(registered) => registered,
        Err(Error::Registration(mut stopped)) if stopped.kind == StopKind::LoginRequired => {
            // As for enroll: the platform's own 401 revokes and forgets the session; the exit
            // code stays the registration's.
            if let Some(api) = stopped.api.clone() {
                let advice = forget_ended_session(&client, &store, &session, api.into()).await;
                stopped.message = format!("{advice}; then run the same register command again");
            }
            return Err(Error::Registration(stopped));
        }
        Err(error) => return Err(error),
    };
    // The registration passed its checks: its hardware ID is the one sent.
    let registration = &registered.registration;
    let record = journal.record_path(&registration.hardware_id);
    say(&format!(
        "{} hardware ID {} (serial {}, valid until {}) after {} attempt(s); recorded in {}",
        if registered.status == StatusCode::CREATED {
            "registered"
        } else {
            "the platform already had the registration of"
        },
        registration.hardware_id,
        registration.serial,
        registration.not_after.to_rfc3339(),
        registered.attempts,
        record.display()
    ));
    Ok(())
}

/// What the gateway reported, before the registration's own line.
fn report_provisioned(ssh: &SshGateway, plan: &Plan, provisioned: &Provisioned) {
    say(&format!(
        "{}: the TPM key at {} was {}; public key SHA-256 {}",
        ssh.destination(),
        plan.handle,
        if provisioned.key_created {
            "created"
        } else {
            "already there and reused"
        },
        provisioned.gateway.public_key_sha256()
    ));
    if !provisioned.ek_certificate {
        tell(&format!(
            "note: the gateway's TPM has no EK certificate. {}",
            pipeline::EK_CERTIFICATE_POLICY
        ));
    }
}

async fn verify(options: &Options) -> Result<(), Error> {
    let hardware_id = hardware_id(options)?;
    let directory = directory(options)?;
    let journal = Journal::new(directory.path());
    let record = journal
        .load(hardware_id.as_str())?
        .filter(|record| record.state == State::Registered);
    let recorded = record
        .as_ref()
        .and_then(|record| record.registration.as_ref());
    let from = verify::search_from(recorded.map(|registration| registration.registered_at));
    let store = SessionStore::new(config::session_directory(&environment_variable)?);
    let (session, client) = session_client(options, &store)?;
    let search = match verify::find(&client, &session.cookie, &hardware_id, from).await {
        Ok(search) => search,
        Err(error) => return Err(forget_ended_session(&client, &store, &session, error).await),
    };
    if search.found.is_empty() {
        let scope = match (from, search.complete) {
            (_, false) => format!(
                "in the newest {} events of the platform's event log (the search stops there)",
                search.events_read
            ),
            (Some(from), true) => format!(
                "in the platform's event log since {} ({} events)",
                from.to_rfc3339(),
                search.events_read
            ),
            (None, true) => format!(
                "in the platform's event log ({} events)",
                search.events_read
            ),
        };
        return Err(Error::NoEvent(format!(
            "no {} event of hardware ID {hardware_id} {scope}. The log records an event a few \
             seconds after the registration: wait and run verify again. If it stays missing, run \
             the same register command again (it is safe) and report it to a platform \
             administrator",
            verify::EVENT_TYPE
        )));
    }
    for found in &search.found {
        let event = &found.registered;
        let serial = found.serial();
        say(&format!(
            "{} at {}: hardware ID {hardware_id}, serial {serial}, registered by {}, public key \
             SHA-256 {} (event {})",
            verify::EVENT_TYPE,
            found.occurred_at.to_rfc3339(),
            printable(&event.workstation),
            printable(&event.public_key_sha256),
            found.event_id
        ));
        match (&record, recorded) {
            (Some(record), Some(registration)) => {
                if registration.serial == event.certificate_serial
                    && record.public_key_sha256 == event.public_key_sha256
                {
                    say("  it matches this workstation's record (serial and public key)");
                } else {
                    tell(&format!(
                        "  it does not match this workstation's record ({}): another \
                         registration of the hardware ID; report it to a platform administrator",
                        journal.record_path(hardware_id.as_str()).display()
                    ));
                }
            }
            _ => say("  this workstation has no registered record of the hardware ID"),
        }
    }
    Ok(())
}

async fn logout(options: &Options) -> Result<(), Error> {
    let store = SessionStore::new(config::session_directory(&environment_variable)?);
    if store.load()?.is_none() {
        say("no session on this workstation: nothing to log out");
        return Ok(());
    }
    let (session, client) = session_client(options, &store)?;
    let outcome = session::logout(&client, &session).await?;
    store.remove()?;
    say(match outcome {
        LoggedOut::Revoked => "logged out: the session is revoked and its file removed",
        LoggedOut::AlreadyEnded => "the session had already ended; its file is removed",
    });
    Ok(())
}
