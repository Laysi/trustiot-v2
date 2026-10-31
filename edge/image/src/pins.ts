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

/** Packages added to the image, at exact versions (from the snapshot above). */
export const IMAGE_PACKAGES = { "tpm-udev": "0.6+nmu1", "tpm2-tools": "5.7-1+b1" } as const;

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
 * Timestamp for everything the build writes with a chosen time (FAT entries, the data file
 * system): the source release date, 2026-10-06T00:00:00Z.
 */
export const SOURCE_DATE_EPOCH = Date.UTC(2026, 9, 6) / 1000;

/** Sector size of the image's partition table. */
export const SECTOR = 512;

/** The data partition (third entry in the table, between boot and root on the disk). */
export const DATA = {
  number: 3,
  sizeSectors: 4194304, // 2 GiB
  label: "trustiot-data",
  /** Fixed so that two builds produce the same file system. */
  uuid: "6f3c1b2e-5a4d-4c1e-9b7a-2d8e0f4a9c31",
  hashSeed: "0b9e4d2a-7c1f-4e3b-8a6d-5f2c9e1b7a40",
  mountPoint: "/var/lib/trustiot-agent",
  mountOptions: "defaults,noatime,nodev,nosuid,noexec,nofail",
} as const;

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
  // The check that loads the rules again when they vanish while the agent runs, and the timer
  // the agent's unit requires, which starts it every 10 s (Laysi/trustiot-v2#878).
  { repo: "trustiot-agent-egress-check.service", image: "/usr/lib/systemd/system/trustiot-agent-egress-check.service" },
  { repo: "trustiot-agent-egress-check.timer", image: "/usr/lib/systemd/system/trustiot-agent-egress-check.timer" },
] as const;
