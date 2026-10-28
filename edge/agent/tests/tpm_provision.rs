//! `agent::tpm::ensure_key` against a real TPM 2.0 implementation: swtpm (libtpms) in a
//! throwaway container (`testkit::swtpm`), inspected with tpm2-tools (Laysi/trustiot-v2#611).
//!
//! - an empty handle gets a key with the attributes of decision 12 (tpm2-tools reads them back);
//!   a rerun in the same session and in a new connection reuses it: same public key, same
//!   persistent handles, nothing left loaded;
//! - without the standard SRK at 0x81000001 a transient primary is made and flushed, and
//!   0x81000001 is left as it was;
//! - objects that are not the key (RSA, restricted, P-384, a storage key, a key with a password,
//!   a key that cannot sign) are refused after TPM2_ReadPublic and at most a test signature, and
//!   the TPM is exactly as before;
//! - two processes creating at once end with one key that both use;
//! - a process that dies before TPM2_Load or TPM2_EvictControl leaves nothing persisted, and the
//!   next run creates the key;
//! - after `tpm2_clear` a rerun creates a different key.
//!
//! Each [`Process`] is one `testkit::swtpm::Session`: like a process with `/dev/tpmrm0` open, it
//! flushes what it loaded when it ends, as the kernel's resource manager does. Without podman or
//! docker the tests skip, unless `CI` is set, where they fail.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers unwrap like the tests themselves (clippy.toml allows it in tests)"
)]

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agent::tpm::{
    DEFAULT_KEY_HANDLE, ProvisionError, Provisioned, SRK_HANDLE, TpmChannel, TpmError, TpmKey,
    TpmTransport, ensure_key,
};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use testkit::swtpm::{Session, Swtpm};

const KEY: u32 = DEFAULT_KEY_HANDLE;

/// How long a test waits for something that happens within milliseconds unless the code is
/// wrong: the TPM thread closing its session, the other process reaching TPM2_EvictControl.
const WAIT: Duration = Duration::from_secs(60);

/// TPM command codes the tests look for.
const EVICT_CONTROL: u32 = 0x120;
const CREATE_PRIMARY: u32 = 0x131;
const CREATE: u32 = 0x153;
const LOAD: u32 = 0x157;
const SIGN: u32 = 0x15d;
const FLUSH_CONTEXT: u32 = 0x165;
const READ_PUBLIC: u32 = 0x173;

/// Lets two processes reach TPM2_EvictControl together, so both have created and loaded a key
/// before either persists it.
struct Rendezvous {
    arrived: Mutex<usize>,
    all_here: Condvar,
    parties: usize,
}

impl Rendezvous {
    fn new(parties: usize) -> Arc<Self> {
        Arc::new(Self {
            arrived: Mutex::new(0),
            all_here: Condvar::new(),
            parties,
        })
    }

    fn arrive(&self) {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        self.all_here.notify_all();
        let (_arrived, waited) = self
            .all_here
            .wait_timeout_while(arrived, WAIT, |arrived| *arrived < self.parties)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "the other process did not reach TPM2_EvictControl within {WAIT:?}"
        );
    }
}

/// What a test sets up for one process.
#[derive(Default)]
struct Options {
    /// Wait here before sending TPM2_EvictControl.
    before_evict: Option<Arc<Rendezvous>>,
    /// The command at which the process dies: that command and every later one fail, without
    /// reaching the TPM.
    dies_at: Option<u32>,
}

/// One process using the TPM through its own session.
struct Process {
    session: Session,
    options: Options,
    dead: bool,
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Dropped after the session, when the TPM thread has let go of the channel.
    _closed: mpsc::Sender<()>,
}

impl TpmChannel for Process {
    fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
        let code = u32::from_be_bytes(command[6..10].try_into().unwrap());
        self.commands.lock().unwrap().push(command.to_vec());
        if self.options.dies_at == Some(code) {
            self.dead = true;
        }
        if self.dead {
            return Err(io::Error::other("the process has died"));
        }
        if code == EVICT_CONTROL
            && let Some(rendezvous) = &self.options.before_evict
        {
            rendezvous.arrive();
        }
        self.session.transact(command)
    }
}

/// What a test can see of a process after it ran.
struct Watch {
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    closed: mpsc::Receiver<()>,
}

impl Watch {
    /// The command codes the process sent, with resends after a TPM warning (the same command
    /// again) counted once.
    fn codes(&self) -> Vec<u32> {
        let mut commands = self.commands.lock().unwrap().clone();
        commands.dedup();
        commands
            .iter()
            .map(|command| u32::from_be_bytes(command[6..10].try_into().unwrap()))
            .collect()
    }

    /// Waits until the process's session is closed (its transient objects flushed), which
    /// happens on the TPM thread after the last key is dropped.
    fn closed(&self) {
        match self.closed.recv_timeout(WAIT) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("the TPM thread did not close its session within {WAIT:?}: {other:?}"),
        }
    }
}

fn process(swtpm: &Swtpm, options: Options) -> (Process, Watch) {
    let commands = Arc::new(Mutex::new(Vec::new()));
    let (closed_sender, closed) = mpsc::channel();
    (
        Process {
            session: swtpm.session(),
            options,
            dead: false,
            commands: Arc::clone(&commands),
            _closed: closed_sender,
        },
        Watch { commands, closed },
    )
}

fn provision_at(
    swtpm: &Swtpm,
    handle: u32,
    options: Options,
) -> (Result<Provisioned, ProvisionError>, Watch) {
    let (process, watch) = process(swtpm, options);
    (
        ensure_key(TpmTransport::Channel(Box::new(process)), handle),
        watch,
    )
}

fn provision(swtpm: &Swtpm) -> (Provisioned, Watch) {
    let (result, watch) = provision_at(swtpm, KEY, Options::default());
    (result.unwrap(), watch)
}

/// Runs `provisioned`'s key to the end of its process.
fn finish(provisioned: Provisioned, watch: &Watch) -> Vec<u8> {
    let spki = provisioned.key.public_key_spki_der().to_vec();
    drop(provisioned);
    watch.closed();
    spki
}

fn hex(text: &str) -> Vec<u8> {
    text.trim()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

/// The SubjectPublicKeyInfo of the object at `handle`, as tpm2-tools computes it.
fn tools_spki(swtpm: &Swtpm, handle: u32) -> Vec<u8> {
    hex(&swtpm.tools(&format!(
        "tpm2_readpublic -Q -c {handle:#010x} -f der -o spki.der\nod -An -v -tx1 spki.der | tr -d ' \\n'"
    )))
}

fn verifies(spki: &[u8], message: &[u8], signature: &[u8]) -> bool {
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &spki[26..])
        .verify(message, signature)
        .is_ok()
}

#[test]
fn an_empty_handle_gets_the_decision_12_key_and_reruns_reuse_it() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE]);

    let (first, watch) = provision(&swtpm);
    assert!(first.created);
    assert_eq!(first.key.handle(), KEY);
    // Under the standard SRK, so no primary; the loaded key flushed by ensure_key itself (the
    // session is still open, so its own flush has not run).
    assert_eq!(
        watch.codes(),
        [
            READ_PUBLIC,
            READ_PUBLIC,
            CREATE,
            LOAD,
            EVICT_CONTROL,
            FLUSH_CONTEXT,
            READ_PUBLIC,
            SIGN
        ]
    );
    assert!(swtpm.transient_handles().is_empty());
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, KEY]);
    // tpm2-tools sees decision 12's key.
    let public = swtpm.tools(&format!("tpm2_readpublic -c {KEY:#010x}"));
    for expected in [
        "value: fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign\n  raw: 0x40072",
        "type:\n  value: ecc",
        "curve-id:\n  value: NIST p256",
        "scheme:\n  value: ecdsa",
        "scheme-halg:\n  value: sha256",
        "sym-alg:\n  value: null",
    ] {
        assert!(public.contains(expected), "{expected:?} in\n{public}");
    }
    assert_eq!(
        first.key.public_key_spki_der(),
        &tools_spki(&swtpm, KEY)[..]
    );
    let signature = first.key.sign(b"provisioned").unwrap();
    assert!(verifies(
        first.key.public_key_spki_der(),
        b"provisioned",
        &signature
    ));
    let spki = finish(first, &watch);

    // A rerun on the same connection, then on a new one: the same key, read and test-signed.
    for new_connection in [false, true] {
        if new_connection {
            swtpm.disconnect();
        }
        let (again, watch) = provision(&swtpm);
        assert!(!again.created);
        assert_eq!(watch.codes(), [READ_PUBLIC, SIGN]);
        assert_eq!(finish(again, &watch), spki);
        assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, KEY]);
        assert!(swtpm.transient_handles().is_empty());
    }

    // The agent opens it as it does at every start.
    let (process, watch) = process(&swtpm, Options::default());
    let key = TpmKey::open(TpmTransport::Channel(Box::new(process)), KEY).unwrap();
    assert_eq!(key.public_key_spki_der(), &spki[..]);
    drop(key);
    watch.closed();
}

#[test]
fn a_non_standard_srk_is_left_alone_and_a_transient_primary_is_used() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    // An ECC storage key at 0x81000001 with noDA: not the tpm2-tools default template.
    let _ = swtpm.tools(&format!(
        "tpm2_createprimary -Q -C o -g sha256 -G ecc256:aes128cfb \
         -a 'fixedtpm|fixedparent|sensitivedataorigin|userwithauth|restricted|decrypt|noda' -c srk.ctx\n\
         tpm2_evictcontrol -Q -C o -c srk.ctx {SRK_HANDLE:#010x}\n\
         tpm2_flushcontext -t"
    ));
    let srk_before = swtpm.tools(&format!("tpm2_readpublic -c {SRK_HANDLE:#010x}"));

    let (provisioned, watch) = provision(&swtpm);
    assert!(provisioned.created);
    let codes = watch.codes();
    assert_eq!(
        codes,
        [
            READ_PUBLIC,
            READ_PUBLIC,
            CREATE_PRIMARY,
            CREATE,
            LOAD,
            FLUSH_CONTEXT,
            EVICT_CONTROL,
            FLUSH_CONTEXT,
            READ_PUBLIC,
            SIGN
        ]
    );
    // Both transient objects flushed by ensure_key, the SRK not persisted or changed.
    assert!(swtpm.transient_handles().is_empty());
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, KEY]);
    assert_eq!(
        swtpm.tools(&format!("tpm2_readpublic -c {SRK_HANDLE:#010x}")),
        srk_before
    );
    assert_eq!(
        provisioned.key.public_key_spki_der(),
        &tools_spki(&swtpm, KEY)[..]
    );
    finish(provisioned, &watch);
}

/// Objects at handles next to the key's, made with tpm2-tools under the standard SRK.
const FOREIGN_OBJECTS: &str = r#"object() {
    handle="$1"; shift
    tpm2_create -Q -C 0x81000001 "$@" -u object.pub -r object.priv
    tpm2_load -Q -C 0x81000001 -u object.pub -r object.priv -c object.ctx
    tpm2_evictcontrol -Q -C o -c object.ctx "$handle"
    tpm2_flushcontext -t
}
signing="fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign"
password() { od -An -N16 -tx1 /dev/urandom | tr -d ' \n'; }
object 0x81000101 -G rsa2048:rsassa-sha256:null -a "$signing"
object 0x81000102 -G ecc256:ecdsa-sha256:null -a "$signing|restricted"
object 0x81000103 -G ecc384:ecdsa-sha384:null -a "$signing"
object 0x81000104 -G ecc256:ecdsa-sha256:null -a "$signing|noda" -p "$(password)"
object 0x81000105 -G ecc256:null:null -a "fixedtpm|fixedparent|sensitivedataorigin|userwithauth|decrypt"
object 0x81000106 -G ecc256:null:aes128cfb -a "fixedtpm|fixedparent|sensitivedataorigin|userwithauth|restricted|decrypt"
object 0x81000107 -G ecc256:ecdsa-sha256:null -a "$signing" -p "$(password)"
"#;

/// The line of `Swtpm::snapshot` with the dictionary-attack counter.
const LOCKOUT_COUNTER: &str = "TPM2_PT_LOCKOUT_COUNTER:";

#[test]
fn objects_that_do_not_match_are_refused_and_the_tpm_is_unchanged() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    let _ = swtpm.tools(FOREIGN_OBJECTS);
    let before = swtpm.snapshot();

    let refuse = |handle: u32| -> (String, Vec<u32>) {
        let (result, watch) = provision_at(&swtpm, handle, Options::default());
        let reason = match result {
            Err(ProvisionError::Refused {
                handle: refused,
                reason,
            }) => {
                assert_eq!(refused, handle);
                reason.to_string()
            }
            other => panic!("{handle:#010x} should be refused, got {other:?}"),
        };
        watch.closed();
        (reason, watch.codes())
    };
    for (handle, expected, codes) in [
        (0x8100_0101, "not an ECC key", &[READ_PUBLIC][..]),
        (0x8100_0102, "have [restricted]", &[READ_PUBLIC]),
        (0x8100_0103, "with hash 0x000c", &[READ_PUBLIC]),
        // Its template is right; the test signature with the empty password fails.
        (0x8100_0104, "TPM_RC_BAD_AUTH", &[READ_PUBLIC, SIGN]),
        (0x8100_0105, "hash none", &[READ_PUBLIC]),
        (0x8100_0106, "symmetric", &[READ_PUBLIC]),
    ] {
        let (reason, sent) = refuse(handle);
        assert!(reason.contains(expected), "{handle:#010x}: {reason}");
        // Read, at most a test signature, nothing else.
        assert_eq!(sent, codes, "{handle:#010x}");
    }
    assert_eq!(swtpm.snapshot(), before, "the TPM changed");

    // A password key without noDA: the failed test signature counts once against the
    // dictionary-attack counter, and nothing else changes.
    let (reason, sent) = refuse(0x8100_0107);
    assert!(reason.contains("TPM_RC_AUTH_FAIL"), "{reason}");
    assert_eq!(sent, [READ_PUBLIC, SIGN]);
    let after = swtpm.snapshot();
    let without_counter = |snapshot: &str| -> Vec<String> {
        snapshot
            .lines()
            .filter(|line| !line.starts_with(LOCKOUT_COUNTER))
            .map(str::to_owned)
            .collect()
    };
    let counter = |snapshot: &str| -> String {
        snapshot
            .lines()
            .find(|line| line.starts_with(LOCKOUT_COUNTER))
            .unwrap()
            .to_owned()
    };
    assert_eq!(without_counter(&after), without_counter(&before));
    assert_eq!(counter(&before), format!("{LOCKOUT_COUNTER} 0x0"));
    assert_eq!(counter(&after), format!("{LOCKOUT_COUNTER} 0x1"));
}

#[test]
fn two_processes_creating_at_once_end_with_one_key() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    let rendezvous = Rendezvous::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let run = || {
            provision_at(
                &swtpm,
                KEY,
                Options {
                    before_evict: Some(Arc::clone(&rendezvous)),
                    dies_at: None,
                },
            )
        };
        let first = scope.spawn(run);
        let second = scope.spawn(run);
        (first.join().unwrap(), second.join().unwrap())
    });
    let ((first, first_watch), (second, second_watch)) = (first, second);
    let (first, second) = (first.unwrap(), second.unwrap());
    // Both created and loaded a key; one persisted it, the other got TPM_RC_NV_DEFINED and
    // took that one.
    for watch in [&first_watch, &second_watch] {
        assert_eq!(
            watch.codes()[..6],
            [
                READ_PUBLIC,
                READ_PUBLIC,
                CREATE,
                LOAD,
                EVICT_CONTROL,
                FLUSH_CONTEXT
            ]
        );
    }
    assert_ne!(first.created, second.created, "exactly one run created it");
    assert_eq!(
        first.key.public_key_spki_der(),
        second.key.public_key_spki_der()
    );
    assert_eq!(
        first.key.public_key_spki_der(),
        &tools_spki(&swtpm, KEY)[..]
    );
    assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, KEY]);
    // Each flushed its own loaded key.
    assert!(swtpm.transient_handles().is_empty());
    finish(first, &first_watch);
    finish(second, &second_watch);
}

#[test]
fn a_process_that_dies_before_the_key_is_persisted_leaves_nothing_and_a_rerun_creates_it() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    for (handle, dies_at) in [(KEY, LOAD), (KEY + 1, EVICT_CONTROL)] {
        let (result, watch) = provision_at(
            &swtpm,
            handle,
            Options {
                before_evict: None,
                dies_at: Some(dies_at),
            },
        );
        assert!(
            matches!(result, Err(ProvisionError::Tpm(TpmError::Io(_)))),
            "{result:?}"
        );
        // The session ends as the process's /dev/tpmrm0 would: what it loaded is flushed.
        watch.closed();
        assert!(watch.codes().contains(&dies_at));
        assert!(swtpm.transient_handles().is_empty());
        assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE]);

        let (rerun, watch) = provision_at(&swtpm, handle, Options::default());
        let rerun = rerun.unwrap();
        assert!(rerun.created);
        finish(rerun, &watch);
        assert_eq!(swtpm.persistent_handles(), [SRK_HANDLE, handle]);
        // Remove this run's key so the next case starts from the same state.
        let _ = swtpm.tools(&format!("tpm2_evictcontrol -Q -C o -c {handle:#010x}"));
    }
}

#[test]
fn after_tpm2_clear_a_rerun_creates_a_different_key() {
    let Some(swtpm) = Swtpm::start() else {
        return;
    };
    swtpm.create_standard_srk();
    let (before, watch) = provision(&swtpm);
    assert!(before.created);
    let before = finish(before, &watch);

    // A cleared TPM: the owner hierarchy's objects are gone, the SRK with them, and the storage
    // seed is new. Decision 12 treats it as a new device.
    let _ = swtpm.tools("tpm2_clear");
    assert!(swtpm.persistent_handles().is_empty());

    let (after, watch) = provision(&swtpm);
    assert!(after.created);
    // No SRK any more: a transient primary, not persisted.
    assert!(watch.codes().contains(&CREATE_PRIMARY));
    let after_spki = after.key.public_key_spki_der().to_vec();
    assert_ne!(after_spki, before, "a cleared TPM must give a new key");
    assert_eq!(after_spki, tools_spki(&swtpm, KEY));
    assert_eq!(swtpm.persistent_handles(), [KEY]);
    assert!(swtpm.transient_handles().is_empty());
    finish(after, &watch);
}
