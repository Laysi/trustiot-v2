// Checks that each release tool's linux asset and checksum file exist for x86_64 and aarch64,
// using the URL patterns of .proto/plugins/<tool>.toml and the versions in .prototools.
const v = { zig: "0.16.0", zb: "0.23.4", ca: "0.7.6", syft: "1.52.0", cosign: "3.1.3", helm: "4.3.0" };
const gh = "https://github.com";
const urls: string[] = [];
for (const arch of ["x86_64", "aarch64"]) {
  const a2 = arch === "x86_64" ? "amd64" : "arm64";
  urls.push(
    `https://ziglang.org/download/${v.zig}/zig-${arch}-linux-${v.zig}.tar.xz`,
    `https://ziglang.org/download/${v.zig}/zig-${arch}-linux-${v.zig}.tar.xz.minisig`,
    `${gh}/rust-cross/cargo-zigbuild/releases/download/v${v.zb}/cargo-zigbuild-${arch}-unknown-linux-gnu.tar.xz`,
    `${gh}/rust-cross/cargo-zigbuild/releases/download/v${v.zb}/cargo-zigbuild-${arch}-unknown-linux-gnu.tar.xz.sha256`,
    `${gh}/rust-secure-code/cargo-auditable/releases/download/v${v.ca}/cargo-auditable-${arch}-unknown-linux-gnu.tar.xz`,
    `${gh}/rust-secure-code/cargo-auditable/releases/download/v${v.ca}/cargo-auditable-${arch}-unknown-linux-gnu.tar.xz.sha256`,
    `${gh}/anchore/syft/releases/download/v${v.syft}/syft_${v.syft}_linux_${a2}.tar.gz`,
    `${gh}/sigstore/cosign/releases/download/v${v.cosign}/cosign-linux-${a2}`,
    `https://get.helm.sh/helm-v${v.helm}-linux-${a2}.tar.gz`,
    `https://get.helm.sh/helm-v${v.helm}-linux-${a2}.tar.gz.sha256sum`,
  );
}
urls.push(
  `${gh}/anchore/syft/releases/download/v${v.syft}/syft_${v.syft}_checksums.txt`,
  `${gh}/sigstore/cosign/releases/download/v${v.cosign}/cosign_checksums.txt`,
);
for (const url of urls) {
  const res = await fetch(url, { method: "HEAD", redirect: "follow" });
  console.log(res.status, res.headers.get("content-length") ?? "-", url.replace(/^https:\/\/[^/]+/, (m) => m));
}
for (const [url, needles] of [
  [`${gh}/anchore/syft/releases/download/v${v.syft}/syft_${v.syft}_checksums.txt`, [`syft_${v.syft}_linux_arm64.tar.gz`, `syft_${v.syft}_linux_amd64.tar.gz`]],
  [`${gh}/sigstore/cosign/releases/download/v${v.cosign}/cosign_checksums.txt`, ["cosign-linux-arm64", "cosign-linux-amd64"]],
] as const) {
  const text = await (await fetch(url)).text();
  for (const n of needles) {
    const line = text.split("\n").find((l) => l.trim().endsWith(` ${n}`) || l.trim().endsWith(`*${n}`));
    console.log("checksum line:", line ?? `MISSING ${n}`);
  }
}
