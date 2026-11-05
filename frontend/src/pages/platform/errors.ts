import type { TenantStateSnapshot } from "../../api/pending/platform.ts";

/**
 * Thrown by a dialog's submit when the tenant's state changed after the dialog opened (decision
 * Q3: until the server takes `expected_state_version`, the page reads the state again before it
 * writes). Nothing was written; `current` is what the tenant is now.
 */
export class StaleStateError extends Error {
  readonly current: TenantStateSnapshot;

  constructor(current: TenantStateSnapshot) {
    super("the tenant's state changed after the dialog opened");
    this.name = "StaleStateError";
    this.current = current;
  }
}
