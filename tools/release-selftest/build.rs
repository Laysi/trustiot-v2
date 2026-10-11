//! Compiles `src/target.c` with the C compiler for the target. A release build for
//! aarch64 uses Zig through cargo-zigbuild, so a broken C cross toolchain fails here, in
//! the release pipeline's own self-test, before a service with C dependencies (for example
//! a TLS library) needs it.

use std::env;

fn main() {
    println!("cargo::rerun-if-changed=src/target.c");
    let target = env::var("TARGET").expect("Cargo sets TARGET for build scripts");
    let host = env::var("HOST").expect("Cargo sets HOST for build scripts");
    if target != host {
        // `cargo zigbuild` sets CC_<target> to Zig's C compiler for the target. Without a
        // compiler for the target, cc would try the host's and the link would fail later
        // with a less obvious error, so say what is missing instead.
        let underscored = format!("CC_{}", target.replace('-', "_"));
        let dashed = format!("CC_{target}");
        for var in [underscored.as_str(), dashed.as_str(), "TARGET_CC"] {
            println!("cargo::rerun-if-env-changed={var}");
        }
        assert!(
            [underscored.as_str(), dashed.as_str(), "TARGET_CC"]
                .iter()
                .any(|var| env::var_os(var).is_some()),
            "cross-compiling for {target} needs a C compiler for the target: build with \
             `cargo zigbuild` (scripts/release/build.ts does), or set {underscored}"
        );
    }
    cc::Build::new()
        .file("src/target.c")
        .warnings_into_errors(true)
        .compile("release_selftest_target");
}
