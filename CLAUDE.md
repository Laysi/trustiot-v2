# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository state

Bootstrapped on 2026-10-09. No application code, build tooling, or test runner exists yet. When the first toolchain lands, pin it in `.prototools` and add its build, lint, and test commands (including how to run a single test) to this file.

Architecture decisions, rebuild scope (items A1–M8), service split, repository layout, and milestones (MS0–MS6) live in `docs/`; start at `docs/README.md`. `docs/` is the source of truth: when a decision changes, update `docs/` in the same change. In `docs/`, a bare `#N` refers to the legacy repo `Laysi/trustiot`; always write legacy references as `Laysi/trustiot#N` in issues and PRs here.

## Work tracking: GitHub Issues + Project board

Work is tracked as real issues in `Laysi/trustiot-v2` on the org Project **TrustIoT v2** (`Laysi`, project #12). Follow the `github-loop-engineering` skill for every issue and board transition. Resolve field and option IDs at runtime; never hard-code them.

- **Status:** `Triage`, `Todo`, `In Progress`, `Blocked`, `Done`. Issues created by an agent start in `Triage`; only a human promotes them to `Todo`.
- **Priority (board field):** `P0` incident or blocker, `P1` current focus, `P2` backlog. The board field is the source of truth. An issue with Priority set carries exactly one matching `priority:P0|P1|P2` label; an untriaged issue has Priority unset and no `priority:*` label.
- **Labels:** every issue carries at least one `area:*` label (or `documentation` for docs-only work). Add an `area:<component>` label, whose description names the component path, when a new top-level component is created. Other workflow labels: `blocked`, `tech-debt`, `security`.
- **Issue types:** `Feature` for new capabilities, `Bug` for incorrect behavior or vulnerabilities, `Task` for everything else.
- **Issue bodies** are written in Traditional Chinese and must be cold-start readable. Use the headings from `.github/ISSUE_TEMPLATE/`: 背景 (Why), 範圍 (What), 現狀 (State), 驗收條件 (Acceptance Criteria), 指標 (Pointers); bugs add 重現 (Reproduction). Use absolute dates only.
- **Blocked work:** `blocked` label, a `Blocked by` section in the body, and Status `Blocked`.
- **Assignment** is decided when work starts, not at creation.
- **Closing:** if the Project's auto-close workflow is enabled, setting Status to `Done` closes the issue, so close the issue through the completion ritual instead of moving the card first.
