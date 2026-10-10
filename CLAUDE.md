# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

Bootstrapped on 2026-10-09; the Cargo workspace and toolchain pins landed on 2026-10-10. `tools/repo-check` enforces repository invariants as part of `cargo test`. Pin every new runtime or dev tool in `.prototools` and add its commands to this file.

Architecture decisions, rebuild scope (items A1–M8), service split, repository layout, and milestones (MS0–MS6) live in `docs/`; start at `docs/README.md`. `docs/` is the source of truth: when a decision changes, update `docs/` in the same change. In `docs/`, a bare `#N` refers to the legacy repo `Laysi/trustiot`; always write legacy references as `Laysi/trustiot#N` in issues and PRs here.

## Build, lint, and test

`.prototools` pins Rust, Bun, Helm, kubeconform, and the release tools (zig, cargo-zigbuild, cargo-auditable, syft, cosign); tools without a built-in proto plugin get a schema plugin in `.proto/plugins/` that verifies the release checksum. proto installs Rust through rustup and does not shim cargo, so rustup picks the toolchain from `rust-toolchain.toml`: change `rust` in `.prototools` and `channel` in `rust-toolchain.toml` together (`cargo test -p repo-check` fails when they differ). Run every command from the repository root.

- Install the pinned tools: `proto install`
- Build: `cargo build --workspace`
- Format: `cargo fmt --all` (check only: `cargo fmt --all --check`)
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace`
- Single test: `cargo test -p <crate> <test-name-filter>`, for example `cargo test -p repo-check toolchain_pins_agree`
- Local dev environment (podman, no Kubernetes; details in `scripts/README.md`): `bun scripts/dev.ts up` | `seed` | `status` | `env` | `down` | `purge`. Script tests: `bun test scripts` (add `TRUSTIOT_DEV_E2E=1` and the file `scripts/tests/e2e.test.ts` to run the podman end-to-end test).
- Helm chart (`deploy/chart`): `bun test deploy/chart` runs `helm lint`, kubeconform on both values files, the chart's own rules, and the README field check; a single check is `bun test deploy/chart -t "<test-name-filter>"`. The individual `helm lint`/`helm template` commands are in `deploy/chart/README.md`.
- Container-backed tests: `crates/service-kit` starts throwaway Postgres and PID 1 containers with podman or docker (or uses `TEST_DATABASE_URL` for Postgres). Without them those tests skip locally; with `CI` set they fail instead.
- Release scripts (`scripts/release`, run by `.github/workflows/release.yml`): `bun test scripts/release`; a single test is `bun test scripts/release -t "<test-name-filter>"`. Building the arm64 images locally and running the whole publish and verify flow against a throwaway registry are described in `scripts/README.md`.
- Images: each directory under `deploy/images/` is a published arm64 image (`image.toml` for a Rust binary, or its own `Containerfile`); see `deploy/images/README.md`. Base images must be pinned by digest.

Every directory under `crates/`, `services/`, `edge/`, `factory/`, and `tools/` is a workspace member, so it needs a `Cargo.toml` that sets `edition.workspace = true` and `[lints] workspace = true`; list non-Rust directories in the root `exclude`. Declare third-party dependencies once in the root `[workspace.dependencies]`. Clippy fails on every lint in `[workspace.lints]`; suppress one only with `#[allow(lint, reason = "...")]`. Save test evidence under `docs/test-reports/` as its README describes.

## Work tracking: GitHub Issues + Project board

Work is tracked as real issues in `Laysi/trustiot-v2` on the org Project **TrustIoT v2** (`Laysi`, project #12). Follow the `github-loop-engineering` skill for every issue and board transition. Resolve field and option IDs at runtime; never hard-code them.

- **Status:** `Triage`, `Todo`, `In Progress`, `Blocked`, `Done`. Agents triage by best judgment, without waiting for a human: accepted work moves to `Todo`, and rejected work is closed as not planned. `Todo` includes accepted work that still waits on other issues in this repo. Those waits are native blocked-by dependencies, and the board's `Ready` view (filter `status:Todo -is:blocked`) lists the work that can start now.
- **Priority (board field):** `P0` incident or blocker, `P1` current focus (the milestone in progress), `P2` backlog. Set it when the issue is created, by best judgment; agents set it too and do not wait for human triage. The board field is the source of truth, and the issue carries exactly one matching `priority:P0|P1|P2` label.
- **Labels:** every issue carries at least one `area:*` label (or `documentation` for docs-only work). Add an `area:<component>` label, whose description names the component path, when a new top-level component is created. Other workflow labels: `blocked`, `tech-debt`, `security`.
- **Issue types:** `Feature` for new capabilities, `Bug` for incorrect behavior or vulnerabilities, `Task` for everything else.
- **Issue bodies** are written in Traditional Chinese and must be cold-start readable. Use the headings from `.github/ISSUE_TEMPLATE/`: 背景 (Why), 範圍 (What), 現狀 (State), 驗收條件 (Acceptance Criteria), 指標 (Pointers); bugs add 重現 (Reproduction). Use absolute dates only. Don't copy board fields such as Status or Priority into the body. The templates come from the `github-loop-engineering` skill (`zh-TW`); update them through the skill, not by hand.
- **Parallel work:** each milestone is a root issue whose sub-issues are epics and then leaves, and blocked-by dependencies connect leaves only. To work through a parent issue, follow the skill's fan-out: the board filter `parent-issue:Laysi/trustiot-v2#N status:Todo -is:blocked` lists its children that can start now.
- **Blocked work:** a blocker outside the work graph, such as missing hardware, a pending decision, or a customer, gets the `blocked` label, a `Blocked by` section in the body, and Status `Blocked`. A wait on another issue in this repo is a native blocked-by dependency instead.
- **CLI fallback:** use the GitHub MCP server for every GitHub read and write. The user has authorized the skill's `gh api` fallback for two things the server can't do yet, and nothing else: native blocked-by dependencies (list, add, remove) and creating milestones.
- **Assignment** is decided when work starts, not at creation.
- **Closing:** if the Project's auto-close workflow is enabled, setting Status to `Done` closes the issue, so close the issue through the completion ritual instead of moving the card first.
