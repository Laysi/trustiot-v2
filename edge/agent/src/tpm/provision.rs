//! Provisioning the gateway key (decision 12, Laysi/trustiot-v2#611): [`ensure_key`] makes sure
//! the persistent handle holds the key the agent uses, for the factory tool's `provision`
//! subcommand (Laysi/trustiot-v2#613).
//!
//! - **Reuse.** When the handle holds an object that [`TpmKey::open`] accepts (ECC NIST P-256,
//!   ECDSA-SHA256, the attributes of decision 12, and a test signature with the empty password
//!   that verifies), it is the key: [`Provisioned::created`] is false. A TPM keeps its state when
//!   the SD card is flashed again (docs/requirements/field-constraints.md), so a rerun after a
//!   reflash finds the key and keeps it.
//! - **Create.** When the handle is empty (`TPM_RC_HANDLE`), TPM2_Create makes a key with that
//!   template under the storage root key, TPM2_Load loads it, TPM2_EvictControl persists it at
//!   the handle, and the key is read back and checked like a reused one.
//! - **Refuse.** Anything else at the handle (an RSA key, a restricted key, P-384, a key with a
//!   password, a key that cannot sign) is refused with [`ProvisionError::Refused`]. It is never
//!   evicted or changed: the only TPM2_EvictControl sent here copies a transient object to an
//!   empty handle. Telling a password-protected key apart costs one failed authorization: a key
//!   without `noDA` adds one to the TPM's dictionary-attack counter, as the agent's start does.
//!
//! The parent is the storage root key at [`SRK_HANDLE`] when that is the standard ECC SRK (the
//! tpm2-tools default template, `marshal::srk_template`; the lab gateway has it). Otherwise a
//! primary with the same template is created in the owner hierarchy for this run and flushed
//! after TPM2_Load; [`SRK_HANDLE`] belongs to the LUKS unlock (Laysi/trustiot-v2#95), so it is
//! never written here. Which parent made the key does not matter afterwards: a persistent key
//! does not need its parent to be loaded, and `fixedTPM` keeps it in this TPM either way.
//!
//! Reruns and races: a rerun finds the key and reuses it, so it gets the same public key and adds
//! no persistent handle. A run that dies between TPM2_Create and TPM2_EvictControl leaves
//! nothing: the loaded key is transient, the kernel's resource manager flushes it when the
//! process closes `/dev/tpmrm0`, and nothing was persisted, so the next run creates a new key.
//! When two runs create at once, the second TPM2_EvictControl gets `TPM_RC_NV_DEFINED`; that run
//! flushes its own key and reuses the first one's if it passes the same checks.
//!
//! Every command goes through the worker with resends on `TPM_RC_RETRY`, `TPM_RC_YIELDED` and
//! `TPM_RC_TESTING` (`worker::Retry::OnWarnings`), and so do the signatures of the returned key.

use super::marshal::{self, PublicArea};
use super::worker::{Retry, Worker};
use super::{ResponseCode, TpmError, TpmKey, TpmTransport, spawn_worker};
use std::time::Duration;

/// The storage root key's persistent handle (TCG TPM v2.0 Provisioning Guidance, section 7.8).
pub const SRK_HANDLE: u32 = 0x8100_0001;

/// How long one provisioning command may take, resends and queueing included. Key generation
/// takes longer than a signature on a slow TPM; no watchdog limits the factory tool.
pub const PROVISION_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The persistent handles the owner can write with TPM2_EvictControl (Part 2, section 7.5); the
/// rest of `0x81xxxxxx` belongs to the platform.
const OWNER_PERSISTENT_HANDLES: std::ops::RangeInclusive<u32> = 0x8100_0000..=0x817f_ffff;

/// `TPM_RC_HANDLE` for the first handle: TPM2_ReadPublic of an empty persistent handle.
const HANDLE_EMPTY: u32 = 0x18b;
/// `TPM_RC_NV_DEFINED`: TPM2_EvictControl to a persistent handle that is already taken.
const NV_DEFINED: u32 = 0x14c;

/// The key at the handle, ready to sign.
#[derive(Debug)]
pub struct Provisioned {
    /// The key; its signatures resend on the warnings the provisioning commands resend on.
    pub key: TpmKey,
    /// True when this run created and persisted the key, false when it was already there (or
    /// another run persisted it first).
    pub created: bool,
}

/// Why [`ensure_key`] did not end with a usable key.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// The handle is not one the owner can write, or it is the storage root key's.
    #[error(
        "{0:#010x} cannot hold the gateway key: it must be an owner persistent handle (0x81000000 to 0x817fffff) other than the storage root key's ({SRK_HANDLE:#010x})"
    )]
    UnsupportedHandle(u32),
    /// The handle holds an object that is not the gateway key. It was left as it was.
    #[error("the object at {handle:#010x} is not the gateway key and was left untouched: {reason}")]
    Refused {
        /// The persistent handle.
        handle: u32,
        /// Why the object does not qualify.
        #[source]
        reason: TpmError,
    },
    /// The TPM could not be reached, refused a command, or answered something unexpected.
    #[error(transparent)]
    Tpm(#[from] TpmError),
}

/// Makes sure the persistent `handle` (on the gateway [`super::DEFAULT_KEY_HANDLE`]) holds the
/// gateway key, as the module documentation describes: reuses it, creates it, or refuses what is
/// there. Blocks for the TPM commands, each at most [`PROVISION_RESPONSE_TIMEOUT`].
///
/// # Errors
///
/// [`ProvisionError::UnsupportedHandle`] before any command for a handle outside the owner's
/// persistent range or [`SRK_HANDLE`]; [`ProvisionError::Refused`] when another object is at the
/// handle; [`ProvisionError::Tpm`] when the device cannot be opened, or the TPM refuses a
/// command or answers malformed.
pub fn ensure_key(transport: TpmTransport, handle: u32) -> Result<Provisioned, ProvisionError> {
    if !OWNER_PERSISTENT_HANDLES.contains(&handle) || handle == SRK_HANDLE {
        return Err(ProvisionError::UnsupportedHandle(handle));
    }
    let worker = spawn_worker(transport, PROVISION_RESPONSE_TIMEOUT, Retry::OnWarnings)?;
    match read_public(&worker, handle)? {
        Some(existing) => {
            tracing::info!(handle = %Hex(handle), "the handle holds an object: checking it");
            adopt(worker, handle, &existing, false)
        }
        None => {
            tracing::info!(handle = %Hex(handle), "the handle is empty: creating the key");
            create(worker, handle)
        }
    }
}

/// The object at `handle`, or `None` when the handle is empty.
fn read_public(worker: &Worker, handle: u32) -> Result<Option<PublicArea>, TpmError> {
    let response = worker.transact(marshal::read_public_command(handle))?;
    match marshal::parse_read_public_area(&response) {
        Ok(area) => Ok(Some(area)),
        Err(TpmError::Response(ResponseCode(HANDLE_EMPTY))) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Uses the object at `handle` as the key if it is one, or refuses it.
fn adopt(
    worker: Worker,
    handle: u32,
    object: &PublicArea,
    created: bool,
) -> Result<Provisioned, ProvisionError> {
    let refused = |reason| ProvisionError::Refused { handle, reason };
    // Exactly decision 12's template, stricter than `TpmKey::open`: provisioning takes over an
    // object only if it is what this function would have created.
    let public = marshal::parse_provisioned_area(&object.area).map_err(refused)?;
    match TpmKey::with_public(worker, handle, public) {
        Ok(key) => Ok(Provisioned { key, created }),
        Err(error @ TpmError::SelfTestFailed) => Err(refused(error)),
        Err(TpmError::Response(code)) if is_authorization_failure(code) => {
            Err(refused(TpmError::Response(code)))
        }
        Err(error) => Err(error.into()),
    }
}

/// A failed authorization: the key has a password (`TPM_RC_AUTH_FAIL`, or `TPM_RC_BAD_AUTH` for
/// a `noDA` key), so the agent cannot use it.
fn is_authorization_failure(code: ResponseCode) -> bool {
    let code = code.code();
    let format_one = code >> 16 == 0 && code & 0x080 != 0;
    format_one && matches!(code & 0x3f, 0x0e | 0x22)
}

/// Creates the key, persists it at the empty `handle`, and reads it back.
fn create(worker: Worker, handle: u32) -> Result<Provisioned, ProvisionError> {
    let template = marshal::key_template();
    let parent = Parent::find(&worker)?;
    let loaded = create_and_load(&worker, parent.handle(), &template);
    parent.release(&worker);
    let (public, key) = loaded?;

    let evicted = worker
        .transact(marshal::evict_control_command(key, handle))
        .and_then(|response| marshal::parse_evict_control(&response));
    // EvictControl copies the object; the loaded one is flushed whatever happened.
    flush(&worker, key);
    match evicted {
        Ok(()) => {
            let persisted = read_public(&worker, handle)?.ok_or_else(|| {
                TpmError::Malformed(format!(
                    "TPM2_EvictControl succeeded but {} is empty",
                    Hex(handle)
                ))
            })?;
            if persisted.area != public {
                return Err(TpmError::Malformed(format!(
                    "the key read back from {} is not the one this run created",
                    Hex(handle)
                ))
                .into());
            }
            tracing::info!(handle = %Hex(handle), "created and persisted the key");
            adopt(worker, handle, &persisted, true)
        }
        Err(TpmError::Response(ResponseCode(NV_DEFINED))) => {
            tracing::info!(
                handle = %Hex(handle),
                "another run persisted an object at the handle first: checking it"
            );
            let existing = read_public(&worker, handle)?.ok_or_else(|| {
                TpmError::Malformed(format!(
                    "TPM2_EvictControl found {} taken, but it is empty",
                    Hex(handle)
                ))
            })?;
            adopt(worker, handle, &existing, false)
        }
        Err(error) => Err(error.into()),
    }
}

/// TPM2_Create and TPM2_Load under `parent`; returns the key's public area and its transient
/// handle.
fn create_and_load(
    worker: &Worker,
    parent: u32,
    template: &[u8],
) -> Result<(Vec<u8>, u32), TpmError> {
    let response = worker.transact(marshal::create_command(parent, template))?;
    let created = marshal::parse_create(&response)?;
    if !marshal::matches_template(&created.public, template) {
        return Err(TpmError::Malformed(
            "TPM2_Create returned a key that does not have the template's fields".to_owned(),
        ));
    }
    let response = worker.transact(marshal::load_command(
        parent,
        &created.private,
        &created.public,
    )?)?;
    let loaded = marshal::parse_load(&response)?;
    if loaded.name != marshal::sha256_name(&created.public) {
        flush(worker, loaded.handle);
        return Err(TpmError::Malformed(
            "TPM2_Load returned a name that is not the created key's".to_owned(),
        ));
    }
    Ok((created.public, loaded.handle))
}

/// The storage key the gateway key is created under.
enum Parent {
    /// The standard SRK at [`SRK_HANDLE`].
    Persistent,
    /// A primary made for this run, flushed by [`Parent::release`].
    Transient(u32),
}

impl Parent {
    fn find(worker: &Worker) -> Result<Self, TpmError> {
        let template = marshal::srk_template();
        if let Some(srk) = read_public(worker, SRK_HANDLE)? {
            if marshal::matches_template(&srk.area, &template) {
                tracing::info!(srk = %Hex(SRK_HANDLE), "using the storage root key");
                return Ok(Self::Persistent);
            }
            tracing::info!(
                srk = %Hex(SRK_HANDLE),
                "the object at the SRK handle is not the standard SRK: leaving it alone"
            );
        }
        let response = worker.transact(marshal::create_primary_command(&template))?;
        let primary = marshal::parse_create_primary(&response)?;
        if !marshal::matches_template(&primary.area, &template)
            || primary.loaded.name != marshal::sha256_name(&primary.area)
        {
            flush(worker, primary.loaded.handle);
            return Err(TpmError::Malformed(
                "TPM2_CreatePrimary returned a key that is not the SRK template's".to_owned(),
            ));
        }
        tracing::info!("created a transient storage root key for this run");
        Ok(Self::Transient(primary.loaded.handle))
    }

    fn handle(&self) -> u32 {
        match self {
            Self::Persistent => SRK_HANDLE,
            Self::Transient(handle) => *handle,
        }
    }

    fn release(self, worker: &Worker) {
        if let Self::Transient(handle) = self {
            flush(worker, handle);
        }
    }
}

/// Flushes a transient object this run loaded. A failure is only logged: the kernel's resource
/// manager flushes it when the device is closed anyway.
fn flush(worker: &Worker, handle: u32) {
    let result = worker
        .transact(marshal::flush_context_command(handle))
        .and_then(|response| marshal::parse_flush_context(&response));
    if let Err(error) = result {
        tracing::warn!(handle = %Hex(handle), %error, "could not flush a transient object");
    }
}

/// A handle in the usual `0x81000100` form.
struct Hex(u32);

impl std::fmt::Display for Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#010x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    //! The whole flow against a scripted TPM that answers with the exchange recorded from swtpm
    //! (`super::super::fixtures`), checking every command byte for byte. The real TPM tests are
    //! `tests/tpm_provision.rs`.

    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};

    use super::super::fixtures as recorded;
    use super::super::marshal::tests::hex;
    use super::*;
    use crate::tpm::TpmChannel;

    const KEY: u32 = 0x8100_0100;

    /// Expects exactly these commands, in order, and gives these answers.
    struct Scripted {
        script: VecDeque<(Vec<u8>, Vec<u8>)>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl TpmChannel for Scripted {
        fn transact(&mut self, command: &[u8]) -> io::Result<Vec<u8>> {
            self.sent.lock().unwrap().push(command.to_vec());
            let Some((expected, response)) = self.script.pop_front() else {
                return Err(io::Error::other("the script has no more answers"));
            };
            assert_eq!(
                command,
                &expected[..],
                "unexpected command; expected {expected:02x?}"
            );
            Ok(response)
        }
    }

    fn run(steps: &[(&[u8], &str)]) -> (Result<Provisioned, ProvisionError>, Vec<Vec<u8>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let scripted = Scripted {
            script: steps
                .iter()
                .map(|(command, response)| (command.to_vec(), hex(response)))
                .collect(),
            sent: Arc::clone(&sent),
        };
        let result = ensure_key(TpmTransport::Channel(Box::new(scripted)), KEY);
        let sent = sent.lock().unwrap().clone();
        (result, sent)
    }

    fn read_public(handle: u32) -> Vec<u8> {
        marshal::read_public_command(handle)
    }

    #[test]
    fn an_empty_handle_gets_a_key_under_the_srk_and_a_retry_is_resent() {
        let create = hex(recorded::CREATE_COMMAND);
        let steps: &[(&[u8], &str)] = &[
            (&read_public(KEY), recorded::READ_PUBLIC_EMPTY_RESPONSE),
            (&read_public(SRK_HANDLE), recorded::READ_PUBLIC_SRK_RESPONSE),
            (&create, recorded::RETRY_RESPONSE),
            (&create, recorded::CREATE_RESPONSE),
            (&hex(recorded::LOAD_COMMAND), recorded::LOAD_RESPONSE),
            (
                &hex(recorded::EVICT_CONTROL_COMMAND),
                recorded::EVICT_CONTROL_RESPONSE,
            ),
            (
                &hex(recorded::FLUSH_KEY_COMMAND),
                recorded::FLUSH_CONTEXT_RESPONSE,
            ),
            (&read_public(KEY), recorded::READ_PUBLIC_KEY_RESPONSE),
            (
                &hex(recorded::SIGN_SELF_TEST_COMMAND),
                recorded::SIGN_SELF_TEST_RESPONSE,
            ),
        ];
        let (result, sent) = run(steps);
        let provisioned = result.unwrap();
        assert!(provisioned.created);
        assert_eq!(provisioned.key.handle(), KEY);
        let persisted = marshal::parse_read_public(&hex(recorded::READ_PUBLIC_KEY_RESPONSE));
        assert_eq!(
            provisioned.key.public_key_spki_der(),
            persisted.unwrap().spki_der()
        );
        assert_eq!(sent.len(), steps.len());
    }

    #[test]
    fn without_the_standard_srk_a_transient_primary_is_made_and_flushed() {
        // The SRK handle holds the gateway key's own kind of key, not a storage key.
        let steps: &[(&[u8], &str)] = &[
            (&read_public(KEY), recorded::READ_PUBLIC_EMPTY_RESPONSE),
            (&read_public(SRK_HANDLE), recorded::READ_PUBLIC_KEY_RESPONSE),
            (
                &hex(recorded::CREATE_PRIMARY_COMMAND),
                recorded::CREATE_PRIMARY_RESPONSE,
            ),
            (
                &marshal::create_command(0x8000_0000, &marshal::key_template()),
                recorded::CREATE_RESPONSE,
            ),
        ];
        // The recorded TPM2_Load was under 0x81000001; here the parent is 0x80000000. Stop the
        // script after TPM2_Create: the primary must still be flushed.
        let (result, sent) = run(steps);
        assert!(matches!(result, Err(ProvisionError::Tpm(TpmError::Io(_)))));
        let codes: Vec<u32> = sent
            .iter()
            .map(|command| u32::from_be_bytes(command[6..10].try_into().unwrap()))
            .collect();
        // ReadPublic, ReadPublic, CreatePrimary, Create, Load (no answer), FlushContext of the
        // primary; nothing touches 0x81000001.
        assert_eq!(codes, [0x173, 0x173, 0x131, 0x153, 0x157, 0x165]);
        assert_eq!(sent[5], hex(recorded::FLUSH_PRIMARY_COMMAND));
    }

    #[test]
    fn an_occupied_handle_after_create_reuses_the_other_runs_key() {
        let steps: &[(&[u8], &str)] = &[
            (&read_public(KEY), recorded::READ_PUBLIC_EMPTY_RESPONSE),
            (&read_public(SRK_HANDLE), recorded::READ_PUBLIC_SRK_RESPONSE),
            (&hex(recorded::CREATE_COMMAND), recorded::CREATE_RESPONSE),
            (&hex(recorded::LOAD_COMMAND), recorded::LOAD_RESPONSE),
            (
                &hex(recorded::EVICT_CONTROL_COMMAND),
                recorded::EVICT_CONTROL_DEFINED_RESPONSE,
            ),
            (
                &hex(recorded::FLUSH_KEY_COMMAND),
                recorded::FLUSH_CONTEXT_RESPONSE,
            ),
            (&read_public(KEY), recorded::READ_PUBLIC_KEY_RESPONSE),
            (
                &hex(recorded::SIGN_SELF_TEST_COMMAND),
                recorded::SIGN_SELF_TEST_RESPONSE,
            ),
        ];
        let (result, _) = run(steps);
        assert!(!result.unwrap().created);
    }

    #[test]
    fn an_existing_key_is_reused_with_two_commands() {
        let steps: &[(&[u8], &str)] = &[
            (&read_public(KEY), recorded::READ_PUBLIC_KEY_RESPONSE),
            (
                &hex(recorded::SIGN_SELF_TEST_COMMAND),
                recorded::SIGN_SELF_TEST_RESPONSE,
            ),
        ];
        let (result, sent) = run(steps);
        assert!(!result.unwrap().created);
        assert_eq!(sent.len(), 2);
    }

    #[test]
    fn other_objects_are_refused_after_reading_them_only() {
        // A storage key (the SRK's kind) at the key's handle.
        let (result, sent) = run(&[(&read_public(KEY), recorded::READ_PUBLIC_SRK_RESPONSE)]);
        match result {
            Err(ProvisionError::Refused { handle, reason }) => {
                assert_eq!(handle, KEY);
                assert!(reason.to_string().contains("symmetric"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(sent.len(), 1);
        // A key with a password: the test signature fails authorization.
        for code in ["80010000000a0000098e", "80010000000a000009a2"] {
            let (result, sent) = run(&[
                (&read_public(KEY), recorded::READ_PUBLIC_KEY_RESPONSE),
                (&hex(recorded::SIGN_SELF_TEST_COMMAND), code),
            ]);
            assert!(
                matches!(
                    result,
                    Err(ProvisionError::Refused {
                        reason: TpmError::Response(_),
                        ..
                    })
                ),
                "{result:?}"
            );
            assert_eq!(sent.len(), 2);
        }
        // Another error of the test signature is the TPM's, not a refusal.
        let (result, _) = run(&[
            (&read_public(KEY), recorded::READ_PUBLIC_KEY_RESPONSE),
            (
                &hex(recorded::SIGN_SELF_TEST_COMMAND),
                "80010000000a00000921",
            ),
        ]);
        assert!(
            matches!(result, Err(ProvisionError::Tpm(TpmError::Response(code))) if code.code() == 0x921),
            "{result:?}"
        );
    }

    /// The recorded ReadPublic of the key, with its public area changed by `edit` (the sizes are
    /// fixed up; the name is left as recorded, which provisioning does not check).
    fn key_response_with(edit: impl FnOnce(&mut Vec<u8>)) -> String {
        let recorded = hex(recorded::READ_PUBLIC_KEY_RESPONSE);
        let mut area = marshal::parse_read_public_area(&recorded).unwrap().area;
        // Header (10 bytes), then the TPM2B_PUBLIC's size and the area, then name and
        // qualifiedName.
        let rest = recorded[12 + area.len()..].to_vec();
        edit(&mut area);
        let mut response = recorded[..10].to_vec();
        response.extend_from_slice(&u16::try_from(area.len()).unwrap().to_be_bytes());
        response.extend_from_slice(&area);
        response.extend_from_slice(&rest);
        let size = u32::try_from(response.len()).unwrap().to_be_bytes();
        response[2..6].copy_from_slice(&size);
        response.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A change to a public area, for [`key_response_with`].
    type AreaEdit = Box<dyn FnOnce(&mut Vec<u8>)>;

    #[test]
    fn a_signing_key_that_differs_from_the_template_is_refused_after_reading_it_only() {
        // Unchanged, the response is the recorded one and the key is reused.
        assert_eq!(
            key_response_with(|_| {}),
            recorded::READ_PUBLIC_KEY_RESPONSE
        );
        // TPMT_PUBLIC: type (0..2), nameAlg (2..4), objectAttributes (4..8), authPolicy (8..).
        let set_attribute = |bit: u32| {
            move |area: &mut Vec<u8>| {
                let attributes = u32::from_be_bytes(area[4..8].try_into().unwrap()) | bit;
                area[4..8].copy_from_slice(&attributes.to_be_bytes());
            }
        };
        let cases: [(&str, AreaEdit); 5] = [
            // SHA-384 as the name algorithm.
            (
                "name algorithm 0x000c",
                Box::new(|area| area[2..4].copy_from_slice(&[0x00, 0x0c])),
            ),
            ("attributes 0x00040472", Box::new(set_attribute(0x400))), // noDA
            ("attributes 0x00040872", Box::new(set_attribute(0x800))), // encryptedDuplication
            ("attributes 0x000400f2", Box::new(set_attribute(0x80))),  // adminWithPolicy
            // A 32-byte authPolicy.
            (
                "32-byte authPolicy",
                Box::new(|area| {
                    area.splice(8..10, [0x00, 0x20].into_iter().chain([0xa5; 32]));
                }),
            ),
        ];
        for (expected, edit) in cases {
            let response = key_response_with(edit);
            let (result, sent) = run(&[(&read_public(KEY), &response)]);
            match result {
                Err(ProvisionError::Refused { handle, reason }) => {
                    assert_eq!(handle, KEY);
                    let reason = reason.to_string();
                    assert!(reason.contains("not decision 12's key"), "{reason}");
                    assert!(reason.contains(expected), "{expected:?} in {reason}");
                }
                other => panic!("{expected}: expected a refusal, got {other:?}"),
            }
            // No test signature, nothing else.
            assert_eq!(sent.len(), 1, "{expected}");
        }
    }

    #[test]
    fn a_tpm_error_reading_the_handle_is_not_taken_for_an_empty_handle() {
        // TPM_RC_VALUE, and TPM_RC_HANDLE for another handle number.
        for code in ["80010000000a000001c4", "80010000000a0000028b"] {
            let (result, sent) = run(&[(&read_public(KEY), code)]);
            assert!(
                matches!(result, Err(ProvisionError::Tpm(TpmError::Response(_)))),
                "{result:?}"
            );
            assert_eq!(sent.len(), 1);
        }
    }

    #[test]
    fn retries_give_up_after_a_bound() {
        let command = read_public(KEY);
        let steps = vec![(&command[..], recorded::RETRY_RESPONSE); 9];
        let (result, sent) = run(&steps);
        assert!(
            matches!(result, Err(ProvisionError::Tpm(TpmError::Response(code))) if code.code() == 0x922),
            "{result:?}"
        );
        // The first try and eight resends.
        assert_eq!(sent.len(), 9);
    }

    #[test]
    fn handles_that_cannot_hold_the_key_are_refused_before_any_command() {
        for handle in [
            SRK_HANDLE,
            0x8180_0000,
            0x81ff_ffff,
            0x8000_0000,
            0x0100_0000,
            0x4000_0001,
        ] {
            let (result, sent) = {
                let sent = Arc::new(Mutex::new(Vec::new()));
                let scripted = Scripted {
                    script: VecDeque::new(),
                    sent: Arc::clone(&sent),
                };
                let result = ensure_key(TpmTransport::Channel(Box::new(scripted)), handle);
                let sent = sent.lock().unwrap().clone();
                (result, sent)
            };
            assert!(
                matches!(result, Err(ProvisionError::UnsupportedHandle(h)) if h == handle),
                "{handle:#010x}"
            );
            assert!(sent.is_empty());
        }
    }

    #[test]
    fn authorization_failures_are_told_from_other_codes() {
        assert!(is_authorization_failure(ResponseCode(0x98e)));
        assert!(is_authorization_failure(ResponseCode(0x9a2)));
        assert!(!is_authorization_failure(ResponseCode(0x921)));
        assert!(!is_authorization_failure(ResponseCode(0x18b)));
        assert!(!is_authorization_failure(ResponseCode(0x000b_098e)));
    }
}
