//! Pipeline self-test for `.github/workflows/release.yml` (docs/architecture/ci-release.md).
//!
//! The release pipeline publishes this binary as `ghcr.io/laysi/trustiot-v2/release-selftest`
//! exactly the way it publishes a service: cross-compiled for aarch64 with Zig as the C
//! compiler and linker, packed into the shared Rust image, signed with cosign and attested with
//! its SBOM. It proves the whole pipeline on every release, including before any service image
//! exists. It is not a service, and nothing deploys it.
//!
//! Running it prints one JSON line about the build and exits with status 0 only when the C
//! object from `src/target.c`, compiled by the target's C compiler, agrees with the Rust code
//! about the pointer width.

use std::process::ExitCode;

use serde_json::{Value, json};

/// The release version stamped by the release pipeline; other builds use the crate version.
const VERSION: &str = match option_env!("TRUSTIOT_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

// The declaration is the only unsafe code: it asserts the C signature, which matches
// `src/target.c`. The function takes no arguments, touches no memory and returns a constant, so
// it is declared `safe` and calls need no `unsafe` block.
#[allow(
    unsafe_code,
    reason = "linking a C object is what this self-test checks; the declaration matches src/target.c"
)]
unsafe extern "C" {
    /// Pointer width in bits as the C compiler saw the target; defined in `src/target.c`.
    safe fn release_selftest_c_pointer_bits() -> u32;
}

/// Builds the report for the pointer width that the C object returned, and whether it matches
/// the Rust code's.
fn report(c_pointer_bits: u32) -> (Value, bool) {
    let matches = c_pointer_bits == usize::BITS;
    let report = json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": VERSION,
        "arch": std::env::consts::ARCH,
        "os": std::env::consts::OS,
        "rust_pointer_bits": usize::BITS,
        "c_pointer_bits": c_pointer_bits,
        "c_object": if matches { "ok" } else { "mismatch" },
    });
    (report, matches)
}

#[allow(
    clippy::print_stdout,
    reason = "the JSON line on stdout is this program's whole output"
)]
fn main() -> ExitCode {
    let (report, matches) = report(release_selftest_c_pointer_bits());
    println!("{report}");
    if matches {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_c_object_agrees_with_rust_on_this_target() {
        assert_eq!(release_selftest_c_pointer_bits(), usize::BITS);
    }

    #[test]
    fn a_matching_c_object_reports_ok() {
        let (report, matches) = report(usize::BITS);
        assert!(matches);
        assert_eq!(report["c_object"], "ok");
        assert_eq!(report["name"], "release-selftest");
        assert_eq!(report["arch"], std::env::consts::ARCH);
        assert_eq!(report["c_pointer_bits"], usize::BITS);
    }

    #[test]
    fn a_c_object_for_another_target_reports_a_mismatch() {
        let (report, matches) = report(usize::BITS / 2);
        assert!(!matches);
        assert_eq!(report["c_object"], "mismatch");
    }

    #[test]
    fn the_version_is_the_release_version_or_the_crate_version() {
        let expected = option_env!("TRUSTIOT_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        let (report, _) = report(usize::BITS);
        assert_eq!(report["version"], expected);
    }
}
