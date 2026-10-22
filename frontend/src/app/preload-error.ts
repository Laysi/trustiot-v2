/**
 * After a deployment, a tab that is still open can ask for a code chunk the new version no longer
 * has; Vite reports that as `vite:preloadError`. Reloading picks up the new index.html. A
 * timestamp in sessionStorage keeps this to one reload a minute, so a chunk that is missing for
 * another reason cannot make the tab reload in a loop.
 */
export const RELOAD_KEY = "trustiot.preload-reload-at";
export const RELOAD_INTERVAL_MS = 60_000;

export interface PreloadErrorDeps {
  target: Pick<Window, "addEventListener">;
  storage: Pick<Storage, "getItem" | "setItem"> | undefined;
  reload(): void;
  now(): number;
}

/** Whether to reload now; records the reload when it says yes. */
export function shouldReload(storage: PreloadErrorDeps["storage"], now: number): boolean {
  try {
    const last = Number(storage?.getItem(RELOAD_KEY) ?? "");
    if (Number.isFinite(last) && last > 0 && now - last < RELOAD_INTERVAL_MS) {
      return false;
    }
    storage?.setItem(RELOAD_KEY, String(now));
  } catch {
    // Without storage there is no guard: do not reload, the error page offers a reload button.
    return false;
  }
  return storage !== undefined;
}

export function reloadOnPreloadError(deps: PreloadErrorDeps): void {
  deps.target.addEventListener("vite:preloadError", (event) => {
    if (shouldReload(deps.storage, deps.now())) {
      event.preventDefault();
      deps.reload();
    }
  });
}
