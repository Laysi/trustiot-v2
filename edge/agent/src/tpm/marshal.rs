//! The two TPM 2.0 commands the agent sends (TPM 2.0 Part 3: TPM2_ReadPublic, section 12.4, and
//! TPM2_Sign, section 20.2), with the structures of Part 2 they carry, and strict parsing of their
//! responses.
//!
//! A response is untrusted input: every length is checked against what is left of the buffer,
//! every structure must end exactly where its size says, and nothing here can panic on any input.

use super::TpmError;
use super::rc::ResponseCode;

/// The largest response accepted (the kernel's `TPM_BUFSIZE`, also the usual
/// `MAX_RESPONSE_SIZE` of a TPM). The responses parsed here are under 200 bytes.
pub(crate) const MAX_RESPONSE_BYTES: usize = 4096;

const TPM_ST_NO_SESSIONS: u16 = 0x8001;
const TPM_ST_SESSIONS: u16 = 0x8002;
const TPM_ST_HASHCHECK: u16 = 0x8024;

const TPM_CC_SIGN: u32 = 0x0000_015d;
const TPM_CC_READ_PUBLIC: u32 = 0x0000_0173;

/// The password authorization session, here with an empty password.
const TPM_RS_PW: u32 = 0x4000_0009;
const TPM_RH_NULL: u32 = 0x4000_0007;

const TPM_ALG_SHA256: u16 = 0x000b;
const TPM_ALG_NULL: u16 = 0x0010;
const TPM_ALG_ECDSA: u16 = 0x0018;
const TPM_ALG_ECC: u16 = 0x0023;
const TPM_ECC_NIST_P256: u16 = 0x0003;

/// `TPMA_OBJECT` bits (Part 2, section 8.3).
mod attribute {
    pub(super) const FIXED_TPM: u32 = 1 << 1;
    pub(super) const FIXED_PARENT: u32 = 1 << 4;
    pub(super) const SENSITIVE_DATA_ORIGIN: u32 = 1 << 5;
    pub(super) const USER_WITH_AUTH: u32 = 1 << 6;
    pub(super) const RESTRICTED: u32 = 1 << 16;
    pub(super) const DECRYPT: u32 = 1 << 17;
    pub(super) const SIGN: u32 = 1 << 18;
}

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
    // Authorization area: one TPMS_AUTH_COMMAND, 9 bytes.
    command.u32(9);
    command.u32(TPM_RS_PW);
    command.u16(0); // nonceCaller: empty
    command.u8(0); // sessionAttributes: none
    command.u16(0); // hmac: the empty password
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
    let mut reader = response_parameters(response, TPM_ST_NO_SESSIONS)?;
    let public = reader.sized("TPM2B_PUBLIC")?;
    let key = parse_public_area(public)?;
    // name and qualifiedName (TPM2B_NAME): checked for shape only.
    reader.sized("name")?;
    reader.sized("qualifiedName")?;
    reader.finish("TPM2_ReadPublic response")?;
    Ok(key)
}

/// TPMT_PUBLIC of an ECC key (Part 2, section 12.2.4).
fn parse_public_area(bytes: &[u8]) -> Result<PublicKey, TpmError> {
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

    fn finish(mut self) -> Vec<u8> {
        // The two commands here are under 100 bytes.
        let size = u32::try_from(self.0.len()).unwrap_or(u32::MAX);
        if let Some(field) = self.0.get_mut(2..6) {
            field.copy_from_slice(&size.to_be_bytes());
        }
        self.0
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
}
