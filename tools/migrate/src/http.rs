//! The HTTP client of the Kubernetes API and ClickHouse calls: rustls with ring, trusting only
//! the CA the deployment names (no system store, no public roots), like the Postgres connections
//! (checklist M3).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;

/// Every request gives up after this; the callers retry or fail the run.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why the client could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot read the CA file {path}: {message}")]
    CaFile { path: String, message: String },
    #[error("the CA file {path} holds no certificate the TLS library accepts")]
    NoCertificate { path: String },
    #[error("cannot build the HTTP client: {0}")]
    Build(String),
}

/// A client for `http://` URLs. Its TLS settings trust nothing, so an `https://` URL fails
/// instead of reaching for the platform's roots.
///
/// # Errors
///
/// When reqwest cannot build it.
pub fn plain() -> Result<reqwest::Client, ClientError> {
    build(RootCertStore::empty(), false)
}

fn build(roots: RootCertStore, https_only: bool) -> Result<reqwest::Client, ClientError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| ClientError::Build(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .tls_backend_preconfigured(tls)
        .https_only(https_only)
        .build()
        .map_err(|error| ClientError::Build(error.to_string()))
}

/// A client whose `https://` connections trust only the certificates in `ca_file` (PEM, one or
/// more). The file is read once, when the client is built.
///
/// # Errors
///
/// When the file cannot be read or holds no usable certificate.
pub fn trusting(ca_file: &Path) -> Result<reqwest::Client, ClientError> {
    let path = ca_file.display().to_string();
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_file)
        .and_then(Iterator::collect)
        .map_err(|error| ClientError::CaFile {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(certificates);
    if added == 0 {
        return Err(ClientError::NoCertificate { path });
    }
    build(roots, true)
}
