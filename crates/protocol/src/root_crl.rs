//! The root CRL over HTTPS (Laysi/trustiot-v2#831; services/certs/README.md, 「端點」 and
//! 「根 CRL」).
//!
//! The certs service hands gateways the root CRL it adopted, the offline root CA's CRL of revoked
//! issuing CAs, at `GET` [`ROOT_CRL_PATH`] on the gateway host. The request carries no cookie and
//! no credential: the CRL is public and signed by the root, so the gateway checks it itself
//! (`crates/root-crl`, the same checks the service applied before adopting it) and keeps it only
//! when its CRL number is larger than the one it holds (Laysi/trustiot-v2#563).
//!
//! The answers, which services/certs/README.md (「端點」) fixes as the contract:
//!
//! - 200 with one PEM block `X509 CRL` in lines of 64 characters, of at most
//!   `root_crl::MAX_BYTES`, `Content-Type` [`ROOT_CRL_CONTENT_TYPE`];
//! - 404 while the service has no root CRL to hand out (none issued yet, or none checked against
//!   the root it loaded);
//! - 405 for every method but `GET` and `HEAD`.
//!
//! Every answer is `Cache-Control: no-store`. A gateway that gets anything but 200 keeps the root
//! CRL it has and asks again at its next poll.

/// The path on the gateway host (the chart's `ingress.publicPaths.rootCrl`, an Exact match).
pub const ROOT_CRL_PATH: &str = "/certs/root-crl";

/// The media type of the 200 answer: PEM, which the gateway agent loads as it is.
pub const ROOT_CRL_CONTENT_TYPE: &str = "application/x-pem-file";
