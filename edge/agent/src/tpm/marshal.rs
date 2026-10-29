//! The TPM 2.0 commands the agent sends, with the structures of Part 2 they carry, and strict
//! parsing of their responses: TPM2_ReadPublic (TPM 2.0 Part 3, section 12.4) and TPM2_Sign
//! (section 20.2), which the agent uses at every start, and the provisioning commands of
//! `provision`: TPM2_CreatePrimary (24.1), TPM2_Create (12.1), TPM2_Load (12.2),
//! TPM2_EvictControl (28.5) and TPM2_FlushContext (28.4); and for the endorsement key
//! (`endorsement`), TPM2_NV_ReadPublic (31.6) and TPM2_NV_Read (31.13).
//!
//! A response is untrusted input: every length is checked against what is left of the buffer,
//! every structure must end exactly where its size says, and nothing here can panic on any input.

use super::TpmError;
use super::rc::ResponseCode;

/// The largest response accepted (the kernel's `TPM_BUFSIZE`, also the usual
/// `MAX_RESPONSE_SIZE` of a TPM). The responses parsed here are under 600 bytes.
pub(crate) const MAX_RESPONSE_BYTES: usize = 4096;

const TPM_ST_NO_SESSIONS: u16 = 0x8001;
const TPM_ST_SESSIONS: u16 = 0x8002;
const TPM_ST_HASHCHECK: u16 = 0x8024;
const TPM_ST_CREATION: u16 = 0x8021;

const TPM_CC_EVICT_CONTROL: u32 = 0x0000_0120;
const TPM_CC_CREATE_PRIMARY: u32 = 0x0000_0131;
const TPM_CC_NV_READ: u32 = 0x0000_014e;
const TPM_CC_CREATE: u32 = 0x0000_0153;
const TPM_CC_LOAD: u32 = 0x0000_0157;
const TPM_CC_SIGN: u32 = 0x0000_015d;
const TPM_CC_FLUSH_CONTEXT: u32 = 0x0000_0165;
const TPM_CC_NV_READ_PUBLIC: u32 = 0x0000_0169;
const TPM_CC_READ_PUBLIC: u32 = 0x0000_0173;

/// The password authorization session, here with an empty password.
const TPM_RS_PW: u32 = 0x4000_0009;
/// The owner (storage) hierarchy, used with its empty authorization as on the gateway.
pub(crate) const TPM_RH_OWNER: u32 = 0x4000_0001;
const TPM_RH_NULL: u32 = 0x4000_0007;
/// The endorsement hierarchy, used with its empty authorization (the TPM's default).
pub(crate) const TPM_RH_ENDORSEMENT: u32 = 0x4000_000b;

const TPM_ALG_AES: u16 = 0x0006;
const TPM_ALG_SHA256: u16 = 0x000b;
const TPM_ALG_NULL: u16 = 0x0010;
const TPM_ALG_ECDSA: u16 = 0x0018;
const TPM_ALG_ECC: u16 = 0x0023;
const TPM_ALG_CFB: u16 = 0x0043;
const TPM_ECC_NIST_P256: u16 = 0x0003;

/// The first byte of a transient object handle (Part 2, section 7.2), which TPM2_CreatePrimary
/// and TPM2_Load return.
const TRANSIENT_HANDLE_TYPE: u32 = 0x80;

/// `TPMA_OBJECT` bits (Part 2, section 8.3).
mod attribute {
    pub(super) const FIXED_TPM: u32 = 1 << 1;
    pub(super) const FIXED_PARENT: u32 = 1 << 4;
    pub(super) const SENSITIVE_DATA_ORIGIN: u32 = 1 << 5;
    pub(super) const USER_WITH_AUTH: u32 = 1 << 6;
    pub(super) const ADMIN_WITH_POLICY: u32 = 1 << 7;
    pub(super) const RESTRICTED: u32 = 1 << 16;
    pub(super) const DECRYPT: u32 = 1 << 17;
    pub(super) const SIGN: u32 = 1 << 18;
}

/// `TPMA_NV` bits (Part 2, section 13.4) and the index type in bits 4 to 7.
pub(crate) mod nv_attribute {
    pub(crate) const TYPE_MASK: u32 = 0xf << 4;
    pub(crate) const OWNERREAD: u32 = 1 << 17;
    pub(crate) const AUTHREAD: u32 = 1 << 18;
    /// TPMA_NV_NO_DA: a failed authorization of the index does not count towards the TPM's
    /// dictionary-attack lockout.
    pub(crate) const NO_DA: u32 = 1 << 25;
    pub(crate) const WRITTEN: u32 = 1 << 29;
}

/// `PolicyA` of the TCG EK Credential Profile (section B.6.1): `TPM2_PolicySecret` with the
/// endorsement hierarchy, the EK's admin policy in the default templates.
const EK_POLICY_A_SHA256: [u8; 32] = [
    0x83, 0x71, 0x97, 0x67, 0x44, 0x84, 0xb3, 0xf8, 0x1a, 0x90, 0xcc, 0x8d, 0x46, 0xa5, 0xd7, 0x24,
    0xfd, 0x52, 0xd7, 0x6e, 0x06, 0x52, 0x0b, 0x64, 0xf2, 0xa1, 0xda, 0x1b, 0x33, 0x14, 0x69, 0xaa,
];

/// The attributes the key must have: generated inside the TPM and never exportable
/// (decision 12), usable with the password session, and for signing.
const REQUIRED_ATTRIBUTES: [(u32, &str); 5] = [
    (attribute::FIXED_TPM, "fixedTPM"),
    (attribute::FIXED_PARENT, "fixedParent"),
    (attribute::SENSITIVE_DATA_ORIGIN, "sensitiveDataOrigin"),
    (attribute::USER_WITH_AUTH, "userWithAuth"),
    (attribute::SIGN, "sign"),
];
/// The attributes it must not have: a restricted key signs only digests the TPM hashed itself,
/// and a key that also decrypts mixes two uses of one secret.
const FORBIDDEN_ATTRIBUTES: [(u32, &str); 2] = [
    (attribute::RESTRICTED, "restricted"),
    (attribute::DECRYPT, "decrypt"),
];

/// Bytes of one P-256 coordinate and of one ECDSA signature component.
pub(crate) const P256_FIELD_BYTES: usize = 32;

/// The DER of a P-256 `SubjectPublicKeyInfo` up to the uncompressed point: SEQUENCE {
/// SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING { 0 unused bits, ... } }.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// The uncompressed form marker of an elliptic curve point (SEC 1, section 2.3.3).
const UNCOMPRESSED_POINT: u8 = 0x04;

/// TPM2_ReadPublic of `handle`.
pub(crate) fn read_public_command(handle: u32) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_NO_SESSIONS, TPM_CC_READ_PUBLIC);
    command.u32(handle);
    command.finish()
}

/// TPM2_Sign of a SHA-256 `digest` with the key at `handle`: empty-password session, scheme
/// ECDSA with SHA-256, and the null hash-check ticket (allowed for an unrestricted key).
pub(crate) fn sign_command(handle: u32, digest: &[u8; 32]) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_SESSIONS, TPM_CC_SIGN);
    command.u32(handle);
    command.password_session();
    // digest: TPM2B_DIGEST
    command.u16(32);
    command.bytes(digest);
    // inScheme: TPMT_SIG_SCHEME { ECDSA, SHA-256 }
    command.u16(TPM_ALG_ECDSA);
    command.u16(TPM_ALG_SHA256);
    // validation: TPMT_TK_HASHCHECK { TPM_ST_HASHCHECK, TPM_RH_NULL, empty digest }
    command.u16(TPM_ST_HASHCHECK);
    command.u32(TPM_RH_NULL);
    command.u16(0);
    command.finish()
}

/// The `TPMT_PUBLIC` template of the gateway key (decision 12, the 2026-10-12 PoC's template):
/// ECC NIST P-256 with scheme ECDSA-SHA256, name algorithm SHA-256, the attributes `fixedTPM`,
/// `fixedParent`, `sensitiveDataOrigin`, `userWithAuth` and `sign`, no policy, and an empty
/// `unique` (the TPM fills in the point).
pub(crate) fn key_template() -> Vec<u8> {
    let attributes = REQUIRED_ATTRIBUTES
        .iter()
        .fold(0, |bits, (bit, _)| bits | bit);
    let mut template = PublicWriter::new(attributes);
    template.u16(TPM_ALG_NULL); // symmetric: none
    template.u16(TPM_ALG_ECDSA);
    template.u16(TPM_ALG_SHA256);
    template.ecc_curve_and_empty_unique();
    template.0
}

/// The `TPMT_PUBLIC` template of the standard ECC storage root key: what `tpm2_createprimary -C o
/// -g sha256 -G ecc256:aes128cfb` uses (tpm2-tools 5.7, checked against its key on swtpm in the
/// unit tests): a restricted decryption key with AES-128-CFB, attributes `fixedTPM`,
/// `fixedParent`, `sensitiveDataOrigin`, `userWithAuth`, `restricted` and `decrypt` (not
/// `noDA`), no policy, and an empty `unique`. The lab gateway's key at 0x81000001 has this
/// template (docs/feasibility/2026-10-12-tpm-poc.md).
pub(crate) fn srk_template() -> Vec<u8> {
    let attributes = attribute::FIXED_TPM
        | attribute::FIXED_PARENT
        | attribute::SENSITIVE_DATA_ORIGIN
        | attribute::USER_WITH_AUTH
        | attribute::RESTRICTED
        | attribute::DECRYPT;
    let mut template = PublicWriter::new(attributes);
    // symmetric: TPMT_SYM_DEF_OBJECT { AES, 128 bits, CFB }
    template.u16(TPM_ALG_AES);
    template.u16(128);
    template.u16(TPM_ALG_CFB);
    template.u16(TPM_ALG_NULL); // scheme: none (no hash follows)
    template.ecc_curve_and_empty_unique();
    template.0
}

/// Bytes of each coordinate of the EK template's `unique`, all zero.
const EK_UNIQUE_COORDINATE_BYTES: u16 = 32;

/// The `TPMT_PUBLIC` of the default ECC NIST P-256 endorsement key: template L-2 of the TCG EK
/// Credential Profile (section B.3.4), which `tpm2_createek -G ecc` uses when the TPM defines no
/// EK template or nonce in NV. A restricted decryption key with AES-128-CFB, name algorithm
/// SHA-256, attributes `fixedTPM`, `fixedParent`, `sensitiveDataOrigin`, `adminWithPolicy`,
/// `restricted` and `decrypt` (0x000300b2), `PolicyA` as its policy, and a `unique` of two
/// 32-byte zero coordinates. The EK is a function of the endorsement seed and these bytes.
pub(crate) fn ek_template() -> Vec<u8> {
    let attributes = attribute::FIXED_TPM
        | attribute::FIXED_PARENT
        | attribute::SENSITIVE_DATA_ORIGIN
        | attribute::ADMIN_WITH_POLICY
        | attribute::RESTRICTED
        | attribute::DECRYPT;
    let mut template = Vec::with_capacity(122);
    template.extend_from_slice(&TPM_ALG_ECC.to_be_bytes());
    template.extend_from_slice(&TPM_ALG_SHA256.to_be_bytes()); // nameAlg
    template.extend_from_slice(&attributes.to_be_bytes());
    // authPolicy: TPM2B_DIGEST
    template.extend_from_slice(&32_u16.to_be_bytes());
    template.extend_from_slice(&EK_POLICY_A_SHA256);
    for value in [
        TPM_ALG_AES,
        128,
        TPM_ALG_CFB,
        TPM_ALG_NULL, // scheme
        TPM_ECC_NIST_P256,
        TPM_ALG_NULL, // kdf
    ] {
        template.extend_from_slice(&value.to_be_bytes());
    }
    for _ in 0..2 {
        template.extend_from_slice(&EK_UNIQUE_COORDINATE_BYTES.to_be_bytes());
        template.extend_from_slice(&[0; EK_UNIQUE_COORDINATE_BYTES as usize]);
    }
    template
}

/// Checks that `area` is the endorsement key made from [`ek_template`]: every field before
/// `unique` equal to the template's, and `unique` an ECC point of two coordinates of 1 to 32
/// bytes and nothing after it. Returns the point; whether it lies on the curve is the caller's
/// check.
pub(crate) fn parse_ek_area(area: &[u8]) -> Result<PublicKey, TpmError> {
    let template = ek_template();
    let unique_bytes = 2 * (2 + usize::from(EK_UNIQUE_COORDINATE_BYTES));
    let prefix = template
        .get(..template.len().saturating_sub(unique_bytes))
        .unwrap_or_default();
    let Some(unique) = area.strip_prefix(prefix) else {
        return Err(unsuitable(
            "the endorsement key's public area differs from the EK template before its unique field"
                .to_owned(),
        ));
    };
    let mut reader = Reader::new(unique);
    let x = reader.sized("EK x coordinate")?;
    let y = reader.sized("EK y coordinate")?;
    reader.finish("endorsement key's TPMT_PUBLIC")?;
    let mut point = [0_u8; 1 + 2 * P256_FIELD_BYTES];
    point[0] = UNCOMPRESSED_POINT;
    let (x_out, y_out) = point[1..].split_at_mut(P256_FIELD_BYTES);
    left_pad(x, x_out, "EK x coordinate")?;
    left_pad(y, y_out, "EK y coordinate")?;
    Ok(PublicKey { point })
}

/// TPM2_CreatePrimary in `hierarchy` (owner or endorsement, with its empty authorization) with
/// `template`, an empty `userAuth` and no creation data or PCRs.
pub(crate) fn create_primary_command(hierarchy: u32, template: &[u8]) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_SESSIONS, TPM_CC_CREATE_PRIMARY);
    command.u32(hierarchy);
    command.password_session();
    command.creation_parameters(template);
    command.finish()
}

/// TPM2_NV_ReadPublic of the NV index `index`.
pub(crate) fn nv_read_public_command(index: u32) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_NO_SESSIONS, TPM_CC_NV_READ_PUBLIC);
    command.u32(index);
    command.finish()
}

/// TPM2_NV_Read of `size` bytes at `offset` of the NV index `index`, authorized by `auth` (the
/// owner hierarchy or the index itself) with the empty password.
pub(crate) fn nv_read_command(auth: u32, index: u32, size: u16, offset: u16) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_SESSIONS, TPM_CC_NV_READ);
    command.u32(auth);
    command.u32(index);
    command.password_session();
    command.u16(size);
    command.u16(offset);
    command.finish()
}

/// What TPM2_NV_ReadPublic told about an NV index (`TPMS_NV_PUBLIC`, Part 2, section 13.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NvPublic {
    /// The index the TPM described, which must be the one asked for.
    pub(crate) index: u32,
    /// Its `TPMA_NV` attributes.
    pub(crate) attributes: u32,
    /// Its size in bytes.
    pub(crate) data_size: u16,
}

/// Parses a TPM2_NV_ReadPublic response.
pub(crate) fn parse_nv_read_public(response: &[u8]) -> Result<NvPublic, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_NO_SESSIONS)?;
    let mut public = Reader::new(reader.sized("TPM2B_NV_PUBLIC")?);
    let index = public.u32("nvIndex")?;
    let _name_alg = public.u16("nameAlg")?;
    let attributes = public.u32("attributes")?;
    let _auth_policy = public.sized("authPolicy")?;
    let data_size = public.u16("dataSize")?;
    public.finish("TPMS_NV_PUBLIC")?;
    // nvName (TPM2B_NAME): checked for shape only.
    reader.sized("nvName")?;
    reader.finish("TPM2_NV_ReadPublic response")?;
    Ok(NvPublic {
        index,
        attributes,
        data_size,
    })
}

/// Parses a TPM2_NV_Read response and returns the data, which must be exactly `size` bytes.
pub(crate) fn parse_nv_read(response: &[u8], size: u16) -> Result<Vec<u8>, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_SESSIONS)?;
    let parameters = reader.counted("parameterSize")?;
    let mut parameters = Reader::new(parameters);
    let data = parameters.sized("TPM2B_MAX_NV_BUFFER")?.to_vec();
    parameters.finish("TPM2_NV_Read parameters")?;
    one_session_response(reader, "TPM2_NV_Read response")?;
    if data.len() != usize::from(size) {
        return Err(malformed(format!(
            "TPM2_NV_Read returned {} bytes where {size} were asked for",
            data.len()
        )));
    }
    Ok(data)
}

/// TPM2_Create of an object with `template` under the storage key `parent` (empty
/// authorization), with an empty `userAuth` and no creation data or PCRs.
pub(crate) fn create_command(parent: u32, template: &[u8]) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_SESSIONS, TPM_CC_CREATE);
    command.u32(parent);
    command.password_session();
    command.creation_parameters(template);
    command.finish()
}

/// TPM2_Load of what TPM2_Create returned (`private` and `public` without their size fields)
/// under `parent`.
pub(crate) fn load_command(
    parent: u32,
    private: &[u8],
    public: &[u8],
) -> Result<Vec<u8>, TpmError> {
    let mut command = CommandWriter::new(TPM_ST_SESSIONS, TPM_CC_LOAD);
    command.u32(parent);
    command.password_session();
    command.sized(private)?;
    command.sized(public)?;
    Ok(command.finish())
}

/// TPM2_EvictControl that copies the loaded transient `object` to the persistent handle
/// `persistent`, authorized by the owner (empty authorization). It never removes anything: that
/// form takes a persistent object handle, which this crate never sends.
pub(crate) fn evict_control_command(object: u32, persistent: u32) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_SESSIONS, TPM_CC_EVICT_CONTROL);
    command.u32(TPM_RH_OWNER);
    command.u32(object);
    command.password_session();
    command.u32(persistent);
    command.finish()
}

/// TPM2_FlushContext of the transient object `handle`.
pub(crate) fn flush_context_command(handle: u32) -> Vec<u8> {
    let mut command = CommandWriter::new(TPM_ST_NO_SESSIONS, TPM_CC_FLUSH_CONTEXT);
    command.u32(handle);
    command.finish()
}

/// What TPM2_ReadPublic returned, unchecked: the `TPMT_PUBLIC` and the object's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicArea {
    /// The `TPMT_PUBLIC` bytes (without the size of its `TPM2B_PUBLIC`).
    pub(crate) area: Vec<u8>,
    /// The `TPM2B_NAME` contents: the name algorithm and the digest of `area`.
    pub(crate) name: Vec<u8>,
}

/// A loaded transient object: its handle and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Loaded {
    pub(crate) handle: u32,
    pub(crate) name: Vec<u8>,
}

/// What TPM2_CreatePrimary returned: the loaded primary and its public area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Primary {
    pub(crate) loaded: Loaded,
    pub(crate) area: Vec<u8>,
}

/// What TPM2_Create returned: the private and public parts to load (without their sizes).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Created {
    pub(crate) private: Vec<u8>,
    pub(crate) public: Vec<u8>,
}

impl std::fmt::Debug for Created {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Created")
            .field("private", &format_args!("{} bytes", self.private.len()))
            .field("public", &format_args!("{} bytes", self.public.len()))
            .finish()
    }
}

/// Parses a TPM2_ReadPublic response without judging the object.
pub(crate) fn parse_read_public_area(response: &[u8]) -> Result<PublicArea, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_NO_SESSIONS)?;
    let area = reader.sized("TPM2B_PUBLIC")?.to_vec();
    let name = reader.sized("name")?.to_vec();
    // qualifiedName (TPM2B_NAME): checked for shape only.
    reader.sized("qualifiedName")?;
    reader.finish("TPM2_ReadPublic response")?;
    Ok(PublicArea { area, name })
}

/// Parses a TPM2_CreatePrimary response.
pub(crate) fn parse_create_primary(response: &[u8]) -> Result<Primary, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_SESSIONS)?;
    let handle = transient_handle(&mut reader)?;
    let parameters = reader.counted("parameterSize")?;
    let mut parameters = Reader::new(parameters);
    let area = parameters.sized("outPublic")?.to_vec();
    creation_data(&mut parameters)?;
    let name = parameters.sized("name")?.to_vec();
    parameters.finish("TPM2_CreatePrimary parameters")?;
    one_session_response(reader, "TPM2_CreatePrimary response")?;
    Ok(Primary {
        loaded: Loaded { handle, name },
        area,
    })
}

/// Parses a TPM2_Create response.
pub(crate) fn parse_create(response: &[u8]) -> Result<Created, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_SESSIONS)?;
    let parameters = reader.counted("parameterSize")?;
    let mut parameters = Reader::new(parameters);
    let private = parameters.sized("outPrivate")?.to_vec();
    let public = parameters.sized("outPublic")?.to_vec();
    creation_data(&mut parameters)?;
    parameters.finish("TPM2_Create parameters")?;
    one_session_response(reader, "TPM2_Create response")?;
    if private.is_empty() || public.is_empty() {
        return Err(malformed(
            "TPM2_Create returned an empty private or public part".to_owned(),
        ));
    }
    Ok(Created { private, public })
}

/// Parses a TPM2_Load response.
pub(crate) fn parse_load(response: &[u8]) -> Result<Loaded, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_SESSIONS)?;
    let handle = transient_handle(&mut reader)?;
    let parameters = reader.counted("parameterSize")?;
    let mut parameters = Reader::new(parameters);
    let name = parameters.sized("name")?.to_vec();
    parameters.finish("TPM2_Load parameters")?;
    one_session_response(reader, "TPM2_Load response")?;
    Ok(Loaded { handle, name })
}

/// Parses a TPM2_EvictControl response (no parameters).
pub(crate) fn parse_evict_control(response: &[u8]) -> Result<(), TpmError> {
    let mut reader = response_parameters(response, TPM_ST_SESSIONS)?;
    let parameters = reader.counted("parameterSize")?;
    Reader::new(parameters).finish("TPM2_EvictControl parameters")?;
    one_session_response(reader, "TPM2_EvictControl response")
}

/// Parses a TPM2_FlushContext response (header only).
pub(crate) fn parse_flush_context(response: &[u8]) -> Result<(), TpmError> {
    response_parameters(response, TPM_ST_NO_SESSIONS)?.finish("TPM2_FlushContext response")
}

/// Whether `area` is an object made from `template` (a template built here, with an empty
/// `unique`): every byte before `unique` is equal, and `unique` is an ECC point of two
/// coordinates of 1 to 32 bytes.
pub(crate) fn matches_template(area: &[u8], template: &[u8]) -> bool {
    // The template ends with the two empty coordinates.
    let Some(prefix) = template.strip_suffix(&[0, 0, 0, 0]) else {
        return false;
    };
    let Some(unique) = area.strip_prefix(prefix) else {
        return false;
    };
    let mut reader = Reader::new(unique);
    let coordinate = |reader: &mut Reader<'_>, what| {
        reader
            .sized(what)
            .is_ok_and(|value| (1..=P256_FIELD_BYTES).contains(&value.len()))
    };
    coordinate(&mut reader, "x") && coordinate(&mut reader, "y") && reader.finish("unique").is_ok()
}

/// Checks that `area` is decision 12's key exactly, as provisioning requires before it adopts an
/// object: [`parse_public_area`]'s checks, then every field before `unique` equal to
/// [`key_template`], so the name algorithm is SHA-256, the attributes are exactly the five
/// required ones (no `noDA`, `encryptedDuplication` or any other bit) and there is no
/// authorization policy. `TpmKey::open` keeps the looser [`parse_public_area`] (decision 12).
pub(crate) fn parse_provisioned_area(area: &[u8]) -> Result<PublicKey, TpmError> {
    let public = parse_public_area(area)?;
    let template = key_template();
    if matches_template(area, &template) {
        return Ok(public);
    }
    let field = |bytes: &[u8]| -> Result<(u16, u32, usize), TpmError> {
        let mut reader = Reader::new(bytes);
        let _type = reader.u16("type")?;
        let name_alg = reader.u16("nameAlg")?;
        let attributes = reader.u32("objectAttributes")?;
        let auth_policy = reader.sized("authPolicy")?;
        Ok((name_alg, attributes, auth_policy.len()))
    };
    let (name_alg, attributes, policy) = field(area)?;
    let expected = field(&template)?;
    if (name_alg, attributes, policy) == expected {
        return Err(unsuitable(
            "its unique is not a P-256 point with coordinates of 1 to 32 bytes".to_owned(),
        ));
    }
    Err(unsuitable(format!(
        "it is not decision 12's key: name algorithm {name_alg:#06x}, attributes {attributes:#010x} \
         and a {policy}-byte authPolicy, where the template has {:#06x}, {:#010x} and no policy",
        expected.0, expected.1
    )))
}

/// The name of an object whose name algorithm is SHA-256 (both templates here): the algorithm
/// identifier followed by the SHA-256 of its `TPMT_PUBLIC` (Part 1, section 16).
pub(crate) fn sha256_name(area: &[u8]) -> Vec<u8> {
    let mut name = TPM_ALG_SHA256.to_be_bytes().to_vec();
    name.extend_from_slice(ring::digest::digest(&ring::digest::SHA256, area).as_ref());
    name
}

/// The object handle in a response's handle area; it must be a transient handle.
fn transient_handle(reader: &mut Reader<'_>) -> Result<u32, TpmError> {
    let handle = reader.u32("objectHandle")?;
    if handle >> 24 != TRANSIENT_HANDLE_TYPE {
        return Err(malformed(format!(
            "the TPM returned {handle:#010x}, not a transient object handle"
        )));
    }
    Ok(handle)
}

/// `creationData`, `creationHash` and `creationTicket` of TPM2_Create and TPM2_CreatePrimary,
/// checked for shape only.
fn creation_data(reader: &mut Reader<'_>) -> Result<(), TpmError> {
    reader.sized("creationData")?;
    reader.sized("creationHash")?;
    let tag = reader.u16("creationTicket tag")?;
    if tag != TPM_ST_CREATION {
        return Err(malformed(format!(
            "the creation ticket's tag is {tag:#06x}, not TPM_ST_CREATION"
        )));
    }
    reader.u32("creationTicket hierarchy")?;
    reader.sized("creationTicket digest")?;
    Ok(())
}

/// The response authorization area for one password session, which must end the response.
fn one_session_response(mut reader: Reader<'_>, what: &str) -> Result<(), TpmError> {
    reader.sized("nonceTPM")?;
    reader.u8("sessionAttributes")?;
    reader.sized("hmac")?;
    reader.finish(what)
}

/// What TPM2_ReadPublic told about the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicKey {
    /// The uncompressed point, `04 || x || y`.
    pub(crate) point: [u8; 1 + 2 * P256_FIELD_BYTES],
}

impl PublicKey {
    /// The key's DER `SubjectPublicKeyInfo`.
    pub(crate) fn spki_der(&self) -> Vec<u8> {
        let mut der = Vec::with_capacity(P256_SPKI_PREFIX.len() + self.point.len());
        der.extend_from_slice(&P256_SPKI_PREFIX);
        der.extend_from_slice(&self.point);
        der
    }
}

/// Parses a TPM2_ReadPublic response and checks that the key is one the agent may use: an ECC
/// NIST P-256 signing key with scheme ECDSA-SHA256 and the attributes of decision 12.
pub(crate) fn parse_read_public(response: &[u8]) -> Result<PublicKey, TpmError> {
    parse_public_area(&parse_read_public_area(response)?.area)
}

/// Checks a `TPMT_PUBLIC` (Part 2, section 12.2.4) as [`parse_read_public`] does.
pub(crate) fn parse_public_area(bytes: &[u8]) -> Result<PublicKey, TpmError> {
    let mut reader = Reader::new(bytes);
    let key_type = reader.u16("type")?;
    if key_type != TPM_ALG_ECC {
        return Err(unsuitable(format!(
            "it is not an ECC key (algorithm {key_type:#06x})"
        )));
    }
    let _name_alg = reader.u16("nameAlg")?;
    let attributes = reader.u32("objectAttributes")?;
    let _auth_policy = reader.sized("authPolicy")?;
    // TPMS_ECC_PARMS
    let symmetric = reader.u16("symmetric algorithm")?;
    if symmetric != TPM_ALG_NULL {
        // keyBits and mode follow; only a storage (restricted decryption) key has them.
        return Err(unsuitable(format!(
            "it has a symmetric algorithm ({symmetric:#06x}), so it is not a signing key"
        )));
    }
    let scheme = reader.u16("scheme")?;
    let scheme_hash = if scheme == TPM_ALG_NULL {
        None
    } else {
        Some(reader.u16("scheme hash")?)
    };
    if scheme != TPM_ALG_ECDSA || scheme_hash != Some(TPM_ALG_SHA256) {
        return Err(unsuitable(format!(
            "its scheme is {scheme:#06x} with hash {}, not ECDSA ({TPM_ALG_ECDSA:#06x}) with SHA-256 ({TPM_ALG_SHA256:#06x})",
            scheme_hash.map_or_else(|| "none".to_owned(), |hash| format!("{hash:#06x}"))
        )));
    }
    let curve = reader.u16("curveID")?;
    if curve != TPM_ECC_NIST_P256 {
        return Err(unsuitable(format!(
            "its curve is {curve:#06x}, not NIST P-256 ({TPM_ECC_NIST_P256:#06x})"
        )));
    }
    let kdf = reader.u16("kdf")?;
    if kdf != TPM_ALG_NULL {
        return Err(unsuitable(format!(
            "it has a key derivation scheme ({kdf:#06x})"
        )));
    }
    // unique: TPMS_ECC_POINT
    let x = reader.sized("x coordinate")?;
    let y = reader.sized("y coordinate")?;
    reader.finish("TPMT_PUBLIC")?;

    let missing: Vec<&str> = REQUIRED_ATTRIBUTES
        .iter()
        .filter(|(bit, _)| attributes & bit == 0)
        .map(|(_, name)| *name)
        .collect();
    let present: Vec<&str> = FORBIDDEN_ATTRIBUTES
        .iter()
        .filter(|(bit, _)| attributes & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if !missing.is_empty() || !present.is_empty() {
        return Err(unsuitable(format!(
            "its attributes {attributes:#010x} lack [{}] or have [{}]",
            missing.join(", "),
            present.join(", ")
        )));
    }

    let mut point = [0_u8; 1 + 2 * P256_FIELD_BYTES];
    point[0] = UNCOMPRESSED_POINT;
    let (x_out, y_out) = point[1..].split_at_mut(P256_FIELD_BYTES);
    left_pad(x, x_out, "x coordinate")?;
    left_pad(y, y_out, "y coordinate")?;
    Ok(PublicKey { point })
}

/// Parses a TPM2_Sign response and returns the signature as a DER `ECDSA-Sig-Value`.
pub(crate) fn parse_sign(response: &[u8]) -> Result<Vec<u8>, TpmError> {
    let mut reader = response_parameters(response, TPM_ST_SESSIONS)?;
    let parameters = reader.counted("parameterSize")?;
    let mut signature = Reader::new(parameters);
    let algorithm = signature.u16("sigAlg")?;
    if algorithm != TPM_ALG_ECDSA {
        return Err(malformed(format!(
            "the signature algorithm is {algorithm:#06x}, not ECDSA"
        )));
    }
    let hash = signature.u16("signature hash")?;
    if hash != TPM_ALG_SHA256 {
        return Err(malformed(format!(
            "the signature hash is {hash:#06x}, not SHA-256"
        )));
    }
    let r = signature.sized("signatureR")?;
    let s = signature.sized("signatureS")?;
    signature.finish("TPMT_SIGNATURE")?;
    // The response authorization area: one TPMS_AUTH_RESPONSE for the password session.
    reader.sized("nonceTPM")?;
    reader.u8("sessionAttributes")?;
    reader.sized("hmac")?;
    reader.finish("TPM2_Sign response")?;
    ecdsa_der(r, s)
}

/// Checks the response header and returns a reader over what follows it. A response code other
/// than success is returned as [`TpmError::Response`], whatever tag it came with.
fn response_parameters(response: &[u8], expected_tag: u16) -> Result<Reader<'_>, TpmError> {
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(malformed(format!(
            "{} bytes, more than the {MAX_RESPONSE_BYTES} accepted",
            response.len()
        )));
    }
    let mut reader = Reader::new(response);
    let tag = reader.u16("tag")?;
    let size = reader.u32("responseSize")?;
    let code = reader.u32("responseCode")?;
    if usize::try_from(size).ok() != Some(response.len()) {
        return Err(malformed(format!(
            "the header says {size} bytes, {} arrived",
            response.len()
        )));
    }
    if code != 0 {
        return Err(TpmError::Response(ResponseCode(code)));
    }
    if tag != expected_tag {
        return Err(malformed(format!(
            "tag {tag:#06x} where {expected_tag:#06x} was expected"
        )));
    }
    Ok(reader)
}

/// Encodes r and s (big-endian, as the TPM returns them) as `ECDSA-Sig-Value ::= SEQUENCE {
/// r INTEGER, s INTEGER }`.
pub(crate) fn ecdsa_der(r: &[u8], s: &[u8]) -> Result<Vec<u8>, TpmError> {
    let r = der_integer(r, "signatureR")?;
    let s = der_integer(s, "signatureS")?;
    // At most 2 * 35 bytes, so the length fits the short form.
    let length = u8::try_from(r.len() + s.len())
        .map_err(|_| malformed("the signature is too long".to_owned()))?;
    let mut der = Vec::with_capacity(2 + r.len() + s.len());
    der.extend_from_slice(&[0x30, length]);
    der.extend_from_slice(&r);
    der.extend_from_slice(&s);
    Ok(der)
}

/// A positive DER INTEGER of at most 32 significant bytes.
fn der_integer(value: &[u8], what: &str) -> Result<Vec<u8>, TpmError> {
    let first = value.iter().position(|&byte| byte != 0);
    let Some(significant) = first.and_then(|first| value.get(first..)) else {
        return Err(malformed(format!("{what} is zero")));
    };
    if significant.len() > P256_FIELD_BYTES {
        return Err(malformed(format!(
            "{what} has {} significant bytes, more than P-256 allows",
            significant.len()
        )));
    }
    let pad = significant.first().is_some_and(|&byte| byte & 0x80 != 0);
    let length = significant.len() + usize::from(pad);
    let mut der = Vec::with_capacity(2 + length);
    der.push(0x02);
    // At most 33.
    der.push(u8::try_from(length).map_err(|_| malformed(format!("{what} is too long")))?);
    if pad {
        der.push(0);
    }
    der.extend_from_slice(significant);
    Ok(der)
}

/// Copies a big-endian coordinate into `out`, right-aligned. A TPM may leave out leading zeros.
fn left_pad(value: &[u8], out: &mut [u8], what: &str) -> Result<(), TpmError> {
    if value.is_empty() {
        return Err(malformed(format!("the {what} is empty")));
    }
    let skip = value.iter().take_while(|&&byte| byte == 0).count();
    let significant = value.get(skip..).unwrap_or_default();
    let start = out.len().checked_sub(significant.len()).ok_or_else(|| {
        malformed(format!(
            "the {what} has {} bytes, more than P-256 allows",
            value.len()
        ))
    })?;
    out.fill(0);
    if let Some(target) = out.get_mut(start..) {
        target.copy_from_slice(significant);
    }
    Ok(())
}

fn malformed(detail: String) -> TpmError {
    TpmError::Malformed(detail)
}

fn unsuitable(reason: String) -> TpmError {
    TpmError::UnsuitableKey(reason)
}

/// Builds a command; the size field is filled in by [`CommandWriter::finish`].
struct CommandWriter(Vec<u8>);

impl CommandWriter {
    fn new(tag: u16, code: u32) -> Self {
        let mut writer = Self(Vec::with_capacity(80));
        writer.u16(tag);
        writer.u32(0);
        writer.u32(code);
        writer
    }

    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    /// A `TPM2B`: a 16-bit size and `value`.
    fn sized(&mut self, value: &[u8]) -> Result<(), TpmError> {
        let size = u16::try_from(value.len()).map_err(|_| {
            malformed(format!(
                "{} bytes do not fit in a TPM2B structure",
                value.len()
            ))
        })?;
        self.u16(size);
        self.bytes(value);
        Ok(())
    }

    /// The authorization area of a command with one handle that needs authorization: the
    /// password session with an empty password.
    fn password_session(&mut self) {
        self.u32(9); // authorizationSize: one TPMS_AUTH_COMMAND
        self.u32(TPM_RS_PW);
        self.u16(0); // nonceCaller: empty
        self.u8(0); // sessionAttributes: none
        self.u16(0); // hmac: the empty password
    }

    /// The parameters of TPM2_Create and TPM2_CreatePrimary: `inSensitive` with an empty
    /// `userAuth` and no data, `inPublic` (the template, under 100 bytes), an empty
    /// `outsideInfo` and no `creationPCR`.
    fn creation_parameters(&mut self, template: &[u8]) {
        // TPM2B_SENSITIVE_CREATE { size 4, TPMS_SENSITIVE_CREATE { userAuth: empty, data: empty } }
        self.u16(4);
        self.u16(0);
        self.u16(0);
        // The templates built here are under 100 bytes.
        self.u16(u16::try_from(template.len()).unwrap_or(u16::MAX));
        self.bytes(template);
        self.u16(0); // outsideInfo: empty
        self.u32(0); // creationPCR: TPML_PCR_SELECTION with no entries
    }

    fn finish(mut self) -> Vec<u8> {
        // The commands here are under 300 bytes (TPM2_Load carries what TPM2_Create returned,
        // whose response is at most MAX_RESPONSE_BYTES).
        let size = u32::try_from(self.0.len()).unwrap_or(u32::MAX);
        if let Some(field) = self.0.get_mut(2..6) {
            field.copy_from_slice(&size.to_be_bytes());
        }
        self.0
    }
}

/// Builds an ECC `TPMT_PUBLIC` template with name algorithm SHA-256 and no policy.
struct PublicWriter(Vec<u8>);

impl PublicWriter {
    fn new(attributes: u32) -> Self {
        let mut writer = Self(Vec::with_capacity(32));
        writer.u16(TPM_ALG_ECC);
        writer.u16(TPM_ALG_SHA256); // nameAlg
        writer.0.extend_from_slice(&attributes.to_be_bytes());
        writer.u16(0); // authPolicy: empty
        writer
    }

    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    /// curveID NIST P-256, kdf none, and `unique` with two empty coordinates.
    fn ecc_curve_and_empty_unique(&mut self) {
        self.u16(TPM_ECC_NIST_P256);
        self.u16(TPM_ALG_NULL);
        self.u16(0);
        self.u16(0);
    }
}

/// Reads big-endian TPM structures from a buffer without ever reading past its end.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn take(&mut self, count: usize, what: &str) -> Result<&'a [u8], TpmError> {
        let Some((taken, rest)) = self.rest.split_at_checked(count) else {
            return Err(malformed(format!(
                "{what} needs {count} bytes, {} are left",
                self.rest.len()
            )));
        };
        self.rest = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], TpmError> {
        let bytes = self.take(N, what)?;
        let mut array = [0_u8; N];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    fn u8(&mut self, what: &str) -> Result<u8, TpmError> {
        Ok(u8::from_be_bytes(self.array(what)?))
    }

    fn u16(&mut self, what: &str) -> Result<u16, TpmError> {
        Ok(u16::from_be_bytes(self.array(what)?))
    }

    fn u32(&mut self, what: &str) -> Result<u32, TpmError> {
        Ok(u32::from_be_bytes(self.array(what)?))
    }

    /// A `TPM2B`: a 16-bit size and that many bytes.
    fn sized(&mut self, what: &str) -> Result<&'a [u8], TpmError> {
        let size = self.u16(what)?;
        self.take(usize::from(size), what)
    }

    /// A 32-bit size and that many bytes (the parameter area of a response with sessions).
    fn counted(&mut self, what: &str) -> Result<&'a [u8], TpmError> {
        let size = self.u32(what)?;
        let size = usize::try_from(size)
            .map_err(|_| malformed(format!("{what} {size} does not fit in memory")))?;
        self.take(size, what)
    }

    /// Fails when anything is left: every structure must end where its size says.
    fn finish(self, what: &str) -> Result<(), TpmError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(malformed(format!(
                "{} bytes left over after the {what}",
                self.rest.len()
            )))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! The fixtures are responses of swtpm 0.10 (Debian trixie) to the commands built here, for a
    //! throwaway key made by tpm2-tools with the PoC's template (2026-10-21).

    use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};

    use super::*;

    /// Tag, size and response code.
    const HEADER_BYTES: usize = 10;

    /// TPM2_ReadPublic of the fixture key at 0x81000100.
    pub(crate) const READ_PUBLIC_RESPONSE: &str = "8001000000ac0000000000580023000b00040072000000100018000b000300100020a4ab29f0245cf560c6f4961c0190591d9d2a552cd4443b5965b73211aa6ba87900205d5cd1cc3f51883a2f4fe1eb1f1578657ebdf119533ef24f6952fad802acf4190022000b0eb2cf7b640dcd6dc596ac1d8a578ca64e6bef7b5441ed5b0f380f702abcb6fd0022000bf6b93c32da9049318d18c9d942a18a9bde7a700cc1d2648b508fc9bddcf404f6";
    /// TPM2_Sign of SHA-256("abc") with that key.
    const SIGN_RESPONSE: &str = "80020000005b00000000000000480018000b00208d2b3b00c15c23b4d818e702af8578d7e120b5715502aba8e554e445cc0134c30020b60c2313a96573eed745af33864fd67ef862bae3b93477d70881abd8cb90f2f00000010000";
    /// `tpm2_readpublic -f der` of the same key: tpm2-tools' SubjectPublicKeyInfo.
    const TPM2_TOOLS_SPKI: &str = "3059301306072a8648ce3d020106082a8648ce3d03010703420004a4ab29f0245cf560c6f4961c0190591d9d2a552cd4443b5965b73211aa6ba8795d5cd1cc3f51883a2f4fe1eb1f1578657ebdf119533ef24f6952fad802acf419";
    /// The command that produced `SIGN_RESPONSE`, as swtpm accepted it.
    /// Underscores separate the fields: header, key handle, authorization area, digest, scheme,
    /// ticket.
    const SIGN_COMMAND: &str = "8002_00000049_0000015d_81000100_00000009_40000009_0000_00_0000_0020_ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad_0018_000b_8024_40000007_0000";

    /// Decodes hex, skipping `_` separators.
    pub(crate) fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text.bytes().filter(|byte| *byte != b'_').collect();
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn sha256(message: &[u8]) -> [u8; 32] {
        ring::digest::digest(&ring::digest::SHA256, message)
            .as_ref()
            .try_into()
            .unwrap()
    }

    #[test]
    fn commands_are_marshalled_as_tpm2_part_3_specifies() {
        assert_eq!(
            read_public_command(0x8100_0100),
            hex("8001_0000000e_00000173_81000100")
        );
        assert_eq!(
            sign_command(0x8100_0100, &sha256(b"abc")),
            hex(SIGN_COMMAND)
        );
    }

    #[test]
    fn read_public_gives_the_same_key_as_tpm2_tools() {
        let key = parse_read_public(&hex(READ_PUBLIC_RESPONSE)).unwrap();
        assert_eq!(key.spki_der(), hex(TPM2_TOOLS_SPKI));
    }

    #[test]
    fn a_tpm_signature_becomes_der_that_verifies_with_the_public_key() {
        let key = parse_read_public(&hex(READ_PUBLIC_RESPONSE)).unwrap();
        let der = parse_sign(&hex(SIGN_RESPONSE)).unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key.point)
            .verify(b"abc", &der)
            .unwrap();
        assert!(
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key.point)
                .verify(b"abd", &der)
                .is_err()
        );
    }

    #[test]
    fn every_truncation_of_a_response_is_an_error() {
        for fixture in [READ_PUBLIC_RESPONSE, SIGN_RESPONSE] {
            let response = hex(fixture);
            for length in 0..response.len() {
                let truncated = response.get(..length).unwrap();
                assert!(parse_read_public(truncated).is_err(), "length {length}");
                assert!(parse_sign(truncated).is_err(), "length {length}");
                // The size field still claims the whole response.
                if length >= HEADER_BYTES {
                    assert!(matches!(
                        parse_read_public(truncated),
                        Err(TpmError::Malformed(_))
                    ));
                }
            }
        }
    }

    /// Sets the header's size to the buffer's length, so the parser looks inside.
    fn reframe(mut response: Vec<u8>) -> Vec<u8> {
        let size = u32::try_from(response.len()).unwrap().to_be_bytes();
        response.get_mut(2..6).unwrap().copy_from_slice(&size);
        response
    }

    #[test]
    fn trailing_bytes_and_wrong_sizes_are_errors() {
        let mut longer = hex(READ_PUBLIC_RESPONSE);
        longer.push(0);
        assert!(matches!(
            parse_read_public(&longer),
            Err(TpmError::Malformed(detail)) if detail.contains("header says")
        ));
        assert!(matches!(
            parse_read_public(&reframe(longer)),
            Err(TpmError::Malformed(detail)) if detail.contains("left over")
        ));
        let mut longer = hex(SIGN_RESPONSE);
        longer.push(0);
        assert!(matches!(
            parse_sign(&reframe(longer)),
            Err(TpmError::Malformed(detail)) if detail.contains("left over")
        ));
        // The parameter area says one byte more than the signature has.
        let mut response = hex(SIGN_RESPONSE);
        response[13] += 1;
        assert!(matches!(parse_sign(&response), Err(TpmError::Malformed(_))));
        // Oversized: more than the TPM could ever send.
        let oversized = reframe(vec![0x80; MAX_RESPONSE_BYTES + 1]);
        assert!(matches!(
            parse_sign(&oversized),
            Err(TpmError::Malformed(detail)) if detail.contains("more than")
        ));
    }

    #[test]
    fn inflated_inner_lengths_never_read_past_the_buffer() {
        // Every pair of bytes after the header set to 0xffff in turn: each size field, and
        // everything else. Changed data inside a coordinate or a name may still parse.
        for (fixture, size_fields) in [
            // TPM2B_PUBLIC, authPolicy, x, y, name, qualifiedName.
            (READ_PUBLIC_RESPONSE, &[10_usize, 20, 32, 66, 100, 136][..]),
            // parameterSize (its low half), signatureR, signatureS, nonceTPM, hmac.
            (SIGN_RESPONSE, &[12, 18, 52, 86, 89][..]),
        ] {
            let response = hex(fixture);
            for offset in HEADER_BYTES..response.len() - 1 {
                let mut changed = response.clone();
                changed[offset] = 0xff;
                changed[offset + 1] = 0xff;
                let read_public = parse_read_public(&changed);
                let sign = parse_sign(&changed);
                if size_fields.contains(&offset) {
                    assert!(read_public.is_err() && sign.is_err(), "offset {offset}");
                }
            }
        }
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        // A fixed xorshift sequence, so a failure reproduces.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let length = usize::try_from(next() % 300).unwrap();
            let mut garbage: Vec<u8> = (0..length).map(|_| next().to_be_bytes()[0]).collect();
            // Half of them get a plausible header, so the parsers go past it.
            if next() % 2 == 0 && garbage.len() >= HEADER_BYTES {
                garbage[0] = 0x80;
                garbage[1] = if next() % 2 == 0 { 0x01 } else { 0x02 };
                garbage[6..10].fill(0);
                garbage = reframe(garbage);
            }
            let _ = parse_read_public(&garbage);
            let _ = parse_sign(&garbage);
        }
    }

    #[test]
    fn an_error_code_is_reported_whatever_the_tag() {
        assert!(matches!(
            parse_read_public(&hex("80010000000a0000018b")),
            Err(TpmError::Response(code)) if code.code() == 0x18b
        ));
        // TPM_RC_BAD_TAG comes back with the TPM 1.2 tag.
        assert!(matches!(
            parse_sign(&hex("00c40000000a0000001e")),
            Err(TpmError::Response(code)) if code.code() == 0x1e
        ));
        // Success with the wrong tag is not believed.
        let mut response = hex(SIGN_RESPONSE);
        response[1] = 0x01;
        assert!(matches!(
            parse_sign(&response),
            Err(TpmError::Malformed(detail)) if detail.contains("tag")
        ));
    }

    /// The fixture's public area with `change` applied, wrapped in a ReadPublic response.
    fn with_public_area(change: impl FnOnce(&mut Vec<u8>)) -> Result<PublicKey, TpmError> {
        let response = hex(READ_PUBLIC_RESPONSE);
        // Header (10), TPM2B_PUBLIC size (2), area (0x58 bytes), then the names.
        let mut area = response[12..12 + 0x58].to_vec();
        let names = response[12 + 0x58..].to_vec();
        change(&mut area);
        let mut rebuilt = response[..10].to_vec();
        rebuilt.extend_from_slice(&u16::try_from(area.len()).unwrap().to_be_bytes());
        rebuilt.extend_from_slice(&area);
        rebuilt.extend_from_slice(&names);
        parse_read_public(&reframe(rebuilt))
    }

    fn unsuitable_reason(result: Result<PublicKey, TpmError>) -> String {
        match result {
            Err(TpmError::UnsuitableKey(reason)) => reason,
            other => panic!("expected an unsuitable key, got {other:?}"),
        }
    }

    #[test]
    fn keys_other_than_a_p256_ecdsa_sha256_signing_key_are_refused() {
        // Offsets in the public area: type 0, nameAlg 2, attributes 4..8, authPolicy size 8..10
        // (empty), symmetric 10, scheme 12, scheme hash 14, curve 16, kdf 18, x 20.
        assert!(with_public_area(|_| {}).is_ok());
        let rsa = unsuitable_reason(with_public_area(|area| area[0..2].copy_from_slice(&[0, 1])));
        assert!(rsa.contains("not an ECC key"), "{rsa}");
        let p384 = unsuitable_reason(with_public_area(|area| area[17] = 0x04));
        assert!(p384.contains("curve"), "{p384}");
        let sha384 = unsuitable_reason(with_public_area(|area| area[15] = 0x0c));
        assert!(sha384.contains("scheme"), "{sha384}");
        // A null scheme has no hash field.
        let null = unsuitable_reason(with_public_area(|area| {
            area[12..14].copy_from_slice(&TPM_ALG_NULL.to_be_bytes());
            area.drain(14..16);
        }));
        assert!(null.contains("hash none"), "{null}");
        let kdf = unsuitable_reason(with_public_area(|area| area[19] = 0x20));
        assert!(kdf.contains("key derivation"), "{kdf}");
        let symmetric = unsuitable_reason(with_public_area(|area| area[11] = 0x06));
        assert!(symmetric.contains("symmetric"), "{symmetric}");
    }

    #[test]
    fn keys_with_the_wrong_attributes_are_refused() {
        let set = |bits: u32| {
            unsuitable_reason(with_public_area(|area| {
                let value = u32::from_be_bytes(area[4..8].try_into().unwrap()) | bits;
                area[4..8].copy_from_slice(&value.to_be_bytes());
            }))
        };
        let clear = |bits: u32| {
            unsuitable_reason(with_public_area(|area| {
                let value = u32::from_be_bytes(area[4..8].try_into().unwrap()) & !bits;
                area[4..8].copy_from_slice(&value.to_be_bytes());
            }))
        };
        assert!(set(attribute::RESTRICTED).contains("have [restricted]"));
        assert!(set(attribute::DECRYPT).contains("have [decrypt]"));
        assert!(clear(attribute::FIXED_TPM).contains("lack [fixedTPM]"));
        assert!(clear(attribute::FIXED_PARENT).contains("lack [fixedParent]"));
        assert!(clear(attribute::SENSITIVE_DATA_ORIGIN).contains("lack [sensitiveDataOrigin]"));
        assert!(clear(attribute::USER_WITH_AUTH).contains("lack [userWithAuth]"));
        assert!(clear(attribute::SIGN).contains("lack [sign]"));
    }

    #[test]
    fn short_coordinates_are_left_padded_and_long_ones_refused() {
        // A coordinate of 31 bytes is right-aligned in the 32-byte field.
        let key = with_public_area(|area| {
            area[20..22].copy_from_slice(&31_u16.to_be_bytes());
            area.remove(22);
        })
        .unwrap();
        assert_eq!(key.point[1], 0);
        assert_eq!(key.point[2..33], hex(READ_PUBLIC_RESPONSE)[35..66]);
        let long = with_public_area(|area| {
            area[20..22].copy_from_slice(&33_u16.to_be_bytes());
            area.insert(22, 0x01);
        });
        assert!(matches!(long, Err(TpmError::Malformed(_))));
        let empty = with_public_area(|area| {
            area[20..22].copy_from_slice(&0_u16.to_be_bytes());
            area.drain(22..54);
        });
        assert!(matches!(empty, Err(TpmError::Malformed(_))));
    }

    #[test]
    fn der_integers_are_minimal_and_positive() {
        let high = [0x80_u8; 32];
        let der = ecdsa_der(&high, &[0, 0, 1]).unwrap();
        assert_eq!(der[..4], [0x30, 2 + 33 + 3, 0x02, 33]);
        assert_eq!(der[4], 0);
        assert_eq!(der[37..], [0x02, 1, 1]);
        // A leading zero byte from the TPM (33 bytes) is accepted and dropped.
        let mut padded = vec![0_u8];
        padded.extend_from_slice(&[0x11; 32]);
        assert_eq!(ecdsa_der(&padded, &[1]).unwrap()[2..4], [0x02, 32]);
        assert!(ecdsa_der(&[0; 32], &[1]).is_err());
        assert!(ecdsa_der(&[1], &[]).is_err());
        assert!(ecdsa_der(&[1; 33], &[1]).is_err());
    }

    // The provisioning commands, against the exchange recorded in `super::super::fixtures`.

    use super::super::fixtures as recorded;

    fn area_of(read_public: &str) -> Vec<u8> {
        parse_read_public_area(&hex(read_public)).unwrap().area
    }

    #[test]
    fn the_templates_are_the_poc_key_and_the_tpm2_tools_srk() {
        assert_eq!(
            key_template(),
            hex("0023_000b_00040072_0000_0010_0018_000b_0003_0010_0000_0000")
        );
        assert_eq!(
            srk_template(),
            hex("0023_000b_00030072_0000_0006_0080_0043_0010_0003_0010_0000_0000")
        );
        // The PoC's key (made by tpm2-tools), the recorded key and the SRK made by tpm2-tools.
        assert!(matches_template(
            &area_of(READ_PUBLIC_RESPONSE),
            &key_template()
        ));
        assert!(matches_template(
            &area_of(recorded::READ_PUBLIC_KEY_RESPONSE),
            &key_template()
        ));
        assert!(matches_template(
            &area_of(recorded::READ_PUBLIC_SRK_RESPONSE),
            &srk_template()
        ));
        assert!(!matches_template(
            &area_of(recorded::READ_PUBLIC_SRK_RESPONSE),
            &key_template()
        ));
        assert!(!matches_template(
            &area_of(recorded::READ_PUBLIC_KEY_RESPONSE),
            &srk_template()
        ));
    }

    #[test]
    fn matches_template_needs_every_field_and_a_p256_point() {
        let area = area_of(recorded::READ_PUBLIC_KEY_RESPONSE);
        let template = key_template();
        let changed = |offset: usize, value: u8| {
            let mut area = area.clone();
            area[offset] = value;
            matches_template(&area, &template)
        };
        // nameAlg SHA-384, noDA added, scheme hash SHA-384, curve P-384.
        assert!(!changed(3, 0x0c));
        assert!(!changed(6, 0x04));
        assert!(!changed(15, 0x0c));
        assert!(!changed(17, 0x04));
        let mut longer = area.clone();
        longer.push(0);
        assert!(!matches_template(&longer, &template));
        // A 33-byte coordinate, and an empty one.
        let mut long_x = area[..20].to_vec();
        long_x.extend_from_slice(&33_u16.to_be_bytes());
        long_x.push(1);
        long_x.extend_from_slice(&area[22..]);
        assert!(!matches_template(&long_x, &template));
        let mut empty_x = area[..20].to_vec();
        empty_x.extend_from_slice(&[0, 0]);
        empty_x.extend_from_slice(&area[54..]);
        assert!(!matches_template(&empty_x, &template));
        assert!(!matches_template(&area[..40], &template));
        assert!(!matches_template(&[], &template));
        assert!(!matches_template(&area, &[]));
    }

    #[test]
    fn provisioning_commands_are_what_swtpm_accepted() {
        assert_eq!(
            create_primary_command(TPM_RH_OWNER, &srk_template()),
            hex(recorded::CREATE_PRIMARY_COMMAND)
        );
        assert_eq!(
            flush_context_command(0x8000_0000),
            hex(recorded::FLUSH_PRIMARY_COMMAND)
        );
        assert_eq!(
            create_command(0x8100_0001, &key_template()),
            hex(recorded::CREATE_COMMAND)
        );
        let created = parse_create(&hex(recorded::CREATE_RESPONSE)).unwrap();
        assert_eq!(
            load_command(0x8100_0001, &created.private, &created.public).unwrap(),
            hex(recorded::LOAD_COMMAND)
        );
        assert_eq!(
            evict_control_command(0x8000_0001, 0x8100_0100),
            hex(recorded::EVICT_CONTROL_COMMAND)
        );
        assert_eq!(
            flush_context_command(0x8000_0001),
            hex(recorded::FLUSH_KEY_COMMAND)
        );
        // A part longer than a TPM2B can carry is refused, not truncated.
        assert!(matches!(
            load_command(0x8100_0001, &vec![0; 0x1_0000], &created.public),
            Err(TpmError::Malformed(_))
        ));
    }

    #[test]
    fn the_primary_is_the_srk_and_the_created_key_is_the_one_persisted() {
        let srk = parse_read_public_area(&hex(recorded::READ_PUBLIC_SRK_RESPONSE)).unwrap();
        let primary = parse_create_primary(&hex(recorded::CREATE_PRIMARY_RESPONSE)).unwrap();
        assert_eq!(primary.loaded.handle, 0x8000_0000);
        // Same seed and template, so the same key and name as tpm2-tools' SRK.
        assert_eq!(primary.area, srk.area);
        assert_eq!(primary.loaded.name, srk.name);
        assert_eq!(primary.loaded.name, sha256_name(&primary.area));

        let created = parse_create(&hex(recorded::CREATE_RESPONSE)).unwrap();
        assert_eq!(created.private.len(), 0x7e);
        assert!(matches_template(&created.public, &key_template()));
        let loaded = parse_load(&hex(recorded::LOAD_RESPONSE)).unwrap();
        assert_eq!(loaded.handle, 0x8000_0001);
        assert_eq!(loaded.name, sha256_name(&created.public));
        let persisted = parse_read_public_area(&hex(recorded::READ_PUBLIC_KEY_RESPONSE)).unwrap();
        assert_eq!(persisted.area, created.public);
        assert_eq!(persisted.name, loaded.name);

        parse_evict_control(&hex(recorded::EVICT_CONTROL_RESPONSE)).unwrap();
        parse_flush_context(&hex(recorded::FLUSH_CONTEXT_RESPONSE)).unwrap();
        // The recorded self-test signature verifies with the persisted key.
        let key = parse_public_area(&persisted.area).unwrap();
        let signature = parse_sign(&hex(recorded::SIGN_SELF_TEST_RESPONSE)).unwrap();
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key.point)
            .verify(super::super::SELF_TEST_MESSAGE, &signature)
            .unwrap();
        assert_eq!(
            sign_command(0x8100_0100, &sha256(super::super::SELF_TEST_MESSAGE)),
            hex(recorded::SIGN_SELF_TEST_COMMAND)
        );
    }

    #[test]
    fn provisioning_error_codes_are_reported() {
        let code = |result: Result<(), TpmError>| match result {
            Err(TpmError::Response(code)) => code.code(),
            other => panic!("expected a response code, got {other:?}"),
        };
        assert_eq!(
            code(parse_evict_control(&hex(
                recorded::EVICT_CONTROL_DEFINED_RESPONSE
            ))),
            0x14c
        );
        assert_eq!(
            code(parse_flush_context(&hex(recorded::FLUSH_UNKNOWN_RESPONSE))),
            0x1cb
        );
        assert_eq!(
            code(parse_create(&hex(recorded::RETRY_RESPONSE)).map(|_| ())),
            0x922
        );
        assert_eq!(
            code(parse_read_public_area(&hex(recorded::READ_PUBLIC_EMPTY_RESPONSE)).map(|_| ())),
            0x18b
        );
    }

    // The endorsement key and the NV commands.

    #[test]
    fn the_ek_template_is_the_tcg_profile_l2_template() {
        let zeros = "00".repeat(32);
        assert_eq!(
            ek_template(),
            hex(&format!(
                "0023_000b_000300b2\
                 _0020_837197674484b3f81a90cc8d46a5d724fd52d76e06520b64f2a1da1b331469aa\
                 _0006_0080_0043_0010_0003_0010_0020_{zeros}_0020_{zeros}"
            ))
        );
        assert_eq!(ek_template().len(), 122);
    }

    #[test]
    fn nv_commands_are_marshalled_as_tpm2_part_3_specifies() {
        assert_eq!(
            nv_read_public_command(0x01c0_000a),
            hex("8001_0000000e_00000169_01c0000a")
        );
        assert_eq!(
            nv_read_command(TPM_RH_OWNER, 0x01c0_000a, 512, 0x0200),
            hex(
                "8002_00000023_0000014e_40000001_01c0000a_00000009_40000009_0000_00_0000_0200_0200"
            )
        );
    }

    #[test]
    fn the_ek_commands_are_what_swtpm_accepted_and_give_tpm2_tools_ek() {
        assert_eq!(
            create_primary_command(TPM_RH_ENDORSEMENT, &ek_template()),
            hex(recorded::EK_CREATE_PRIMARY_COMMAND)
        );
        assert_eq!(
            read_public_command(0x8000_0000),
            hex(recorded::EK_READ_PUBLIC_COMMAND)
        );
        let primary = parse_create_primary(&hex(recorded::EK_CREATE_PRIMARY_RESPONSE)).unwrap();
        assert_eq!(primary.loaded.handle, 0x8000_0000);
        assert_eq!(primary.loaded.name, sha256_name(&primary.area));
        let read = parse_read_public_area(&hex(recorded::EK_READ_PUBLIC_RESPONSE)).unwrap();
        assert_eq!(read.area, primary.area);
        assert_eq!(read.name, primary.loaded.name);
        let ek = parse_ek_area(&primary.area).unwrap();
        assert_eq!(ek.spki_der(), hex(recorded::EK_TPM2_TOOLS_SPKI));
        // A restricted decryption key: never usable as the gateway key.
        assert!(parse_public_area(&primary.area).is_err());
        assert!(!matches_template(&primary.area, &key_template()));
        // The SRK and the gateway key are not EKs.
        for other in [
            recorded::READ_PUBLIC_SRK_RESPONSE,
            recorded::READ_PUBLIC_KEY_RESPONSE,
        ] {
            assert!(matches!(
                parse_ek_area(&area_of(other)),
                Err(TpmError::UnsuitableKey(_))
            ));
        }
    }

    #[test]
    fn an_ek_area_is_refused_unless_it_is_the_template_and_a_p256_point() {
        let area = parse_create_primary(&hex(recorded::EK_CREATE_PRIMARY_RESPONSE))
            .unwrap()
            .area;
        // The template up to unique (54 bytes), then x and y with their sizes.
        assert_eq!(area.len(), 54 + 2 + 32 + 2 + 32);
        for offset in 0..54 {
            let mut changed = area.clone();
            changed[offset] ^= 0x01;
            assert!(
                matches!(parse_ek_area(&changed), Err(TpmError::UnsuitableKey(_))),
                "offset {offset}"
            );
        }
        let mut longer = area.clone();
        longer.push(0);
        assert!(matches!(
            parse_ek_area(&longer),
            Err(TpmError::Malformed(_))
        ));
        for length in 54..area.len() {
            assert!(parse_ek_area(&area[..length]).is_err(), "length {length}");
        }
        // A 33-byte x coordinate.
        let mut long_x = area[..54].to_vec();
        long_x.extend_from_slice(&33_u16.to_be_bytes());
        long_x.push(1);
        long_x.extend_from_slice(&area[56..]);
        assert!(matches!(
            parse_ek_area(&long_x),
            Err(TpmError::Malformed(_))
        ));
        // A 31-byte y coordinate is right-aligned.
        let mut short_y = area[..88].to_vec();
        short_y.extend_from_slice(&31_u16.to_be_bytes());
        short_y.extend_from_slice(&area[91..]);
        let point = parse_ek_area(&short_y).unwrap().point;
        assert_eq!(point[33], 0);
        assert_eq!(point[34..], area[91..]);
    }

    #[test]
    fn nv_responses_parse_as_recorded() {
        let certificate_index =
            parse_nv_read_public(&hex(recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE)).unwrap();
        assert_eq!(
            certificate_index,
            NvPublic {
                index: 0x01c0_000a,
                attributes: 0x6207_2001,
                data_size: 798,
            }
        );
        let written_and_readable =
            nv_attribute::WRITTEN | nv_attribute::OWNERREAD | nv_attribute::AUTHREAD;
        assert_eq!(
            certificate_index.attributes & written_and_readable,
            written_and_readable
        );
        assert_eq!(certificate_index.attributes & nv_attribute::TYPE_MASK, 0);
        assert_eq!(
            parse_nv_read_public(&hex(recorded::NV_READ_PUBLIC_TEMPLATE_RESPONSE))
                .unwrap()
                .index,
            0x01c0_000c
        );
        assert!(matches!(
            parse_nv_read_public(&hex(recorded::NV_READ_PUBLIC_UNDEFINED_RESPONSE)),
            Err(TpmError::Response(code)) if code.code() == 0x18b
        ));

        assert_eq!(
            nv_read_command(TPM_RH_OWNER, 0x01c0_000a, 512, 0),
            hex(recorded::NV_READ_FIRST_COMMAND)
        );
        assert_eq!(
            nv_read_command(TPM_RH_OWNER, 0x01c0_000a, 286, 512),
            hex(recorded::NV_READ_SECOND_COMMAND)
        );
        let mut certificate = parse_nv_read(&hex(recorded::NV_READ_FIRST_RESPONSE), 512).unwrap();
        certificate.extend_from_slice(
            &parse_nv_read(&hex(recorded::NV_READ_SECOND_RESPONSE), 286).unwrap(),
        );
        assert_eq!(certificate, hex(recorded::EK_CERTIFICATE));
        // Data of another length than asked for.
        for asked in [511, 513, 0] {
            assert!(matches!(
                parse_nv_read(&hex(recorded::NV_READ_FIRST_RESPONSE), asked),
                Err(TpmError::Malformed(detail)) if detail.contains("asked for")
            ));
        }
    }

    /// Every NV parser applied to `response`; true when all of them failed.
    fn every_nv_parser_fails(response: &[u8]) -> bool {
        parse_nv_read_public(response).is_err()
            && parse_nv_read(response, 512).is_err()
            && parse_nv_read(response, 286).is_err()
    }

    const NV_RESPONSES: [&str; 4] = [
        recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE,
        recorded::NV_READ_PUBLIC_TEMPLATE_RESPONSE,
        recorded::NV_READ_FIRST_RESPONSE,
        recorded::NV_READ_SECOND_RESPONSE,
    ];

    #[test]
    fn every_truncation_and_extension_of_an_nv_or_ek_response_is_an_error() {
        for fixture in NV_RESPONSES {
            let response = hex(fixture);
            for length in 0..response.len() {
                let truncated = response.get(..length).unwrap();
                assert!(every_nv_parser_fails(truncated), "length {length}");
                if length > HEADER_BYTES {
                    assert!(
                        every_nv_parser_fails(&reframe(truncated.to_vec())),
                        "reframed length {length}"
                    );
                }
            }
            let mut longer = response.clone();
            longer.push(0);
            assert!(every_nv_parser_fails(&reframe(longer)));
        }
        for fixture in [
            recorded::EK_CREATE_PRIMARY_RESPONSE,
            recorded::EK_READ_PUBLIC_RESPONSE,
        ] {
            let response = hex(fixture);
            for length in 0..response.len() {
                let truncated = response.get(..length).unwrap();
                assert!(every_provisioning_parser_fails(truncated));
                if length > HEADER_BYTES {
                    assert!(every_provisioning_parser_fails(&reframe(
                        truncated.to_vec()
                    )));
                }
            }
        }
    }

    #[test]
    fn inflated_sizes_in_nv_responses_never_read_past_the_buffer() {
        for (fixture, size_fields) in [
            // TPM2B_NV_PUBLIC, authPolicy, nvName.
            (
                recorded::NV_READ_PUBLIC_CERTIFICATE_RESPONSE,
                &[10_usize, 22, 26][..],
            ),
            // parameterSize (its low half), the data's size.
            (recorded::NV_READ_FIRST_RESPONSE, &[12, 14][..]),
            (recorded::NV_READ_SECOND_RESPONSE, &[12, 14][..]),
        ] {
            let response = hex(fixture);
            for offset in HEADER_BYTES..response.len() - 1 {
                let mut changed = response.clone();
                changed[offset] = 0xff;
                changed[offset + 1] = 0xff;
                // Must not panic; changed data may still parse.
                let all_fail = every_nv_parser_fails(&changed);
                if size_fields.contains(&offset) {
                    assert!(all_fail, "offset {offset}");
                }
            }
        }
    }

    #[test]
    fn nv_garbage_is_rejected_without_panicking() {
        let mut state: u64 = 0x5851_f42d_4c95_7f2d;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let length = usize::try_from(next() % 700).unwrap();
            let mut garbage: Vec<u8> = (0..length).map(|_| next().to_be_bytes()[0]).collect();
            if next() % 2 == 0 && garbage.len() >= HEADER_BYTES {
                garbage[0] = 0x80;
                garbage[1] = if next() % 2 == 0 { 0x01 } else { 0x02 };
                garbage[6..10].fill(0);
                garbage = reframe(garbage);
            }
            let _ = every_nv_parser_fails(&garbage);
            let _ = parse_ek_area(&garbage);
        }
    }

    /// Every provisioning parser applied to `response`; true when all of them failed.
    fn every_provisioning_parser_fails(response: &[u8]) -> bool {
        parse_read_public_area(response).is_err()
            && parse_create_primary(response).is_err()
            && parse_create(response).is_err()
            && parse_load(response).is_err()
            && parse_evict_control(response).is_err()
            && parse_flush_context(response).is_err()
    }

    #[test]
    fn every_truncation_and_extension_of_a_provisioning_response_is_an_error() {
        for fixture in [
            recorded::READ_PUBLIC_SRK_RESPONSE,
            recorded::CREATE_PRIMARY_RESPONSE,
            recorded::CREATE_RESPONSE,
            recorded::LOAD_RESPONSE,
            recorded::EVICT_CONTROL_RESPONSE,
            recorded::FLUSH_CONTEXT_RESPONSE,
        ] {
            let response = hex(fixture);
            for length in 0..response.len() {
                let truncated = response.get(..length).unwrap();
                assert!(
                    every_provisioning_parser_fails(truncated),
                    "length {length}"
                );
                // With the size field corrected, so the parsers look inside. The bare header is
                // a valid TPM2_FlushContext response.
                if length > HEADER_BYTES {
                    assert!(
                        every_provisioning_parser_fails(&reframe(truncated.to_vec())),
                        "reframed length {length}"
                    );
                }
            }
            let mut longer = response.clone();
            longer.push(0);
            assert!(every_provisioning_parser_fails(&reframe(longer)));
        }
    }

    #[test]
    fn inflated_sizes_in_provisioning_responses_never_read_past_the_buffer() {
        for fixture in [
            recorded::CREATE_PRIMARY_RESPONSE,
            recorded::CREATE_RESPONSE,
            recorded::LOAD_RESPONSE,
            recorded::EVICT_CONTROL_RESPONSE,
        ] {
            let response = hex(fixture);
            for offset in HEADER_BYTES..response.len() - 1 {
                let mut changed = response.clone();
                changed[offset] = 0xff;
                changed[offset + 1] = 0xff;
                // Must not panic; the result depends on what was changed.
                let _ = every_provisioning_parser_fails(&changed);
            }
        }
        // A handle that is not transient.
        let mut persistent = hex(recorded::LOAD_RESPONSE);
        persistent[10] = 0x81;
        assert!(matches!(
            parse_load(&persistent),
            Err(TpmError::Malformed(detail)) if detail.contains("transient")
        ));
        // A creation ticket with another tag.
        let mut response = hex(recorded::CREATE_RESPONSE);
        // From the end: the session (5 bytes), the ticket's digest (2 + 64), hierarchy (4), tag.
        let ticket = response.len() - 5 - (2 + 0x40) - 4 - 2;
        assert_eq!(response[ticket..ticket + 2], [0x80, 0x21]);
        response[ticket + 1] = 0x22;
        assert!(matches!(
            parse_create(&response),
            Err(TpmError::Malformed(detail)) if detail.contains("ticket")
        ));
    }

    #[test]
    fn provisioning_garbage_is_rejected_without_panicking() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let length = usize::try_from(next() % 500).unwrap();
            let mut garbage: Vec<u8> = (0..length).map(|_| next().to_be_bytes()[0]).collect();
            if next() % 2 == 0 && garbage.len() >= HEADER_BYTES {
                garbage[0] = 0x80;
                garbage[1] = if next() % 2 == 0 { 0x01 } else { 0x02 };
                garbage[6..10].fill(0);
                if next() % 2 == 0 && garbage.len() >= 14 {
                    garbage[10] = 0x80;
                }
                garbage = reframe(garbage);
            }
            let _ = every_provisioning_parser_fails(&garbage);
        }
    }
}
