# Image for every Rust binary that release.yml publishes (docs/architecture/ci-release.md).
# scripts/release/build.ts writes the binary name in place of @BIN@ and builds this file for
# linux/arm64 with a build context that holds only the cross-compiled, statically linked
# binary (and, for an image with a [static] project, that project's build output in www/).
# No instruction runs a program, so the build needs no emulator.
#
# distroless/static has no shell and no package manager, only CA certificates, tzdata and
# /etc/passwd. When updating the base image, change the tag and the digest together.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:e2e927ec666bae08560abb3c55d0659eceabb657f56b6782ab500a9fc7f555e3

COPY --chmod=0555 @BIN@ /usr/local/bin/@BIN@

# An image whose image.toml has a [static] table gets `COPY www/ /srv/www/` in place of the
# next line: the build output, owned by root, files 0644 and directories 0755 (set in the
# context by scripts/release/static-build.ts). Every other image loses the line.
# @STATIC@

# A numeric UID lets Kubernetes enforce runAsNonRoot. OpenShift may run the image with another
# UID; that works because the binary is world-executable and the root filesystem is read-only.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/@BIN@"]
