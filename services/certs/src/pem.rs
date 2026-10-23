//! PEM text of public objects (certificates, CRLs, certificate signing requests): base64 in
//! 64-character lines between the RFC 7468 labels. Never used for private keys.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The label of a certificate.
pub const CERTIFICATE: &str = "CERTIFICATE";

/// The label of a CRL.
pub const CRL: &str = "X509 CRL";

/// The label of a certificate signing request (PKCS #10).
pub const CERTIFICATE_REQUEST: &str = "CERTIFICATE REQUEST";

/// `der` as one PEM block labelled `label`, ending in a newline.
#[must_use]
pub fn encode(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut text = String::with_capacity(body.len() + body.len() / 64 + 2 * label.len() + 40);
    text.push_str("-----BEGIN ");
    text.push_str(label);
    text.push_str("-----\n");
    let mut rest = body.as_str();
    while !rest.is_empty() {
        let (line, after) = rest.split_at(rest.len().min(64));
        text.push_str(line);
        text.push('\n');
        rest = after;
    }
    text.push_str("-----END ");
    text.push_str(label);
    text.push_str("-----\n");
    text
}

/// The DER contents of every PEM block labelled `label` in `text`, in order.
///
/// # Errors
///
/// When the text has a block that does not decode, a block with another label, or other text
/// between blocks.
pub fn decode_all(text: &str, label: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut blocks = Vec::new();
    let mut rest = text.as_bytes();
    loop {
        let start = rest.iter().position(|byte| !byte.is_ascii_whitespace());
        let Some(start) = start else {
            return Ok(blocks);
        };
        let (after, block) = x509_parser::pem::parse_x509_pem(&rest[start..])
            .map_err(|_| format!("block {} is not PEM", blocks.len() + 1))?;
        if block.label != label {
            return Err(format!(
                "block {} is a {}, not a {label}",
                blocks.len() + 1,
                block.label
            ));
        }
        blocks.push(block.contents);
        rest = after;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_in_64_character_lines_and_decodes_back() {
        let der: Vec<u8> = (0..=255).collect();
        let text = encode(CERTIFICATE, &der);
        assert!(text.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(text.ends_with("-----END CERTIFICATE-----\n"));
        assert!(text.lines().all(|line| line.len() <= 64));
        let twice = format!("{text}\n{text}");
        assert_eq!(
            decode_all(&twice, CERTIFICATE).expect("decode"),
            [der.clone(), der]
        );
        assert!(decode_all(&text, CRL).is_err());
        assert!(decode_all("garbage", CRL).is_err());
        assert!(decode_all("  \n", CRL).expect("empty").is_empty());
    }
}
