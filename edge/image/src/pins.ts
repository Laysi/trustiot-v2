// Everything the gateway image is built from, pinned in one place. Changing a pin changes the
// image: update edge/image/README.md (section 「釘選」) in the same change.

/** The official Raspberry Pi OS Lite (64-bit) release the image starts from. */
export const SOURCE = {
  file: "2026-10-06-raspios-trixie-arm64-lite.img.xz",
  url: "https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/2026-10-06-raspios-trixie-arm64-lite.img.xz",
  /** Matches the .sha256 file published next to the image (checked 2026-10-31). */
  sha256: "483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843",
  xzBytes: 550466056,
  imageBytes: 3078619136,
} as const;

/** The partition table of the decompressed source image; the build refuses any other. */
export const SOURCE_TABLE = {
  labelId: "0x27b94958",
  partitions: [
    { number: 1, start: 16384, size: 1048576, type: "c" },
    { number: 2, start: 1064960, size: 4947968, type: "83" },
  ],
} as const;

/**
 * The container every build and check step runs in: Debian 13 (trixie), pinned by the digest of
 * its multi-architecture index, so the same pin serves arm64 and x86_64 hosts. Written as
 * name:tag@digest so Renovate reads it and updates the copy in README.md in the same pull request.
 */
export const TOOL_IMAGE =
  "docker.io/library/debian:trixie@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1";

/**
 * snapshot.debian.org timestamp for every package the build installs, in the tool container and
 * in the image: the same timestamp always yields the same package versions.
 */
export const DEBIAN_SNAPSHOT = "20261030T000000Z";

/**
 * Tools installed in the tool container, at exact versions from the snapshot above. ca-certificates
 * is fetched over plain HTTP (the container has no CA certificates yet): its exact version keeps a
 * replayed older, validly signed index from handing over an older package.
 */
export const TOOL_PACKAGES = {
  "ca-certificates": "20250419+deb13u1",
  dosfstools: "4.2-1.2",
  e2fsprogs: "1.47.2-3+b12",
  fdisk: "2.41.5-0+deb13u1",
  fuse2fs: "1.47.2-3+b12",
  mtools: "4.0.48-1",
  "xz-utils": "5.8.1-1+deb13u2",
} as const;

/**
 * Packages added to the image, at exact versions (from the snapshot above). systemd-cryptsetup
 * matches the image's own systemd version, so installing it upgrades nothing.
 */
export const IMAGE_PACKAGES = {
  "cryptsetup-bin": "2:2.7.5-2",
  "systemd-cryptsetup": "257.13-1~deb13u1",
  "tpm-udev": "0.6+nmu1",
  "tpm2-tools": "5.7-1+b1",
} as const;

/**
 * The boot test (README.md, section 「開機測試」) boots a copy of the image in QEMU with a software
 * TPM. Debian's arm64 kernel stands in for the gateway's (QEMU cannot run the Raspberry Pi kernel);
 * it does not build tpm_tis, the driver of QEMU's TPM device, so the test builds that one module
 * from the same kernel's source. Packages at exact versions from the snapshot above.
 */
export const BOOT_TEST = {
  kernel: "6.12.113+deb13-arm64",
  packages: {
    "linux-headers-6.12.113+deb13-arm64": "6.12.113-1",
    "linux-image-6.12.113+deb13-arm64": "6.12.113-1",
    "linux-source-6.12": "6.12.113-1",
    make: "4.4.1-2",
    "qemu-system-arm": "1:10.0.13+ds-0+deb13u1",
    swtpm: "0.7.1-1.5",
    "libtss2-tcti-swtpm0t64": "4.1.3-1.2",
    "tpm2-tools": "5.7-1+b1",
    "cryptsetup-bin": "2:2.7.5-2",
  },
} as const;

/**
 * cloud-init's input files on the source image's boot partition. The image masks every cloud-init
 * unit, so they would do nothing; the build removes them so that nobody edits them expecting an
 * effect.
 */
export const REMOVED_BOOT_FILES = ["meta-data", "network-config", "user-data"] as const;

/**
 * gitleaks findings the check accepts. Each names one file a package in the image ships, the
 * gitleaks rule, the owning package and the SHA-256 of the whole file: other content in that file,
 * another rule or a file the build writes itself still fails the check. Reviewed 2026-10-31: the
 * TPM 2.0 FAPI profiles of libtss2-fapi1t64 4.1.3-1.2, where the rule matches an `authPolicy`
 * value, the public digest of an NV index policy, not a secret.
 */
export const KNOWN_FINDINGS = [
  {
    path: "/etc/tpm2-tss/fapi-profiles/P_ECCP384SHA384.json",
    rule: "generic-api-key",
    package: "libtss2-fapi1t64",
    sha256: "b13a1027cd41dc4dcef1b7dc326021421cfd2789a453807e20393374aac0f428",
  },
  {
    path: "/etc/tpm2-tss/fapi-profiles/P_RSA3072SHA384.json",
    rule: "generic-api-key",
    package: "libtss2-fapi1t64",
    sha256: "7f0a1d5529f987faf93be4baae05bf02f2619dbda4a9499b664c9cef46318e85",
  },
] as const;

/**
 * Timestamp for everything the build writes with a chosen time (FAT entries): the source release
 * date, 2026-10-06T00:00:00Z.
 */
export const SOURCE_DATE_EPOCH = Date.UTC(2026, 9, 6) / 1000;

/** Sector size of the image's partition table. */
export const SECTOR = 512;

/**
 * The data partition (third entry in the table, between boot and root on the disk). The image
 * leaves it all zeros; the gateway encrypts it on its first boot (LUKS below).
 */
export const DATA = {
  number: 3,
  sizeSectors: 4194304, // 2 GiB
  mountPoint: "/var/lib/trustiot-agent",
  mountOptions: "defaults,noatime,nodev,nosuid,noexec,nofail",
} as const;

/**
 * The data partition's LUKS2 volume, made by files/data/trustiot-data-setup on the gateway's
 * first boot (README.md, section 「資料分割區加密」). That script and files/data/crypttab carry
 * the same values; tests/data.test.ts compares them with these.
 */
export const LUKS = {
  /** Fixed, so that /etc/crypttab in the image names the volume before it exists. Not a secret. */
  uuid: "9da7f680-5912-4d5c-bca6-d46c83e064f8",
  /** The device-mapper name: /dev/mapper/trustiot_data, systemd-cryptsetup@trustiot_data.service. */
  mapping: "trustiot_data",
  /** LUKS2 label until the setup finished, then the final one. */
  labelNew: "trustiot-data-new",
  label: "trustiot-data",
  /** ext4 label of the file system inside. */
  fsLabel: "trustiot-data",
  cipher: "aes-xts-plain64",
  keyBits: 512,
  sectorBytes: 4096,
} as const;

/**
 * Every file of files/data and where the image installs it, unchanged: the data partition's setup,
 * its unlock and mount, the agent's dependency on it, and the logs and swap kept off the root partition
 * (README.md, section 「資料分割區加密」).
 */
export const DATA_FILES = [
  { repo: "trustiot-data-setup", image: "/usr/libexec/trustiot/trustiot-data-setup", mode: 0o755 },
  { repo: "trustiot-data-setup.service", image: "/usr/lib/systemd/system/trustiot-data-setup.service", mode: 0o644 },
  {
    repo: "systemd-cryptsetup-trustiot-data.conf",
    image: "/usr/lib/systemd/system/systemd-cryptsetup@trustiot_data.service.d/trustiot-data.conf",
    mode: 0o644,
  },
  { repo: "trustiot-agent-data.conf", image: "/usr/lib/systemd/system/trustiot-agent.service.d/trustiot-data.conf", mode: 0o644 },
  { repo: "crypttab", image: "/etc/crypttab", mode: 0o644 },
  { repo: "journald.conf", image: "/etc/systemd/journald.conf.d/60-trustiot.conf", mode: 0o644 },
  { repo: "rpi-swap.conf", image: "/etc/rpi/swap.conf.d/60-trustiot.conf", mode: 0o644 },
] as const;

/** NTP servers systemd-timesyncd uses; the firewall (Laysi/trustiot-v2#96) allows only these. */
export const NTP_SERVERS = ["time.cloudflare.com"] as const;

/** The only login account: SSH public key only, no password. */
export const ADMIN_USER = "trustiot-admin";

/** The default account of the source image, removed by the build. */
export const SOURCE_USER = "pi";

/** Name of the TPM overlay in files/ and on the boot partition. */
export const TPM_OVERLAY = "tpm-slb9670-ce0";

/**
 * Every file of edge/agent/systemd and where the image installs it, unchanged. A new file there
 * (another unit the agent requires, for example) fails tests/agent-unit.test.ts until it is
 * listed here, so the image never ships an agent whose dependencies are missing.
 */
export const AGENT_SYSTEMD_FILES = [
  { repo: "trustiot-agent.service", image: "/usr/lib/systemd/system/trustiot-agent.service" },
  { repo: "trustiot-agent.sysusers", image: "/usr/lib/sysusers.d/trustiot-agent.conf" },
  // The agent account's egress rules, the unit that loads them before the agent starts and the
  // nftables.service drop-in that keeps them loaded (Laysi/trustiot-v2#813). The unit and the
  // drop-in name these paths, so they follow edge/agent/README.md, 「出口規則」.
  { repo: "trustiot-agent-egress.service", image: "/usr/lib/systemd/system/trustiot-agent-egress.service" },
  { repo: "trustiot-agent-egress.nft", image: "/usr/share/trustiot-agent/trustiot-agent-egress.nft" },
  { repo: "nftables-reload.conf", image: "/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf" },
] as const;
