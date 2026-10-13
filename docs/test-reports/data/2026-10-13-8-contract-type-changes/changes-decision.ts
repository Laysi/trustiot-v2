// Prints which ci.yml jobs scripts/ci/changes.ts runs for a change to each of these paths,
// one change at a time (Laysi/trustiot-v2#8, acceptance criterion 2). Run from the repository
// root: bun docs/test-reports/data/2026-10-13-8-contract-type-changes/changes-decision.ts
import { decide, PATH_JOBS } from "../../../../scripts/ci/changes.ts";

const changes = [
  "crates/contract/src/lib.rs",
  "crates/contract/src/identity.rs",
  "crates/contract/src/event.rs",
  "crates/contract/src/error.rs",
  "crates/contract/Cargo.toml",
  "crates/protocol/src/lib.rs",
  "crates/protocol/src/telemetry.rs",
  "crates/protocol/tests/golden/current/telemetry-ack-committed.json",
  "docs/README.md",
];

for (const path of changes) {
  const decision = decide([path]);
  console.log(`changed: ${path}`);
  for (const job of PATH_JOBS) {
    console.log(`  ${job}: ${decision[job].run ? "runs" : "skipped"} (${decision[job].reason})`);
  }
}
