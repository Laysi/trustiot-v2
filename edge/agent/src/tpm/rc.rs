//! TPM 2.0 response codes (TPM 2.0 Part 2, section 6.6), decoded into readable errors.

use std::fmt;

/// A response code other than `TPM_RC_SUCCESS`, as the TPM (or the kernel's resource manager)
/// returned it. `Display` names it, for example `TPM_RC_HANDLE (0x0000018b, handle 1)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResponseCode(pub u32);

/// Bit 7: a format-one code (error number in bits 0-5, with a handle, session or parameter).
const RC_FMT1: u32 = 0x080;
/// Bit 8 of a format-zero code: a TPM 2.0 code (clear: a TPM 1.2 code).
const RC_VER1: u32 = 0x100;
/// Bit 10 of a format-zero code: defined by the TPM vendor.
const RC_VENDOR: u32 = 0x400;
/// Bit 11 of a format-zero code: a warning rather than an error.
const RC_WARN: u32 = 0x800;
/// Bit 6 of a format-one code: the error is about the parameter numbered in bits 8-11.
const RC_P: u32 = 0x040;

impl ResponseCode {
    /// The raw 32-bit code.
    #[must_use]
    pub const fn code(self) -> u32 {
        self.0
    }

    /// The TCG name, or `None` for a code this table does not know.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        let code = self.0;
        // Bits 16-23 carry the layer of a software stack's code; the kernel and the TPM use 0.
        if code >> 16 != 0 {
            return None;
        }
        if code & RC_FMT1 != 0 {
            return format_one_name(code & 0x3f);
        }
        if code & RC_VER1 == 0 || code & RC_VENDOR != 0 {
            return None;
        }
        if code & RC_WARN != 0 {
            warning_name(code & 0x7f)
        } else {
            error_name(code & 0x7f)
        }
    }

    /// For a format-one code, what the error is about: a handle, a session or a parameter,
    /// numbered from 1 in the order of the command.
    #[must_use]
    pub fn subject(self) -> Option<String> {
        let code = self.0;
        if code >> 16 != 0 || code & RC_FMT1 == 0 {
            return None;
        }
        let number = (code >> 8) & 0xf;
        Some(if code & RC_P != 0 {
            format!("parameter {number}")
        } else if number & 0x8 != 0 {
            format!("session {}", number & 0x7)
        } else if number != 0 {
            format!("handle {number}")
        } else {
            return None;
        })
    }
}

impl fmt::Display for ResponseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.name().unwrap_or("unknown response code");
        write!(f, "{name} ({:#010x}", self.0)?;
        if let Some(subject) = self.subject() {
            write!(f, ", {subject}")?;
        }
        if self.0 >> 16 == 0 && self.0 & RC_FMT1 == 0 && self.0 & RC_VER1 == 0 {
            write!(f, ", TPM 1.2 format")?;
        }
        write!(f, ")")
    }
}

/// Format-one error numbers (`TPM_RC_FMT1 + n`).
fn format_one_name(number: u32) -> Option<&'static str> {
    Some(match number {
        0x01 => "TPM_RC_ASYMMETRIC",
        0x02 => "TPM_RC_ATTRIBUTES",
        0x03 => "TPM_RC_HASH",
        0x04 => "TPM_RC_VALUE",
        0x05 => "TPM_RC_HIERARCHY",
        0x07 => "TPM_RC_KEY_SIZE",
        0x08 => "TPM_RC_MGF",
        0x09 => "TPM_RC_MODE",
        0x0a => "TPM_RC_TYPE",
        0x0b => "TPM_RC_HANDLE",
        0x0c => "TPM_RC_KDF",
        0x0d => "TPM_RC_RANGE",
        0x0e => "TPM_RC_AUTH_FAIL",
        0x0f => "TPM_RC_NONCE",
        0x10 => "TPM_RC_PP",
        0x12 => "TPM_RC_SCHEME",
        0x15 => "TPM_RC_SIZE",
        0x16 => "TPM_RC_SYMMETRIC",
        0x17 => "TPM_RC_TAG",
        0x18 => "TPM_RC_SELECTOR",
        0x1a => "TPM_RC_INSUFFICIENT",
        0x1b => "TPM_RC_SIGNATURE",
        0x1c => "TPM_RC_KEY",
        0x1d => "TPM_RC_POLICY_FAIL",
        0x1f => "TPM_RC_INTEGRITY",
        0x20 => "TPM_RC_TICKET",
        0x21 => "TPM_RC_RESERVED_BITS",
        0x22 => "TPM_RC_BAD_AUTH",
        0x23 => "TPM_RC_EXPIRED",
        0x24 => "TPM_RC_POLICY_CC",
        0x25 => "TPM_RC_BINDING",
        0x26 => "TPM_RC_CURVE",
        0x27 => "TPM_RC_ECC_POINT",
        _ => return None,
    })
}

/// Format-zero error numbers (`TPM_RC_VER1 + n`).
fn error_name(number: u32) -> Option<&'static str> {
    Some(match number {
        0x00 => "TPM_RC_INITIALIZE",
        0x01 => "TPM_RC_FAILURE",
        0x03 => "TPM_RC_SEQUENCE",
        0x0b => "TPM_RC_PRIVATE",
        0x19 => "TPM_RC_HMAC",
        0x20 => "TPM_RC_DISABLED",
        0x21 => "TPM_RC_EXCLUSIVE",
        0x24 => "TPM_RC_AUTH_TYPE",
        0x25 => "TPM_RC_AUTH_MISSING",
        0x26 => "TPM_RC_POLICY",
        0x27 => "TPM_RC_PCR",
        0x28 => "TPM_RC_PCR_CHANGED",
        0x2d => "TPM_RC_UPGRADE",
        0x2e => "TPM_RC_TOO_MANY_CONTEXTS",
        0x2f => "TPM_RC_AUTH_UNAVAILABLE",
        0x30 => "TPM_RC_REBOOT",
        0x31 => "TPM_RC_UNBALANCED",
        0x42 => "TPM_RC_COMMAND_SIZE",
        0x43 => "TPM_RC_COMMAND_CODE",
        0x44 => "TPM_RC_AUTHSIZE",
        0x45 => "TPM_RC_AUTH_CONTEXT",
        0x46 => "TPM_RC_NV_RANGE",
        0x47 => "TPM_RC_NV_SIZE",
        0x48 => "TPM_RC_NV_LOCKED",
        0x49 => "TPM_RC_NV_AUTHORIZATION",
        0x4a => "TPM_RC_NV_UNINITIALIZED",
        0x4b => "TPM_RC_NV_SPACE",
        0x4c => "TPM_RC_NV_DEFINED",
        0x50 => "TPM_RC_BAD_CONTEXT",
        0x51 => "TPM_RC_CPHASH",
        0x52 => "TPM_RC_PARENT",
        0x53 => "TPM_RC_NEEDS_TEST",
        0x54 => "TPM_RC_NO_RESULT",
        0x55 => "TPM_RC_SENSITIVE",
        _ => return None,
    })
}

/// Format-zero warning numbers (`TPM_RC_WARN + n`).
fn warning_name(number: u32) -> Option<&'static str> {
    Some(match number {
        0x01 => "TPM_RC_CONTEXT_GAP",
        0x02 => "TPM_RC_OBJECT_MEMORY",
        0x03 => "TPM_RC_SESSION_MEMORY",
        0x04 => "TPM_RC_MEMORY",
        0x05 => "TPM_RC_SESSION_HANDLES",
        0x06 => "TPM_RC_OBJECT_HANDLES",
        0x07 => "TPM_RC_LOCALITY",
        0x08 => "TPM_RC_YIELDED",
        0x09 => "TPM_RC_CANCELED",
        0x0a => "TPM_RC_TESTING",
        0x10..=0x16 => "TPM_RC_REFERENCE_H",
        0x18..=0x1e => "TPM_RC_REFERENCE_S",
        0x20 => "TPM_RC_NV_RATE",
        0x21 => "TPM_RC_LOCKOUT",
        0x22 => "TPM_RC_RETRY",
        0x23 => "TPM_RC_NV_UNAVAILABLE",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::ResponseCode;

    #[test]
    fn format_one_codes_name_the_error_and_its_subject() {
        // What swtpm answered for TPM2_ReadPublic of an empty persistent handle.
        assert_eq!(
            ResponseCode(0x18b).to_string(),
            "TPM_RC_HANDLE (0x0000018b, handle 1)"
        );
        // A wrong password on the first session.
        assert_eq!(
            ResponseCode(0x98e).to_string(),
            "TPM_RC_AUTH_FAIL (0x0000098e, session 1)"
        );
        assert_eq!(
            ResponseCode(0x2c4).to_string(),
            "TPM_RC_VALUE (0x000002c4, parameter 2)"
        );
        // Format one without a handle, session or parameter number.
        assert_eq!(
            ResponseCode(0x0a2).to_string(),
            "TPM_RC_BAD_AUTH (0x000000a2)"
        );
    }

    #[test]
    fn format_zero_errors_and_warnings_are_told_apart() {
        assert_eq!(
            ResponseCode(0x101).to_string(),
            "TPM_RC_FAILURE (0x00000101)"
        );
        assert_eq!(
            ResponseCode(0x143).to_string(),
            "TPM_RC_COMMAND_CODE (0x00000143)"
        );
        assert_eq!(
            ResponseCode(0x921).to_string(),
            "TPM_RC_LOCKOUT (0x00000921)"
        );
        assert_eq!(ResponseCode(0x922).to_string(), "TPM_RC_RETRY (0x00000922)");
        assert_eq!(
            ResponseCode(0x902).to_string(),
            "TPM_RC_OBJECT_MEMORY (0x00000902)"
        );
    }

    #[test]
    fn unknown_vendor_layered_and_tpm12_codes_stay_readable() {
        assert_eq!(
            ResponseCode(0x17f).to_string(),
            "unknown response code (0x0000017f)"
        );
        assert_eq!(
            ResponseCode(0x501).to_string(),
            "unknown response code (0x00000501)"
        );
        assert_eq!(
            ResponseCode(0x000b_0101).to_string(),
            "unknown response code (0x000b0101)"
        );
        assert_eq!(
            ResponseCode(0x01e).to_string(),
            "unknown response code (0x0000001e, TPM 1.2 format)"
        );
        assert_eq!(ResponseCode(0x18b).code(), 0x18b);
    }
}
