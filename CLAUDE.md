# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

Bootstrapped on 2026-10-09; the Cargo workspace and toolchain pins landed on 2026-10-10. `tools/repo-check` enforces repository invariants as part of `cargo test`. Pin every new runtime or dev tool in `.prototools` and add its commands to this file.

Architecture decisions, rebuild scope (items A1–M8), service split, repository layout, and milestones (MS0–MS6) live in `docs/`; start at `docs/README.md`. `docs/` is the source of truth: when a decision changes, update `docs/` in the same change. In `docs/`, a bare `#N` refers to the legacy repo `Laysi/trustiot`; always write legacy references as `Laysi/trustiot#N` in issues and PRs here.

## Build, lint, and test

`.prototools` pins Rust, Bun, actionlint, cargo-deny, gitleaks, Helm, k3d, kubeconform, and kubectl; tools without a built-in proto plugin get a schema plugin in `.proto/plugins/` that verifies the release checksum. proto installs Rust through rustup and does not shim cargo, so rustup picks the toolchain from `rust-toolchain.toml`: change `rust` in `.prototools` and `channel` in `rust-toolchain.toml` together (`cargo test -p repo-check` fails when they differ). Run every command from the repository root.

- Install the pinned tools: `proto install`
- Build: `cargo build --workspace`
- Format: `cargo fmt --all` (check only: `cargo fmt --all --check`)
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace`
- Single test: `cargo test -p <crate> <test-name-filter>`, for example `cargo test -p repo-check toolchain_pins_agree`
- Local dev environment (podman, no Kubernetes; details in `scripts/README.md`): `bun scripts/dev.ts up` | `seed` | `status` | `env` | `down` | `purge`. Script tests: `bun test scripts` (add `TRUSTIOT_DEV_E2E=1` and the file `scripts/tests/e2e.test.ts` to run the podman end-to-end test).
- Helm chart (`deploy/chart`): `bun test deploy/chart` runs `helm lint`, kubeconform on both values files, the chart's own rules, and the README field check; a single check is `bun test deploy/chart -t "<test-name-filter>"`. The individual `helm lint`/`helm template` commands are in `deploy/chart/README.md`.
- SaaS host installer (`deploy/host`): `bun install --cwd deploy/host --frozen-lockfile`, then `bun run --cwd deploy/host typecheck`, `bun run --cwd deploy/host test` (simulated host; a single test is `-t "<test-name-filter>"`) and `bun run --cwd deploy/host build` (the x86_64 and aarch64 executables in `deploy/host/dist/`). Running it on a host is in `deploy/host/README.md`.
- Nightly workflow (`.github/workflows/nightly.yml`, `scripts/nightly/`): CI deploys the chart with both values files to a k3d cluster and smoke-tests it; it runs only in CI, because workstations do not run Kubernetes. Its unit tests need no cluster: `bun test scripts/nightly`. Renovate (`renovate.json5`) updates dependencies; a tool pinned in `.prototools` with a schema plugin needs a `# renovate:` comment above its pin (`bun test scripts/nightly` checks it). Details: `docs/architecture/ci-release.md`.
- Ingress e2e (needs a cluster with Istio ambient; never run Kubernetes locally): `E2E_NAMESPACE=<ns> E2E_IDENTITY_IMAGE=<image> bun test --timeout 600000 ./deploy/chart/tests/e2e/ingress.e2e.ts`; prerequisites and the other variables are in `deploy/chart/tests/e2e/README.md`. Its tunnel recovery (`raw-client.ts`, `tunnel-recovery.ts`) has cluster-free unit tests that `bun test deploy/chart` runs.
- Container-backed tests: `crates/service-kit` starts throwaway Postgres and PID 1 containers with podman or docker (or uses `TEST_DATABASE_URL` for Postgres). Without them those tests skip locally; with `CI` set they fail instead.
- `crates/service-kit` without its default `contract` feature, which the `--workspace` commands never build: `cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings` and `cargo test -p service-kit --no-default-features`
- API spec (`api/openapi.json`, generated from the code, never edited by hand): regenerate with `cargo run -p openapi` after changing any API type and commit the result; `cargo run -p openapi -- --check` only verifies it. `cargo test --workspace` and CI fail when the committed file differs from what the code generates. Each service exposes `pub fn openapi()` and registers it in `tools/openapi` (`tools/openapi/README.md`).
- Dependency licenses, advisories, and sources (rules in `deny.toml`): `cargo deny --locked check`
- Secret scan, as CI runs it: `gitleaks git --redact --log-opts="--full-history --diff-filter=tuxdb --diff-merges=first-parent HEAD" .`
- Workflow policy (job `permissions`, SHA-pinned actions): `bun scripts/ci/check-workflows.ts`
- Workflow syntax and expression check with the pinned actionlint, which rejects what GitHub cannot parse (for example `runner.temp` in job-level `env`): `bun scripts/ci/lint-workflows.ts` (run `proto install actionlint` first; the shellcheck integration is deliberately off, see `docs/architecture/ci-release.md`)
- Bun scripts in `scripts/` (the dev environment and the CI checks): `bun install --cwd scripts --frozen-lockfile` installs the type checker, then `bun run --cwd scripts typecheck` checks every `.ts` file there and under `deploy/chart/tests/` (the ingress e2e included, which needs a cluster to run but not to type-check); `bun run --cwd scripts test` runs the same test files as `bun test scripts`, and a single test is `bun run --cwd scripts test -t "<test-name-filter>"`
- Gateway crates (`crates/protocol` and every member under `edge/`) must build for aarch64: `rustup target add aarch64-unknown-linux-gnu` once, then `cargo build --locked --target aarch64-unknown-linux-gnu -p <crate>`

`.github/workflows/ci.yml` runs these checks on every pull request and every push to main; its jobs and the required-checks list are in `docs/architecture/ci-release.md`. Every job must list its `permissions`, every action must be pinned to a full commit SHA with the tag in a trailing comment, and local actions belong under `.github/actions/`.

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
