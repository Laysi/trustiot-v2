import { type ReactNode, useId, useSyncExternalStore } from "react";

import { Button } from "../components/ui/button.tsx";
import { NativeSelect } from "../components/ui/native-select.tsx";
import type { MemberRole } from "../session/session.ts";
import { PREVIEW_SEEDS, type PreviewSeed } from "./contract.ts";
import { FAILURE_KINDS, type FailureKind, type PreviewControls, SLOW_DELAYS_MS } from "./controls.ts";
import { type PreviewSettings, ROLES } from "./settings.ts";

/**
 * The UI preview's toolbar, above the app in a root of its own (ui-preview.html): the session's
 * role and platform administrator, the seed, a failure for the next request (or every request)
 * and a slow delay for every request. It is a development tool, so its labels are English and not translated. Classes
 * only, no `style` attribute: the preview keeps the SPA's CSP rules.
 */

const FAILURE_LABELS: Record<FailureKind, string> = {
  unavailable: "503 unavailable",
  internal: "500 internal",
  permission_denied: "403 permission denied",
  network: "network error",
};

function Field({ label, children }: { label: string; children: (id: string) => ReactNode }) {
  const id = useId();
  return (
    <div className="flex items-center gap-2">
      <label htmlFor={id} className="whitespace-nowrap text-muted-foreground">
        {label}
      </label>
      <div className="w-36">{children(id)}</div>
    </div>
  );
}

export function Toolbar({
  settings,
  controls,
  fakes,
  onSettings,
}: {
  settings: PreviewSettings;
  controls: PreviewControls;
  /** The names of the fakes in use, in the order loaded. */
  fakes: readonly string[];
  /** Saves new settings and starts the preview again with them. */
  onSettings: (settings: PreviewSettings) => void;
}) {
  const state = useSyncExternalStore(controls.subscribe, controls.get);
  // The slow delay changes without a reload, so the settings saved next carry its current value.
  const current: PreviewSettings = { ...settings, slowMs: state.slowMs };
  const failureId = useId();
  return (
    <section
      aria-label="UI preview"
      className="flex flex-wrap items-center gap-x-4 gap-y-2 border-b border-dashed bg-muted px-4 py-2 text-sm"
    >
      <strong className="font-semibold" title={fakes.join(", ")}>
        UI preview · fake data ({fakes.length} fakes)
      </strong>
      <Field label="Role">
        {(id) => (
          <NativeSelect
            id={id}
            value={settings.role}
            onChange={(event) => onSettings({ ...current, role: event.target.value as MemberRole })}
          >
            {ROLES.map((role) => (
              <option key={role} value={role}>
                {role}
              </option>
            ))}
          </NativeSelect>
        )}
      </Field>
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          className="size-4 accent-primary"
          checked={settings.platformAdmin}
          onChange={(event) => onSettings({ ...current, platformAdmin: event.target.checked })}
        />
        Platform admin
      </label>
      <Field label="Data">
        {(id) => (
          <NativeSelect
            id={id}
            value={settings.seed}
            onChange={(event) => onSettings({ ...current, seed: event.target.value as PreviewSeed })}
          >
            {PREVIEW_SEEDS.map((seed) => (
              <option key={seed} value={seed}>
                {seed}
              </option>
            ))}
          </NativeSelect>
        )}
      </Field>
      <Field label="Slow">
        {(id) => (
          <NativeSelect
            id={id}
            value={String(state.slowMs)}
            onChange={(event) => {
              const slowMs = Number(event.target.value);
              controls.set({ slowMs });
              onSettings({ ...current, slowMs });
            }}
          >
            {SLOW_DELAYS_MS.map((delay) => (
              <option key={delay} value={delay}>
                {delay === 0 ? "off" : `${delay / 1000} s per request`}
              </option>
            ))}
          </NativeSelect>
        )}
      </Field>
      <div className="flex items-center gap-2">
        <label htmlFor={failureId} className="whitespace-nowrap text-muted-foreground">
          Fail next request
        </label>
        <div className="w-44">
          <NativeSelect
            id={failureId}
            value={state.failNext ?? ""}
            onChange={(event) =>
              controls.set({ failNext: event.target.value === "" ? null : (event.target.value as FailureKind) })
            }
          >
            <option value="">no</option>
            {FAILURE_KINDS.map((kind) => (
              <option key={kind} value={kind}>
                {FAILURE_LABELS[kind]}
              </option>
            ))}
          </NativeSelect>
        </div>
        <label className="flex items-center gap-2 whitespace-nowrap">
          <input
            type="checkbox"
            className="size-4 accent-primary"
            checked={state.keepFailing}
            onChange={(event) => controls.set({ keepFailing: event.target.checked })}
          />
          Every request
        </label>
        {state.failNext !== null ? (
          <Button variant="ghost" size="sm" onClick={() => controls.set({ failNext: null })}>
            Disarm
          </Button>
        ) : null}
      </div>
    </section>
  );
}
