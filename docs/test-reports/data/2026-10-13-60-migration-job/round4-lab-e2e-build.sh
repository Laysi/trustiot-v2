#!/usr/bin/env bash
S=<scratch>
cd <worktree>
bash $S/rcheck.sh 60e2e 'set -eo pipefail; ZB=$(proto bin zig --reporter text); PATH=$(dirname $ZB):$(dirname $(proto bin cargo-zigbuild --reporter text)):$PATH; export PATH; zig version; rustup target add aarch64-unknown-linux-musl 2>&1 | tail -1; date -u +%FT%TZ; cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p service-kit --bin example-service -p migrate --bin migrate 2>&1 | tail -8; date -u +%FT%TZ; R=$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release; ls -l $R/example-service $R/migrate; sha256sum $R/example-service $R/migrate'
